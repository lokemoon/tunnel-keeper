'use strict';

const { Tray, Menu, app } = require('electron');
const trayIcon = require('./trayIcon');

let tray = null;
let show = () => {};
let lastKey = '';

// Overall state, worst-first: one broken tunnel matters more than three healthy ones.
function overall(creds, list) {
  if (!creds.loaded) return { status: 'idle', text: 'Not signed in' };
  if (creds.expired) return { status: 'error', text: 'Credentials expired' };

  // Offline outranks everything below it: with no confirmed connection to AWS the
  // tunnel states we hold are not trustworthy, so we do not advertise them.
  if (!creds.online) {
    const live = list.filter((t) => t.status === 'open' || t.status === 'starting').length;
    const why = creds.offlineReason || 'AWS not reachable';
    return {
      status: 'warn',
      text: live ? `Offline — ${live} tunnel${live > 1 ? 's' : ''} not working · ${why}` : `Offline — ${why}`,
    };
  }

  const open = list.filter((t) => t.status === 'open');
  const starting = list.filter((t) => t.status === 'starting');
  const retrying = list.filter((t) => t.status === 'reconnecting');
  const dropped = list.filter((t) => t.dropped && t.status === 'error');

  if (dropped.length) {
    return { status: 'error', text: `${dropped.length} tunnel${dropped.length > 1 ? 's' : ''} dropped`, count: open.length };
  }
  // Retrying is recoverable, so it reads as in-progress rather than failed.
  if (retrying.length) {
    return { status: 'warn', text: `${retrying.length} reconnecting`, count: open.length };
  }
  if (starting.length) {
    return { status: 'warn', text: `${starting.length} starting`, count: open.length };
  }
  if (open.length) {
    return { status: 'active', text: `${open.length} tunnel${open.length > 1 ? 's' : ''} open`, count: open.length };
  }
  if (creds.expiresInSeconds !== null && creds.expiresInSeconds < 300) {
    return { status: 'warn', text: 'Credentials expiring soon' };
  }
  return { status: 'ready', text: 'Ready — no tunnels' };
}

function buildMenu(creds, list, actions) {
  const items = [];

  if (creds.loaded) {
    items.push({ label: `${creds.username || 'unknown'} · ${creds.account || '—'}`, enabled: false });
    items.push({
      label: creds.online ? `Region ${creds.region}` : `Region ${creds.region} · offline`,
      enabled: false,
    });
  } else {
    items.push({ label: 'No credentials loaded', enabled: false });
  }
  items.push({ type: 'separator' });

  const DOT = { open: '●', starting: '○', reconnecting: '◌', error: '✕', stopped: '·', stopping: '·' };

  if (list.length) {
    for (const t of list) {
      const live = t.status === 'open' || t.status === 'starting' || t.status === 'reconnecting';
      // Never label a tunnel as running while we cannot confirm AWS is reachable.
      const offline = !creds.online && (t.status === 'open' || t.status === 'starting');
      const submenu = [
        { label: `Copy localhost:${t.localPort}`, click: () => actions.copy(`localhost:${t.localPort}`) },
        {
          label: 'Auto-reconnect if it drops',
          type: 'checkbox',
          checked: !!t.autoReconnect,
          click: (item) => actions.setAuto(t.id, item.checked),
        },
        { type: 'separator' },
      ];
      if (live) {
        submenu.push({ label: 'Stop this tunnel', click: () => actions.stop(t.id) });
      } else {
        // Same port, so nothing the user configured needs changing.
        submenu.push({
          label: `Reconnect on localhost:${t.localPort}`,
          click: () => actions.reconnect(t.id),
        });
        submenu.push({ label: 'Dismiss', click: () => actions.remove(t.id) });
      }

      items.push({
        label:
          `${offline ? '⚠' : DOT[t.status] || '·'} ${t.instanceName} — ` +
          `localhost:${t.localPort} → ${t.remoteHost || ''}:${t.remotePort}${offline ? ' (offline)' : ''}`,
        submenu,
      });
    }
    items.push({ type: 'separator' });
    if (list.some((t) => t.status === 'open' || t.status === 'starting' || t.status === 'reconnecting')) {
      items.push({ label: 'Stop all tunnels', click: actions.stopAll });
    }
  } else {
    items.push({ label: 'No tunnels', enabled: false });
  }

  items.push({ type: 'separator' });
  items.push({ label: 'Open Tunnel Keeper', click: actions.show });
  items.push({ label: 'Quit', click: () => app.quit() });

  return Menu.buildFromTemplate(items);
}

function create(actions) {
  show = actions.show;
  tray = new Tray(trayIcon.icon('idle'));
  tray.setToolTip('Tunnel Keeper');
  // Clicking the icon should get you to the window on both platforms; macOS opens
  // the menu on click by default, so only Windows needs this wired up.
  if (process.platform === 'win32') tray.on('click', () => show());
  tray.on('double-click', () => show());
  return tray;
}

function update(creds, list, actions) {
  if (!tray || tray.isDestroyed()) return;
  const o = overall(creds, list);

  // Rebuilding the menu on every 1s tick would close it while in use.
  const key = JSON.stringify([o, creds.online, list.map((t) => [t.id, t.status, t.localPort, t.autoReconnect])]);
  if (key === lastKey) return;
  lastKey = key;

  tray.setImage(trayIcon.icon(o.status));
  tray.setToolTip(`Tunnel Keeper — ${o.text}`);
  // macOS can show text beside the icon; keep it to the open-tunnel count.
  if (process.platform === 'darwin') {
    tray.setTitle(o.count ? String(o.count) : '');
  }
  tray.setContextMenu(buildMenu(creds, list, actions));
}

function destroy() {
  if (tray && !tray.isDestroyed()) tray.destroy();
  tray = null;
}

module.exports = { create, update, destroy, overall };
