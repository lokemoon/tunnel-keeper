'use strict';

// Recreates the `hub` API the Electron preload used to expose, on top of Tauri's
// IPC, so renderer.js is unchanged. Loaded before renderer.js.
//
// Every call resolves to {ok, data|error} — never rejects. A rejected invoke
// (missing command, crashed core) would otherwise leave the UI stuck on a
// "working…" message with no way to tell the user anything went wrong.
(function () {
  const tauri = window.__TAURI__;

  async function call(channel, ...args) {
    try {
      if (!tauri) throw new Error('Tauri bridge is not available.');
      const res = await tauri.core.invoke('call', { channel, args });
      if (res && typeof res.ok === 'boolean') return res;
      return { ok: false, error: `Unexpected response from ${channel}.` };
    } catch (err) {
      const msg = (err && err.message) || String(err);
      return { ok: false, error: `Internal error (${channel}): ${msg.split('Error: ').pop()}` };
    }
  }

  // Electron forwarded renderer console errors to the main process log; a webview
  // has no such hook, and a renderer exception is otherwise invisible — buttons
  // silently stop responding because their listeners never got wired.
  const report = (text) => {
    if (tauri) tauri.core.invoke('call', { channel: 'log:renderer', args: [String(text)] }).catch(() => {});
  };
  window.addEventListener('error', (e) => report(`${e.filename}:${e.lineno} ${e.message}`));
  window.addEventListener('unhandledrejection', (e) => report(`unhandled rejection: ${e.reason}`));
  const consoleError = console.error.bind(console);
  console.error = (...args) => {
    consoleError(...args);
    report(args.map(String).join(' '));
  };

  const hub = Object.freeze({
    getState: () => call('state'),
    appInfo: () => call('app:info'),
    showAbout: () => call('app:about'),
    loadCreds: (text) => call('creds:load', text),
    listProfiles: () => call('profiles:list'),
    useProfile: (name) => call('creds:profile', name),
    clearCreds: () => call('creds:clear'),
    setRegion: (r) => call('creds:region', r),
    listInstances: () => call('instances:list'),
    freePort: () => call('port:free'),
    startTunnel: (opts) => call('tunnel:start', opts),
    reconnectTunnel: (id, allowForeignKill) => call('tunnel:reconnect', id, !!allowForeignKill),
    setAutoReconnect: (id, enabled) => call('tunnel:auto', id, enabled),
    stopTunnel: (id) => call('tunnel:stop', id),
    removeTunnel: (id) => call('tunnel:remove', id),
    onState: (fn) => {
      if (tauri) tauri.event.listen('state', (e) => fn(e.payload));
    },
  });

  // Non-configurable, as contextBridge made it — so renderer.js must still never
  // declare a top-level `hub` of its own.
  Object.defineProperty(window, 'hub', { value: hub, writable: false, configurable: false });
})();
