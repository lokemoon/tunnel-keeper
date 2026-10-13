'use strict';

const { app, BrowserWindow, clipboard, dialog, powerMonitor, net } = require('electron');
const path = require('node:path');
const creds = require('./creds');
const aws = require('./aws');
const tunnels = require('./tunnels');
const ipc = require('./ipc');
const health = require('./health');
const connectivity = require('./connectivity');
const settings = require('./settings');
const tray = require('./tray');
const notify = require('./notify');

const APP_ID = 'com.lokemoon.tunnel-keeper';

let win = null;
let quitting = false;

function createWindow() {
  win = new BrowserWindow({
    width: 980,
    height: 780,
    minWidth: 720,
    minHeight: 560,
    title: 'Tunnel Keeper',
    backgroundColor: '#12141a',
    show: false,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
    },
  });

  // A renderer exception would otherwise be invisible: buttons silently stop
  // responding because their listeners never got wired.
  win.webContents.on('console-message', (_e, level, message, line, source) => {
    if (level >= 2) console.error(`[renderer] ${source}:${line} ${message}`);
  });
  win.webContents.on('preload-error', (_e, file, err) => {
    console.error(`[preload] ${file}: ${err && err.message}`);
  });

  win.once('ready-to-show', () => win.show());

  // Closing the window leaves tunnels running in the tray rather than killing
  // them; quitting is explicit, from the tray or the app menu.
  win.on('close', (e) => {
    if (!quitting) {
      e.preventDefault();
      win.hide();
    }
  });

  win.loadFile(path.join(__dirname, 'renderer', 'index.html'));
}

function showWindow() {
  if (!win || win.isDestroyed()) createWindow();
  else {
    win.show();
    win.focus();
  }
  if (process.platform === 'darwin') app.dock.show();
}

const trayActions = {
  show: showWindow,
  stop: (id) => tunnels.stop(id),
  stopAll: () => tunnels.stopAll(),
  remove: (id) => tunnels.remove(id),
  setAuto: (id, enabled) => tunnels.setAutoReconnect(id, enabled),
  copy: (text) => clipboard.writeText(text),
  // From the tray there is no status line to write to, so a failure has to speak
  // for itself as a notification, and a question has to become a dialog.
  reconnect: async (id) => {
    try {
      await tunnels.reconnect(id);
    } catch (err) {
      if (err.needsConfirmation) return askThenReconnect(id, err.conflict);
      notify.show('Could not reconnect', err.message, showWindow);
    }
  },
};

// The newly loaded credentials are a different identity from the one the listed
// tunnels were opened with, so none of them can be reconnected to the same place.
// Asked rather than done: the cards are the user's record of what they had set up,
// and their local ports may be written into client configs.
async function confirmResetTunnels({ stale, principal, deliberate, role }) {
  const n = stale.length;
  const subject = n === 1 ? 'That tunnel' : 'Those tunnels';
  // The user asked for this change, so say what they did rather than reporting it as a
  // surprise. The outcome for the tunnels is identical either way.
  const headline =
    deliberate === 'assume'
      ? `You assumed ${role || 'a different role'}.`
      : deliberate === 'revert'
        ? 'You returned to your base credentials.'
        : `${n === 1 ? 'One tunnel was' : `${n} tunnels were`} opened with different credentials.`;
  const { response } = await dialog.showMessageBox({
    type: 'question',
    title: deliberate ? 'Identity changed' : 'Different AWS identity',
    message: headline,
    detail:
      `${deliberate ? 'You are now' : 'These credentials are for'} ${creds.describePrincipal(principal)}. ` +
      `${subject} ${n === 1 ? 'was' : 'were'} opened as a different identity and cannot reconnect ` +
      'to the same instance, so reconnecting is disabled until dismissed. Clear them now?',
    buttons: [n === 1 ? 'Dismiss it' : 'Dismiss them', 'Keep for now'],
    defaultId: 0,
    cancelId: 1,
    noLink: true,
  });
  return response === 0;
}

// Confirmation for killing a process the app did not start. Used by the tray and by
// automatic retries, neither of which has a card to render into.
async function confirmKill(conflict) {
  if (!conflict.canTerminate) {
    await dialog.showMessageBox({
      type: 'error',
      title: 'Cannot reconnect',
      message: conflict.message,
      detail:
        'It was not started by Tunnel Keeper and belongs to another user, ' +
        'so this app cannot stop it. Free the port yourself, then reconnect.',
      buttons: ['OK'],
      defaultId: 0,
    });
    return false;
  }

  const { response } = await dialog.showMessageBox({
    type: 'warning',
    title: 'Port in use by another program',
    message: conflict.message,
    detail:
      `${conflict.holder.name || 'That process'} was not started by Tunnel Keeper. ` +
      'Stopping it will end whatever is using it. Reconnecting needs this exact port.',
    buttons: [`Stop ${conflict.holder.name || 'it'} and reconnect`, 'Cancel'],
    defaultId: 1,
    cancelId: 1,
    noLink: true,
  });
  return response === 0;
}

async function askThenReconnect(id, conflict) {
  const approved = await confirmKill(conflict);
  if (!approved) {
    notify.show('Reconnect cancelled', conflict.message, showWindow);
    return;
  }
  try {
    await tunnels.reconnect(id, { allowForeignKill: true });
  } catch (err) {
    notify.show('Could not reconnect', err.message, showWindow);
  }
}

// The window is in front of the user, so the badge and banner already tell them
// everything a notification would. Anything less — hidden to tray, minimised, behind
// another app, or on another Space — counts as background.
function windowInForeground() {
  return (
    !!win &&
    !win.isDestroyed() &&
    win.isVisible() &&
    !win.isMinimized() &&
    win.isFocused() &&
    app.isReady()
  );
}

function currentState() {
  return { creds: creds.summary(), tunnels: tunnels.list() };
}

// Online/offline transitions are announced from here, but only when the window is not
// the active window — see src/connectivity.js for the two suppression rules.
const connection = connectivity.create({
  isForeground: windowInForeground,
  getState: currentState,
  onLost: (info) => notify.connectionLost(info, showWindow),
  onRestored: (info) => notify.connectionRestored(info, showWindow),
});

function broadcast() {
  const state = currentState();
  if (win && !win.isDestroyed()) win.webContents.send('state', state);
  tray.update(state.creds, state.tunnels, trayActions);
  connection.observe(state);
}

tunnels.onChange(broadcast);
ipc.register(broadcast);

tunnels.onEvent(async (kind, t) => {
  // With auto-reconnect on, a drop is not worth interrupting for — the outcome is.
  if (kind === 'dropped') notify.tunnelDropped(t, showWindow);
  else if (kind === 'restored') notify.tunnelRestored(t, showWindow);
  else if (kind === 'gaveup') notify.tunnelGaveUp(t, showWindow);
  else if (kind === 'credentials-changed') {
    if (await confirmResetTunnels(t)) tunnels.removeStale();
    broadcast();
  } else if (kind === 'confirm-kill') {
    // An automatic retry found a foreign process on the port. Auto-reconnect still
    // does not get to kill someone else's process without being told to.
    const approved = await confirmKill(t.conflict);
    try {
      await tunnels.resolveForeignKill(t.id, approved);
    } catch {
      // resolveForeignKill has already recorded the reason on the tunnel.
    }
  }
});

aws.onCredentialsExpired(() => {
  // Every tunnel is dead once the session is, so do not leave them looking open.
  // Suspended rather than merely stopped, so a renewal of the same access can bring
  // back exactly the tunnels the expiry took down.
  tunnels.suspendForExpiry();
  notify.credentialsExpired(showWindow);
  broadcast();
});

// Push the credential expiry countdown without the UI having to poll for it.
setInterval(broadcast, 1000);

// One instance only: a second launch should surface the existing tray/window
// rather than start a rival set of tunnels.
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', showWindow);

  // Sleep, lock and network loss are exactly when the app's picture of the world goes
  // stale without any event telling it so. Nothing was watching these before, which is
  // why credentials stayed "Active" and dead tunnels stayed "Open" across a lid close.
  function watchSystemEvents() {
    powerMonitor.on('resume', () => health.wake('resume'));
    powerMonitor.on('unlock-screen', () => health.wake('unlock'));
    powerMonitor.on('user-did-become-active', () => health.wake('active'));
    // Watched in both directions. Losing the network is the fastest possible signal
    // that nothing is working, so it is reported at once instead of waiting for a
    // check to time out or for the staleness window to elapse.
    let hadNetwork = net.isOnline();
    if (!hadNetwork) creds.markUnreachable('This machine has no network connection.');

    setInterval(() => {
      const now = net.isOnline();
      if (now === hadNetwork) return;
      hadNetwork = now;

      if (now) {
        // Do not claim Online again on the strength of the interface alone — a check
        // has to succeed first. wake() runs one immediately.
        health.wake('network');
      } else {
        creds.markUnreachable('This machine has no network connection.');
        broadcast();
      }
    }, 3000).unref();
  }

  app.whenReady().then(() => {
    notify.init(APP_ID);
    // Seed the region from the last session before anything queries AWS.
    const remembered = settings.get('region');
    if (remembered) creds.setDefaultRegion(remembered);

    tray.create(trayActions);
    createWindow();
    broadcast();

    health.init({ onChange: broadcast });
    watchSystemEvents();

    app.on('activate', showWindow);
  });
}

// The tray keeps the app alive with no windows open, which is the point of it.
app.on('window-all-closed', () => {});

app.on('before-quit', () => {
  quitting = true;
  tunnels.stopAll();
  creds.clear();
  tray.destroy();
});
