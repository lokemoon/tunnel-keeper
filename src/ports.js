'use strict';

const net = require('node:net');
const { execFile } = require('node:child_process');
const binaries = require('./binaries');

// IPv4 and IPv6 loopback are separate address spaces: a process on [::1]:5985 does
// not block a bind to 127.0.0.1:5985. Both must be checked, because clients resolving
// "localhost" may pick either — on macOS and Windows ::1 is usually tried first — so a
// tunnel sharing a port with a foreign IPv6 listener silently routes to the wrong one.
const LOOPBACKS = ['127.0.0.1', '::1'];

// A bind to the loopback also fails when something holds the wildcard address
// (0.0.0.0 / ::), so probing these two covers wildcard binds as well.
function bindable(port, host) {
  return new Promise((resolve) => {
    const srv = net.createServer();
    srv.once('error', (err) => {
      // No IPv6 stack at all, or the address is unavailable: that is not a conflict.
      resolve(err.code === 'EADDRNOTAVAIL' || err.code === 'EAFNOSUPPORT' ? true : false);
    });
    srv.once('listening', () => srv.close(() => resolve(true)));
    srv.listen(port, host);
  });
}

// Ask the OS for an unused ephemeral port, then confirm it is free on the other stack
// too. There is an unavoidable race between this and the CLI binding it; the caller
// reports the bind failure if it loses.
async function findFreePort() {
  for (let attempt = 0; attempt < 20; attempt++) {
    const port = await new Promise((resolve, reject) => {
      const srv = net.createServer();
      srv.unref();
      srv.on('error', reject);
      srv.listen(0, '127.0.0.1', () => {
        const { port: p } = srv.address();
        srv.close(() => resolve(p));
      });
    });
    if (!(await inUse(port))) return port;
  }
  throw new Error('Could not find a local port free on both IPv4 and IPv6.');
}

// True if anything is listening on this port on either stack.
async function inUse(port) {
  for (const host of LOOPBACKS) {
    if (!(await bindable(port, host))) return true;
  }
  return false;
}

function run(cmd, args, timeout = 4000) {
  return new Promise((resolve) => {
    execFile(cmd, args, { timeout, env: { ...process.env, PATH: binaries.augmentedPath() } },
      (err, stdout) => resolve(err && !stdout ? '' : String(stdout || '')));
  });
}

// Everything listening on this port, across both address families. Returning only the
// first match is what hid the IPv6 listener behind our own IPv4 one.
async function listeners(port) {
  const rows = process.platform === 'win32' ? await listenersWindows(port) : await listenersUnix(port);
  // Same process on both stacks is one holder as far as the user is concerned.
  const byPid = new Map();
  for (const r of rows) {
    const existing = byPid.get(r.pid);
    if (existing) existing.families.push(r.family);
    else byPid.set(r.pid, { name: r.name, pid: r.pid, families: [r.family] });
  }
  return [...byPid.values()];
}

// The first listener, kept for callers that only need to name one.
async function holder(port) {
  const all = await listeners(port);
  return all[0] || null;
}

async function listenersUnix(port) {
  // -nP: no DNS or port-name lookups. +c 0: full command name — lsof truncates to
  // 9 characters by default, which turns session-manager-plugin into "session-m".
  const out = await run('lsof', ['-nP', '+c', '0', `-iTCP:${port}`, '-sTCP:LISTEN']);
  const rows = [];
  for (const line of out.split('\n').slice(1)) {
    // COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
    const m = line.match(/^(.+?)\s+(\d+)\s+\S+\s+\S+\s+(IPv4|IPv6)\s/);
    if (!m) continue;
    // Only listeners on this exact port, not a foreign address that happens to match.
    if (!new RegExp(`[:\\]]${port}\\b`).test(line)) continue;
    rows.push({ name: m[1].trim(), pid: Number(m[2]), family: m[3] });
  }
  return rows;
}

async function listenersWindows(port) {
  // No `-p TCP`: that filters to IPv4 on some Windows versions, which would hide an
  // IPv6 listener exactly as the lsof first-match bug did.
  const out = await run('netstat', ['-ano']);
  const found = new Map(); // pid -> Set(family)
  for (const line of out.split('\n')) {
    const cols = line.trim().split(/\s+/);
    // Proto  Local Address  Foreign Address  State  PID
    if (cols.length < 5 || !/^TCP/i.test(cols[0])) continue;
    if (!/LISTENING/i.test(cols[3])) continue;

    const local = cols[1];
    const portPart = local.slice(local.lastIndexOf(':') + 1);
    if (portPart !== String(port)) continue;

    const pid = Number(cols[4]);
    if (!Number.isInteger(pid) || pid <= 0) continue;
    const family = local.startsWith('[') ? 'IPv6' : 'IPv4';
    if (!found.has(pid)) found.set(pid, new Set());
    found.get(pid).add(family);
  }
  if (!found.size) return [];

  const rows = [];
  for (const [pid, families] of found) {
    const tasks = await run('tasklist', ['/FI', `PID eq ${pid}`, '/NH', '/FO', 'CSV']);
    const m = tasks.match(/^"([^"]+)"/m);
    for (const family of families) rows.push({ name: m ? m[1] : null, pid, family });
  }
  return rows;
}

// Names the stack as well as the process, since which one a client reaches depends on
// how it resolves "localhost" — that ambiguity is the whole problem here.
function nameOf(who) {
  if (!who) return 'another program';
  const parts = [`pid ${who.pid}`];
  if (who.families && who.families.length) parts.push(who.families.join('+'));
  const detail = parts.join(', ');
  return who.name ? `${who.name} (${detail})` : `process ${detail}`;
}

function listNames(all) {
  if (!all.length) return 'another program';
  return all.map(nameOf).join(' and ');
}

// One phrasing for every "port is taken" message in the app.
async function describeConflict(port) {
  return `Local port ${port} is already in use by ${listNames(await listeners(port))}.`;
}

// Who holds the port, and whether all of it is ours — the facts every caller needs to
// decide between clearing it and asking first.
async function conflict(port) {
  const owned = require('./owned');
  const procs = require('./procs');
  // Stale pids must not be mistaken for our own processes.
  owned.prune();

  const all = await listeners(port);
  const foreign = all.filter((h) => !owned.isOwned(h.pid));

  return {
    port,
    holders: all,
    foreign,
    // The one to name in a button: a foreign holder if there is one, since that is
    // what the user has to decide about.
    holder: foreign[0] || all[0] || null,
    label: listNames(all),
    // Only ours if *every* listener is ours. One foreign IPv6 listener alongside our
    // own IPv4 one still needs the user's say-so.
    ours: all.length > 0 && foreign.length === 0,
    canTerminate: all.length > 0 && all.every((h) => procs.canSignal(h.pid)),
    message: `Local port ${port} is already in use by ${listNames(all)}.`,
  };
}

module.exports = {
  findFreePort, inUse, listeners, holder, describeConflict, conflict, nameOf,
};
