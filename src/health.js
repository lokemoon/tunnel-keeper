'use strict';

const creds = require('./creds');
const aws = require('./aws');
const tunnels = require('./tunnels');
const ports = require('./ports');
const procs = require('./procs');
const owned = require('./owned');

// Nothing here existed before, and its absence caused two wrong claims: credentials
// shown "Active" for as long as the app stayed open, and tunnels shown "Open" after
// the machine slept. Both came from trusting the last known state instead of
// checking. This module does the checking.
//
// It matters most around suspend/resume and network loss, because that is exactly
// when state goes stale without any event reaching the app.

const CRED_INTERVAL_MS = 2 * 60 * 1000;
const TUNNEL_INTERVAL_MS = 60 * 1000;

let timer = null;
let notify = () => {};
let checkingCreds = false;
let checkingTunnels = false;
let lastCredCheck = 0;

// Sessions can take a moment to notice a resumed network, so a check immediately on
// wake would report a false death.
const WAKE_GRACE_MS = 6000;

function init({ onChange }) {
  notify = onChange || (() => {});
  if (timer) clearInterval(timer);
  timer = setInterval(tick, TUNNEL_INTERVAL_MS);
  if (timer.unref) timer.unref();
}

async function tick(reason = 'interval') {
  await checkCredentials(reason);
  await checkTunnels(reason);
}

// A cheap, authoritative call: if AWS accepts it the session is alive, and if it
// answers ExpiredToken then aws.js marks the credentials expired and the app reacts
// through the existing onCredentialsExpired path.
async function checkCredentials(reason = 'interval') {
  if (checkingCreds || !creds.has()) return;
  const s = creds.summary();
  if (s.expired) return;

  const due = Date.now() - lastCredCheck >= CRED_INTERVAL_MS;
  if (reason === 'interval' && !due) return;

  checkingCreds = true;
  try {
    const identity = await aws.getCallerIdentity();
    creds.setIdentity(identity);
    creds.markVerified();
    // Profile mode can also refresh the expiry, since the CLI may have rotated the
    // underlying credentials on its own.
    if (creds.isProfile()) {
      const exp = await aws.credentialExpiration();
      if (exp) creds.setExpiration(exp);
    }
  } catch (err) {
    // A failed check is evidence in its own right. Reporting it immediately is what
    // makes the app go Offline in seconds rather than waiting out the staleness
    // window — while still never claiming the credentials are expired, which only
    // AWS can tell us.
    if (err.kind === 'unreachable') {
      creds.markUnreachable('AWS cannot be reached. Check your network or VPN connection.');
    } else if (err.kind === 'missing-cli') {
      creds.markUnreachable(err.message);
    }
    // 'expired' has already been handled by aws.js; 'denied'/'other' leave the
    // reachability state alone, since AWS clearly answered.
  } finally {
    lastCredCheck = Date.now();
    checkingCreds = false;
    notify();
  }
}

// Is this tunnel still genuinely working? Three signals, cheapest first.
async function inspect(t) {
  // 1. Our process. If it is gone, the tunnel is definitively dead.
  const pids = owned.byTunnel(t.id);
  if (pids.length && !pids.some((p) => procs.alive(p.pid))) {
    return { dead: true, reason: 'The tunnel process is no longer running.' };
  }

  // 2. The local port. Nothing listening means nothing can connect.
  if (!(await ports.inUse(t.localPort))) {
    return { dead: true, reason: `Nothing is listening on local port ${t.localPort} any more.` };
  }

  // 3. Whether the port is still ours. Something else on it means ours is gone.
  const c = await ports.conflict(t.localPort);
  if (c.holder && !c.ours) {
    return { dead: true, reason: `Local port ${t.localPort} is now held by ${c.label}.` };
  }

  return { dead: false };
}

// The definitive check, when the role is allowed to make it: AWS's own view of
// whether the session is still active. This is the only way to catch a plugin that
// is still running happily over a session AWS has already closed — which is what a
// laptop sleeping does.
let describeSessionsAllowed = true;

async function serverSideDead(open) {
  if (!describeSessionsAllowed) return null;
  const withIds = open.filter((t) => t.sessionId);
  if (!withIds.length) return null;

  let active;
  try {
    const res = await aws.cliJson(['ssm', 'describe-sessions', '--state', 'Active', '--max-results', '200']);
    active = new Set(((res && res.Sessions) || []).map((s) => s.SessionId));
  } catch (err) {
    // Most roles are not granted ssm:DescribeSessions. That is not an error worth
    // showing, but there is no point asking again every minute.
    if (/not authorized|AccessDenied/i.test(err.message)) describeSessionsAllowed = false;
    return null;
  }
  return withIds.filter((t) => !active.has(t.sessionId));
}

// States when the problem was noticed, which is all we can honestly claim — not why
// the tunnel died.
const NOTICED = {
  resume: ' Noticed when the machine woke up.',
  unlock: ' Noticed after unlocking.',
  network: ' Noticed when the network came back.',
  active: '',
  interval: '',
};

async function checkTunnels(reason = 'interval') {
  if (checkingTunnels) return;
  const open = tunnels.list().filter((t) => t.status === 'open');
  if (!open.length) return;

  checkingTunnels = true;
  try {
    for (const t of open) {
      const verdict = await inspect(t);
      if (verdict.dead) {
        tunnels.reportDead(t.id, `${verdict.reason}${NOTICED[reason] || ''}`);
      }
    }

    // Re-read: some may have just been declared dead above.
    const stillOpen = tunnels.list().filter((t) => t.status === 'open');
    const closed = await serverSideDead(stillOpen);
    for (const t of closed || []) {
      tunnels.reportDead(t.id, 'AWS reports this SSM session is no longer active.');
    }
  } finally {
    checkingTunnels = false;
    notify();
  }
}

// Called on resume, unlock and network-restored: the moments when everything the app
// believes may have quietly stopped being true.
function wake(reason) {
  lastCredCheck = 0; // force a credential check regardless of the interval
  setTimeout(() => tick(reason), WAKE_GRACE_MS);
}

module.exports = { init, tick, wake, checkCredentials, checkTunnels };
