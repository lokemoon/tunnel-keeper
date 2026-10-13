'use strict';

const { spawn } = require('node:child_process');
const creds = require('./creds');
const binaries = require('./binaries');
const ports = require('./ports');
const procs = require('./procs');
const owned = require('./owned');

const tunnels = new Map(); // id -> { proc, meta, log[], timer }
let seq = 0;
let notify = () => {};
let emit = () => {};

// Bounded on purpose: an endless retry loop against AWS is noise, and after three
// failures the cause is something the user has to fix.
const MAX_ATTEMPTS = 3;
const BACKOFF_MS = [2000, 5000, 10000];

// A session that opens and immediately dies is flapping, not recovered. Attempts
// only reset after it has held for this long, so a flapping tunnel still exhausts
// its retries instead of reconnecting forever.
const STABLE_MS = 10000;

function onChange(fn) {
  notify = fn;
}

// kind: 'dropped' | 'restored' | 'gaveup'
function onEvent(fn) {
  emit = fn;
}

// The plugin's chatter ends with boilerplate like "Exiting session with sessionId";
// the line before that is usually the actual cause.
const NOISE =
  /^(Exiting session|Starting session|Waiting for connections|Connection accepted|Port \d+ opened|SessionId:)/i;

// Only lines from the current launch count — see logMark in launch().
function lastMeaningfulLine(t) {
  for (let i = t.log.length - 1; i >= (t.logMark || 0); i--) {
    const line = t.log[i].trim();
    if (line && !NOISE.test(line)) return line.slice(0, 200);
  }
  return null;
}

// Retrying an authorisation failure just fails again three times; only the user can
// fix it, so say so once instead.
const UNRECOVERABLE = /ExpiredToken|expired|AccessDenied|not authorized|UnauthorizedOperation|InvalidClientTokenId/i;

function push(t, line) {
  const text = String(line).trimEnd();
  if (!text) return;
  t.log.push(text);
  if (t.log.length > 200) t.log.shift();
}

function get(id) {
  const t = tunnels.get(id);
  if (!t) throw new Error('That tunnel no longer exists.');
  return t;
}

function isLive(meta) {
  return meta.status === 'open' || meta.status === 'starting' || meta.status === 'reconnecting';
}

// A tunnel keeps its local port for as long as its card exists, whatever its status.
// A stopped or dropped tunnel is still holding a reservation the user can act on —
// Reconnect reuses that exact port — so handing the same port to a second tunnel would
// give two cards the same address and make Reconnect ambiguous. The reservation ends
// only when the card is dismissed.
function claimant(port, exceptId = null) {
  for (const t of tunnels.values()) {
    if (t.meta.localPort === port && t.meta.id !== exceptId) return t.meta;
  }
  return null;
}

const STATUS_WORD = {
  open: 'open',
  starting: 'starting',
  reconnecting: 'reconnecting',
  stopping: 'stopping',
  stopped: 'stopped',
  error: 'disconnected',
};

function claimMessage(port, meta) {
  const what = STATUS_WORD[meta.status] || meta.status;
  return (
    `Local port ${port} is already assigned to ${meta.instanceName} in Tunnels (${what}). ` +
    (isLive(meta)
      ? 'Stop that tunnel first, or use a different local port.'
      : 'Reconnect that tunnel to reuse it, or dismiss it to free the port.')
  );
}

function clearTimer(t) {
  if (t.timer) {
    clearTimeout(t.timer);
    t.timer = null;
  }
  if (t.stableTimer) {
    clearTimeout(t.stableTimer);
    t.stableTimer = null;
  }
  t.meta.retryAt = null;
}

function guardCredentials(t) {
  if (!creds.has()) throw new Error('Load credentials first.');
  if (creds.summary().expired) {
    throw new Error('Credentials have expired. Load a fresh set, then reconnect.');
  }
  // A tunnel marked stale belongs to credentials that are no longer loaded. Retrying
  // it under a different account, role or region would open a session to a different
  // place behind the same local port, which is worse than not reconnecting at all.
  if (t && t.meta.stale && creds.samePrincipal(t.meta.principal, creds.principal()) !== true) {
    throw new Error(staleMessage(t.meta.principal));
  }
}

function staleMessage(was) {
  return (
    `These credentials are for ${creds.describePrincipal(creds.principal())}, but this tunnel ` +
    `was opened with ${creds.describePrincipal(was)}. Dismiss it and start a new one.`
  );
}

// Thrown when the port is held by a process this app did not start. Carries the
// details the UI needs to ask the user before killing anything.
class NeedsConfirmation extends Error {
  constructor(conflict) {
    super(conflict.message);
    this.needsConfirmation = true;
    this.conflict = conflict;
  }
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

// Frees the local port so a same-port reconnect can proceed.
//   ours          -> cleared without asking; it is our own orphaned plugin
//   someone       -> refused until the caller passes allowForeignKill
//   not ours, and unkillable -> refused with the reason
// Returns null if the port was already free, or a description of what it stopped,
// so the caller can tell the user what happened rather than doing it silently.
async function clearPort(port, { allowForeignKill = false } = {}) {
  if (!(await ports.inUse(port))) return null;

  const c = await ports.conflict(port);
  if (!c.holders.length) {
    throw new Error(`${c.message} It could not be identified, so it was left alone.`);
  }

  // Never signal ourselves, whatever the registry or the user says.
  if (c.holders.some((h) => h.pid === process.pid || h.pid === process.ppid)) {
    throw new Error(`${c.message} That is Tunnel Keeper itself, so it will not be stopped.`);
  }

  if (!c.ours && !allowForeignKill) throw new NeedsConfirmation(c);

  if (!c.canTerminate) {
    throw new Error(`${c.message} It belongs to another user, so this app cannot stop it.`);
  }

  // Every listener must go, not just the first: a port shared between an IPv4 and an
  // IPv6 listener is only actually free once both are gone.
  for (const h of c.holders) {
    const res = await procs.terminate(h.pid);
    if (!res.ok) throw new Error(`Could not stop ${ports.nameOf(h)}: ${res.error}`);
    owned.forget(h.pid);
  }

  // The port can linger for a moment after the processes go.
  for (let i = 0; i < 10; i++) {
    if (!(await ports.inUse(port))) {
      return { label: c.label, pid: c.holders[0].pid, ours: c.ours, count: c.holders.length };
    }
    await wait(150);
  }
  throw new Error(`${c.label} exited but local port ${port} is still not free.`);
}

// The CLI's session-manager-plugin child is what binds the port, and it can outlive
// its parent. Recording it is what lets a later reconnect recognise the orphan as
// ours and clear it without bothering the user.
async function trackChildren(t, pid) {
  try {
    for (const child of await procs.descendants(pid)) {
      owned.record(child, t.meta.id, 'session-manager-plugin');
    }
  } catch {
    // Discovery is best-effort; failing it only means we prompt later.
  }
}

// Launches (or relaunches) the CLI for an existing record. The record's identity,
// local port and auto-reconnect toggle all survive a reconnect.
function launch(t) {
  if (!binaries.hasSessionManagerPlugin()) {
    throw new Error(
      'session-manager-plugin is not installed. AWS SSM tunnels require it — install it from AWS, then reopen this app.'
    );
  }

  const { instanceId, remotePort, localPort } = t.meta;
  const args = [
    'ssm', 'start-session',
    '--target', instanceId,
    '--document-name', 'AWS-StartPortForwardingSession',
    '--parameters', `portNumber=${remotePort},localPortNumber=${localPort}`,
  ];

  const proc = spawn(binaries.awsOrThrow(), args, {
    env: creds.env(),
    stdio: ['ignore', 'pipe', 'pipe'],
    // Its own process group, so stopping the tunnel can take the whole group down
    // and not leave session-manager-plugin orphaned on the port. Windows has no
    // process groups here; taskkill /T handles the tree instead.
    detached: process.platform !== 'win32',
    windowsHide: true,
  });
  t.proc = proc;
  t.pgid = process.platform !== 'win32' ? proc.pid : null;

  owned.record(proc.pid, t.meta.id, 'aws');
  // The plugin appears shortly after the CLI starts.
  setTimeout(() => trackChildren(t, proc.pid), 800);
  setTimeout(() => trackChildren(t, proc.pid), 2500);
  t.meta.status = 'starting';
  t.meta.stopping = false;
  t.meta.dropped = false;
  t.meta.askedForeignKill = false;
  t.meta.startedAt = Date.now();

  // Everything below belongs to the session about to start, not to the record that
  // outlives it. sessionId especially: it is only captured while empty, so keeping
  // the previous session's id here makes health.js check a live session against a
  // dead id, declare a perfectly healthy tunnel dead every minute, and reconnect
  // forever — which is why only records that had already reconnected once flapped.
  t.meta.sessionId = null;
  t.meta.reason = null;
  // The log also survives reconnects, so only lines produced by this launch may be
  // used to explain this launch's death. Otherwise an ExpiredToken line from before
  // a credential renewal is read back as the new failure and retrying is abandoned.
  t.logMark = t.log.length;
  // Which credentials this session was opened with, so a later load can tell a
  // renewal of the same access from a switch to a different account, role or region.
  t.meta.principal = creds.principal();
  t.meta.stale = false;
  t.meta.suspended = false;

  const watch = (stream) => {
    stream.setEncoding('utf8');
    stream.on('data', (chunk) => {
      for (const line of chunk.split('\n')) push(t, line);
      // Captured so a health check can ask AWS whether this session is still alive
      // — the only way to catch a plugin that is still running over a dead session.
      if (!t.meta.sessionId) {
        const m = chunk.match(/SessionId:\s*([\w@.\-]+)/i);
        if (m) t.meta.sessionId = m[1];
      }
      // The plugin prints this once the local listener is actually accepting.
      if (t.meta.status === 'starting' && /port \[?\d+\]? opened|Waiting for connections/i.test(chunk)) {
        t.meta.status = 'open';
        // Only declare recovery once it has actually held, so a flapping session
        // cannot reset the retry budget on every cycle.
        if (t.meta.attempts > 0 && !t.stableTimer) {
          t.stableTimer = setTimeout(() => {
            t.stableTimer = null;
            if (t.meta.status !== 'open') return;
            emit('restored', { ...t.meta });
            t.meta.attempts = 0;
            notify();
          }, STABLE_MS);
        }
      }
      notify();
    });
  };
  watch(proc.stdout);
  watch(proc.stderr);

  proc.on('error', (err) => {
    push(t, err.message);
    t.meta.status = 'error';
    notify();
  });

  proc.on('exit', (code, signal) => {
    const manual = !!t.meta.stopping;
    const wasUp = t.meta.status === 'open' || t.meta.status === 'starting';
    t.meta.exitCode = code;
    t.meta.exitSignal = signal;
    t.meta.endedAt = Date.now();
    t.proc = null;
    if (t.stableTimer) {
      clearTimeout(t.stableTimer);
      t.stableTimer = null;
    }

    if (manual) {
      t.meta.status = 'stopped';
      notify();
      return;
    }

    // An unasked-for exit is what the user needs telling about: the local port is
    // dead and whatever was using it is about to fail. A clean exit code still
    // counts, because the CLI exits 0 when the remote end closes the session.
    if (!wasUp) {
      t.meta.status = code === 0 ? 'stopped' : 'error';
      notify();
      return;
    }

    t.meta.dropped = true;
    t.meta.reason = lastMeaningfulLine(t) || 'The SSM session ended unexpectedly.';

    if (t.meta.autoReconnect) {
      scheduleRetry(t);
    } else {
      t.meta.status = 'error';
      emit('dropped', { ...t.meta });
    }
    notify();
  });
}

function scheduleRetry(t) {
  const fail = (why) => {
    t.meta.status = 'error';
    t.meta.reason = why;
    clearTimer(t);
    emit('gaveup', { ...t.meta });
    notify();
  };

  // CLI output rarely ends in punctuation, so don't run two sentences together.
  const because = (t.meta.reason || '').replace(/[.\s]*$/, '');

  if (UNRECOVERABLE.test(because)) {
    return fail(`${because}. Reconnecting will not help until that is fixed.`);
  }
  if (t.meta.attempts >= MAX_ATTEMPTS) {
    return fail(`${because}. Gave up after ${MAX_ATTEMPTS} reconnect attempts.`);
  }

  const delay = BACKOFF_MS[Math.min(t.meta.attempts, BACKOFF_MS.length - 1)];
  t.meta.attempts += 1;
  t.meta.status = 'reconnecting';
  t.meta.retryAt = Date.now() + delay;
  notify();

  t.timer = setTimeout(async () => {
    t.timer = null;
    t.meta.retryAt = null;
    try {
      guardCredentials(t);
      // Our own orphan is cleared silently; anything else has to be asked about,
      // even on an automatic retry.
      await clearPort(t.meta.localPort, { allowForeignKill: !!t.meta.foreignKillApproved });
      launch(t);
    } catch (err) {
      if (err.needsConfirmation && !t.meta.askedForeignKill) {
        // Ask once per drop, not once per attempt.
        t.meta.askedForeignKill = true;
        t.meta.status = 'reconnecting';
        notify();
        emit('confirm-kill', { ...t.meta, conflict: err.conflict });
        return;
      }
      // Port taken or credentials gone: both need the user, so stop retrying.
      fail(err.message);
    }
    notify();
  }, delay);
}

async function start({ instanceId, instanceName, remotePort, localPort, label, allowForeignKill = false }) {
  guardCredentials();
  if (!instanceId) throw new Error('Pick an instance.');

  const remote = Number(remotePort);
  if (!Number.isInteger(remote) || remote < 1 || remote > 65535) {
    throw new Error('Remote port must be 1-65535.');
  }

  let local;
  let cleared = null;
  if (localPort) {
    local = Number(localPort);
    if (!Number.isInteger(local) || local < 1 || local > 65535) {
      throw new Error('Local port must be 1-65535.');
    }
    // A port still listed in Tunnels belongs to that card until it is dismissed.
    // Checked before touching the OS, so no process is killed for a port that was
    // never going to be granted.
    const held = claimant(local);
    if (held) throw new Error(claimMessage(local, held));

    // Same rules as a reconnect: our own leftovers are cleared automatically, and
    // anything else is asked about first.
    cleared = await clearPort(local, { allowForeignKill });
  } else {
    // Auto-allocation must also respect existing cards, or it could hand out a port
    // that a stopped tunnel is still holding for its Reconnect.
    local = null;
    for (let i = 0; i < 20; i++) {
      const candidate = await ports.findFreePort();
      if (!claimant(candidate)) {
        local = candidate;
        break;
      }
    }
    if (local === null) {
      throw new Error('Could not find a free local port. Dismiss some stopped tunnels and try again.');
    }
  }

  const id = `t${++seq}`;
  const t = {
    proc: null,
    timer: null,
    log: [],
    meta: {
      id,
      instanceId,
      instanceName: instanceName || instanceId,
      label: label || `${instanceName || instanceId}:${remote}`,
      remotePort: remote,
      localPort: local,
      status: 'starting',
      autoReconnect: false,
      attempts: 0,
      retryAt: null,
      startedAt: Date.now(),
    },
  };
  tunnels.set(id, t);

  try {
    launch(t);
  } catch (err) {
    tunnels.delete(id);
    throw err;
  }

  notify();
  return { ...t.meta, cleared };
}

// Manual reconnect: same instance, same remote port, same local port. Never
// silently moves to a different port.
//
// If our own orphaned plugin is squatting on the port it is cleared automatically.
// If something else holds it, this throws NeedsConfirmation and the caller must ask
// the user before calling again with allowForeignKill.
async function reconnect(id, { allowForeignKill = false } = {}) {
  const t = get(id);
  if (isLive(t.meta) && t.meta.status !== 'reconnecting') {
    throw new Error('That tunnel is already running.');
  }
  clearTimer(t);
  guardCredentials(t);
  const cleared = await clearPort(t.meta.localPort, { allowForeignKill });

  t.meta.attempts = 0;
  t.meta.reason = null;
  launch(t);
  notify();
  return { ...t.meta, cleared };
}

// Declares a tunnel dead from outside its own exit handler.
//
// A tunnel can stop working while its process keeps running: after the machine
// sleeps or the network drops, the SSM session dies server-side but the local
// plugin may sit there holding the port. Nothing exits, so `proc.on('exit')` never
// fires and the UI would keep claiming "Open" indefinitely. Health checks call this
// so such a zombie is treated exactly like any other unexpected drop, including
// auto-reconnect.
function reportDead(id, reason) {
  const t = tunnels.get(id);
  if (!t) return;
  if (!isLive(t.meta)) return;

  t.meta.dropped = true;
  t.meta.reason = reason;
  t.meta.endedAt = Date.now();

  const proc = t.proc;
  const pgid = t.pgid;
  const port = t.meta.localPort;
  t.proc = null;

  // Clear the corpse out of the way so a reconnect finds its own port free.
  sweep(t, proc, pgid, port);

  if (t.meta.autoReconnect) {
    scheduleRetry(t);
  } else {
    t.meta.status = 'error';
    emit('dropped', { ...t.meta });
  }
  notify();
}

// The user's answer to a 'confirm-kill' prompt raised by an automatic retry.
async function resolveForeignKill(id, approved) {
  const t = get(id);
  if (!approved) {
    t.meta.status = 'error';
    t.meta.reason = `${t.meta.reason || 'Tunnel dropped.'} Reconnect cancelled — local port ${t.meta.localPort} is still in use.`;
    clearTimer(t);
    notify();
    return { ...t.meta };
  }
  t.meta.foreignKillApproved = true;
  try {
    const meta = await reconnect(id, { allowForeignKill: true });
    t.meta.foreignKillApproved = false;
    return meta;
  } catch (err) {
    t.meta.foreignKillApproved = false;
    t.meta.status = 'error';
    t.meta.reason = err.message;
    notify();
    throw err;
  }
}

function setAutoReconnect(id, enabled) {
  const t = get(id);
  t.meta.autoReconnect = !!enabled;
  // Turning it off mid-countdown should cancel the pending attempt.
  if (!t.meta.autoReconnect && t.meta.status === 'reconnecting') {
    clearTimer(t);
    t.meta.status = 'error';
  }
  notify();
  return t.meta.autoReconnect;
}

function stop(id) {
  const t = tunnels.get(id);
  if (!t) return;
  clearTimer(t);
  t.meta.stopping = true;
  t.meta.autoReconnect = false; // an explicit stop means stop
  t.meta.status = t.proc ? 'stopping' : 'stopped';
  notify();
  if (!t.proc) return;

  const proc = t.proc;
  const pgid = t.pgid;
  const port = t.meta.localPort;

  // SIGTERM the whole group, not just the CLI: killing only the parent leaves
  // session-manager-plugin alive holding the local port, which is exactly what
  // makes a later same-port reconnect fail.
  if (pgid) {
    try {
      process.kill(-pgid, 'SIGTERM');
    } catch {}
  }
  try {
    proc.kill('SIGTERM');
  } catch {}

  // Escalate promptly: a slow teardown means a quick reconnect finds its own port
  // still occupied. 1.5s is ample for a cooperative exit.
  setTimeout(() => sweep(t, proc, pgid, port), 1500);
}

// Makes certain nothing of ours is left running or holding the port. Works from the
// pids we recorded, so it catches an orphan whether or not it still has the port.
async function sweep(t, proc, pgid, port) {
  if (proc && proc.exitCode === null && proc.signalCode === null) {
    if (pgid) {
      try {
        process.kill(-pgid, 'SIGKILL');
      } catch {}
    }
    try {
      proc.kill('SIGKILL');
    } catch {}
  }

  for (const p of owned.byTunnel(t.meta.id)) {
    if (procs.alive(p.pid)) await procs.terminate(p.pid, { graceMs: 800 });
    owned.forget(p.pid);
  }

  // Anything of ours still on the port goes too, so the next reconnect finds it free.
  if (await ports.inUse(port)) {
    const c = await ports.conflict(port);
    // Only our own leftovers, and all of them — a foreign listener sharing the port is
    // not ours to clean up.
    if (c.ours && c.canTerminate) {
      for (const h of c.holders) {
        await procs.terminate(h.pid, { graceMs: 800 });
        owned.forget(h.pid);
      }
    }
  }
  notify();
}

function remove(id) {
  const t = tunnels.get(id);
  if (!t) return;
  if (isLive(t.meta)) stop(id);
  clearTimer(t);
  // Deliberately does NOT forget this tunnel's pids: a leftover process can outlive
  // the card, and forgetting it here would make the app prompt about its own
  // orphan. sweep() forgets each pid as it confirms it dead.
  tunnels.delete(id);
  notify();
}

function stopAll() {
  for (const id of tunnels.keys()) stop(id);
}

// Credentials expired, so every SSM session died with them. The tunnels are stopped
// like any other stop — but stamped, because if the user comes back with a renewal of
// the same access these exact tunnels can be brought back, and only these.
function suspendForExpiry() {
  for (const [id, t] of tunnels) {
    if (!isLive(t.meta)) continue;
    const auto = t.meta.autoReconnect;
    stop(id); // clears the retry timer and turns auto-reconnect off
    t.meta.suspended = true;
    t.meta.autoBefore = auto;
    t.meta.reason = 'Credentials expired, so the SSM session ended.';
  }
  notify();
}

// Called once a newly loaded set of credentials has been verified.
//
// An existing card is only worth anything if the new credentials are the same
// identity its session was opened with. A renewal still reaches the same instance
// through the same local port, so tunnels suspended by an expiry are resumed. A
// different account, role or region does not, and reconnecting there would put a
// different destination behind an unchanged local port — so those are marked and
// left alone for the user to decide about. Nothing is torn down here.
//
// `context.deliberate` says the user caused this on purpose — 'assume' or 'revert' —
// which does not change what happens to the tunnels, only how it is explained. A
// prompt that reads as though something unexpected happened is wrong when the user
// just asked for it.
async function reviewCredentials(context = {}) {
  const now = creds.principal();
  const stale = [];
  const resumable = [];

  for (const t of tunnels.values()) {
    const same = creds.samePrincipal(t.meta.principal, now);
    if (same === false) {
      clearTimer(t);
      t.meta.autoReconnect = false;
      t.meta.suspended = false;
      t.meta.stale = true;
      t.meta.reason = staleMessage(t.meta.principal);
      // A live tunnel is left running and its status untouched: its session was
      // opened under credentials that AWS has not rejected, so claiming it is dead
      // would be a guess. The mark and the prompt are enough.
      if (!isLive(t.meta)) t.meta.status = 'error';
      stale.push({ ...t.meta });
    } else if (same === true && t.meta.suspended) {
      resumable.push(t);
    }
    // same === null: one side's identity is unknown, so neither resume nor mark.
  }

  const resumed = [];
  for (const t of resumable) {
    t.meta.suspended = false;
    try {
      await reconnect(t.meta.id);
      if (t.meta.autoBefore) t.meta.autoReconnect = true;
      resumed.push(t.meta.id);
    } catch (err) {
      // A port that cannot be freed is the usual cause, and it is the user's to
      // resolve — the card keeps its Reconnect button.
      t.meta.status = 'error';
      t.meta.reason = err.needsConfirmation
        ? `${err.message} Reconnect to decide what to do about it.`
        : err.message;
    }
    delete t.meta.autoBefore;
  }

  notify();
  if (stale.length) {
    emit('credentials-changed', { stale, resumed, principal: now, ...context });
  }
  return { stale: stale.length, resumed: resumed.length };
}

// The user's answer to the prompt raised above: dismiss every card that belongs to
// credentials that are no longer loaded. Goes through remove(), so each one is
// stopped and its port released exactly as a manual dismissal would.
function removeStale() {
  const ids = [...tunnels.values()].filter((t) => t.meta.stale).map((t) => t.meta.id);
  for (const id of ids) remove(id);
  return ids.length;
}

function list() {
  return [...tunnels.values()].map((t) => ({ ...t.meta, log: t.log.slice(-12) }));
}

module.exports = {
  start, reconnect, resolveForeignKill, setAutoReconnect, stop, remove, stopAll, list,
  suspendForExpiry, reviewCredentials, removeStale,
  reportDead, onChange, onEvent, findFreePort: ports.findFreePort,
};
