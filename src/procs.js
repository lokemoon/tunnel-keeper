'use strict';

const { execFile } = require('node:child_process');
const binaries = require('./binaries');

function run(cmd, args, timeout = 5000) {
  return new Promise((resolve) => {
    execFile(cmd, args, { timeout, env: { ...process.env, PATH: binaries.augmentedPath() } },
      (err, stdout) => resolve(String(stdout || '')));
  });
}

// Direct children of a pid. Used to record the session-manager-plugin that the AWS
// CLI spawns, so we can recognise and clean up our own leftovers later.
async function children(pid) {
  if (process.platform === 'win32') {
    const out = await run('powershell', [
      '-NoProfile', '-NonInteractive', '-Command',
      `Get-CimInstance Win32_Process -Filter "ParentProcessId=${pid}" | Select-Object -ExpandProperty ProcessId`,
    ]);
    return out.split(/\s+/).map(Number).filter((n) => Number.isInteger(n) && n > 0);
  }
  const out = await run('pgrep', ['-P', String(pid)]);
  return out.split('\n').map((s) => Number(s.trim())).filter((n) => Number.isInteger(n) && n > 0);
}

// Every descendant, not just direct children, so a whole tunnel's process tree can
// be accounted for.
async function descendants(pid, depth = 0) {
  if (depth > 4) return [];
  const kids = await children(pid);
  const all = [...kids];
  for (const kid of kids) all.push(...(await descendants(kid, depth + 1)));
  return [...new Set(all)];
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    // EPERM means it exists but belongs to someone else.
    return err.code === 'EPERM';
  }
}

// Whether this user could actually signal the process. A process owned by another
// user (or root) reports EPERM, and no amount of retrying changes that.
function canSignal(pid) {
  if (process.platform === 'win32') return true; // decided by taskkill's exit code
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return err.code !== 'EPERM';
  }
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

// Ask politely, then insist. Resolves {ok, error} rather than throwing so callers
// can turn a failure straight into a message.
async function terminate(pid, { graceMs = 2000 } = {}) {
  // Refuse to shoot ourselves in the foot, no matter who asked.
  if (pid === process.pid || pid === process.ppid) {
    return { ok: false, error: 'That is this application itself.' };
  }
  if (!alive(pid)) return { ok: true };

  if (process.platform === 'win32') {
    // /T takes the process tree with it, /F forces it.
    const out = await run('taskkill', ['/PID', String(pid), '/T', '/F']);
    await wait(400);
    if (!alive(pid)) return { ok: true };
    return {
      ok: false,
      error: /denied|elevat/i.test(out)
        ? 'Access denied — it is owned by another user or needs Administrator rights.'
        : 'The process did not exit.',
    };
  }

  if (!canSignal(pid)) {
    return { ok: false, error: 'It belongs to another user, so this app cannot stop it.' };
  }

  try {
    process.kill(pid, 'SIGTERM');
  } catch (err) {
    if (err.code === 'ESRCH') return { ok: true };
    if (err.code === 'EPERM') {
      return { ok: false, error: 'It belongs to another user, so this app cannot stop it.' };
    }
    return { ok: false, error: err.message };
  }

  const deadline = Date.now() + graceMs;
  while (Date.now() < deadline) {
    await wait(100);
    if (!alive(pid)) return { ok: true };
  }

  try {
    process.kill(pid, 'SIGKILL');
  } catch {}
  await wait(300);
  return alive(pid) ? { ok: false, error: 'The process would not exit.' } : { ok: true };
}

// Kill a whole process group (POSIX) or tree (Windows), so the CLI's
// session-manager-plugin child cannot survive as an orphan holding the local port.
async function terminateTree(pid, pgid) {
  if (process.platform === 'win32') return terminate(pid);
  if (pgid) {
    try {
      process.kill(-pgid, 'SIGTERM');
    } catch {}
  }
  return terminate(pid);
}

module.exports = { children, descendants, alive, canSignal, terminate, terminateTree };
