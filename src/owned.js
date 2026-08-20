'use strict';

// Which OS processes this app started.
//
// The AWS CLI spawns session-manager-plugin as a child, and that child is what
// actually binds the local port. Killing the CLI does not always take the plugin
// with it, so a stopped tunnel can leave an orphan still holding the port — which
// is why a reconnect on the same port fails.
//
// Every pid we spawn, and every descendant we discover, is recorded here. That is
// the ownership test: a port held by a pid in this registry is our own leftover and
// can be cleared without asking. Anything else belongs to the user or another tool,
// and requires explicit confirmation before we kill it.
//
// Deliberately pid-exact. A session-manager-plugin the user started in their own
// terminal is not ours, even though it has the same name and may sit on the same
// port, so it still prompts.

const registry = new Map(); // pid -> { tunnelId, role, since }

function record(pid, tunnelId, role) {
  if (!Number.isInteger(pid) || pid <= 0) return;
  registry.set(pid, { tunnelId, role, since: Date.now() });
}

function forget(pid) {
  registry.delete(pid);
}

function forgetTunnel(tunnelId) {
  for (const [pid, info] of registry) {
    if (info.tunnelId === tunnelId) registry.delete(pid);
  }
}

// Drops pids that have exited. Without this the registry would grow forever, and a
// dead pid could eventually be reused by an unrelated process which we would then
// wrongly treat as our own.
function prune() {
  const { alive } = require('./procs');
  for (const pid of [...registry.keys()]) {
    if (!alive(pid)) registry.delete(pid);
  }
}

function isOwned(pid) {
  return registry.has(pid);
}

function byTunnel(tunnelId) {
  return [...registry.entries()]
    .filter(([, v]) => v.tunnelId === tunnelId)
    .map(([pid, v]) => ({ pid, ...v }));
}

function info(pid) {
  return registry.get(pid) || null;
}

function all() {
  return [...registry.entries()].map(([pid, v]) => ({ pid, ...v }));
}

module.exports = { record, forget, forgetTunnel, prune, isOwned, byTunnel, info, all };
