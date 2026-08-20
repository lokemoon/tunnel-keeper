'use strict';

const { contextBridge, ipcRenderer } = require('electron');

// Every call resolves to {ok, data|error} — never rejects. A rejected invoke
// (dead handler, crashed main process) would otherwise leave the UI stuck on a
// "working…" message with no way to tell the user anything went wrong.
async function call(channel, ...args) {
  try {
    const res = await ipcRenderer.invoke(channel, ...args);
    if (res && typeof res.ok === 'boolean') return res;
    return { ok: false, error: `Unexpected response from ${channel}.` };
  } catch (err) {
    const msg = (err && err.message) || String(err);
    return { ok: false, error: `Internal error (${channel}): ${msg.split('Error: ').pop()}` };
  }
}

contextBridge.exposeInMainWorld('hub', {
  getState: () => call('state'),
  loadCreds: (text) => call('creds:load', text),
  listProfiles: () => call('profiles:list'),
  useProfile: (name) => call('creds:profile', name),
  clearCreds: () => call('creds:clear'),
  setRegion: (r) => call('creds:region', r),
  listInstances: () => call('instances:list'),
  freePort: () => call('port:free'),
  startTunnel: (opts) => call('tunnel:start', opts),
  reconnectTunnel: (id, allowForeignKill) => call('tunnel:reconnect', id, allowForeignKill),
  setAutoReconnect: (id, enabled) => call('tunnel:auto', id, enabled),
  stopTunnel: (id) => call('tunnel:stop', id),
  removeTunnel: (id) => call('tunnel:remove', id),
  onState: (fn) => ipcRenderer.on('state', (_e, state) => fn(state)),
});
