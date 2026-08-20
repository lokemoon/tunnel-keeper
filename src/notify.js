'use strict';

const { Notification } = require('electron');

// Windows only shows notifications from an app it can identify, and the id has to
// match the installer's appId or the toast is silently dropped.
function init(appId) {
  const { app } = require('electron');
  if (process.platform === 'win32') app.setAppUserModelId(appId);
}

function show(title, body, onClick) {
  if (!Notification.isSupported()) return;
  const n = new Notification({ title, body, silent: false });
  if (onClick) n.on('click', onClick);
  n.show();
}

const where = (t) => `${t.instanceName} on localhost:${t.localPort}`;

function tunnelDropped(t, onClick) {
  show('Tunnel disconnected', `${where(t)} — ${t.reason}`, onClick);
}

// Only sent when auto-reconnect brought it back, so the user who was told it
// dropped also learns it recovered.
function tunnelRestored(t, onClick) {
  const n = t.attempts > 1 ? ` after ${t.attempts} attempts` : '';
  show('Tunnel reconnected', `${where(t)} is open again${n}.`, onClick);
}

function tunnelGaveUp(t, onClick) {
  show('Tunnel could not reconnect', `${where(t)} — ${t.reason}`, onClick);
}

function connectionLost({ reason, live }, onClick) {
  const tunnels = live
    ? ` ${live} tunnel${live > 1 ? 's are' : ' is'} not carrying traffic.`
    : '';
  show('AWS connection lost', `${reason}${tunnels}`, onClick);
}

function connectionRestored({ live }, onClick) {
  const tunnels = live
    ? ` ${live} tunnel${live > 1 ? 's are' : ' is'} working again.`
    : ' Credentials confirmed.';
  show('AWS connection restored', `Back online.${tunnels}`, onClick);
}

function credentialsExpired(onClick) {
  show(
    'AWS credentials expired',
    'Tunnels have been stopped. Load a fresh set to continue.',
    onClick
  );
}

module.exports = {
  init, show, tunnelDropped, tunnelRestored, tunnelGaveUp, connectionLost,
  connectionRestored, credentialsExpired,
};
