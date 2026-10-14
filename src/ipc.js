'use strict';

const { ipcMain } = require('electron');
const creds = require('./creds');
const aws = require('./aws');
const tunnels = require('./tunnels');
const settings = require('./settings');
const awsConfig = require('./awsConfig');
const about = require('./about');

// Uniform envelope so the renderer has exactly one error path to render.
const handle = (channel, fn) =>
  ipcMain.handle(channel, async (_e, ...args) => {
    try {
      return { ok: true, data: await fn(...args) };
    } catch (err) {
      return { ok: false, error: (err && err.message) || String(err) };
    }
  });

function register(broadcast) {
  handle('state', () => ({ creds: creds.summary(), tunnels: tunnels.list() }));

  // The pasted blob crosses IPC exactly once and is discarded here; only the
  // redacted summary ever goes back the other way.
  handle('creds:load', async (text) => {
    const parsed = creds.parse(text);
    creds.set(parsed);
    // A region in the paste wins; otherwise reuse the last one the user chose, so
    // nobody has to re-pick their region on every launch.
    if (!parsed.region) {
      const remembered = settings.get('region');
      if (remembered) {
        try {
          creds.setRegion(remembered);
        } catch {
          settings.set('region', null); // stored value is no longer valid
        }
      }
    }
    try {
      creds.setIdentity(await aws.getCallerIdentity());
      creds.markVerified();
    } catch (err) {
      // Never leave half-loaded credentials behind after a failed verification.
      creds.clear();
      throw err;
    }
    // Existing tunnels belong to whatever credentials opened them: a renewal of the
    // same identity resumes them, a different one leaves them marked for the user.
    await tunnels.reviewCredentials();
    broadcast();
    return creds.summary();
  });

  handle('app:info', () => about.basic());
  handle('app:about', () => about.show());

  handle('profiles:list', () => ({
    profiles: awsConfig.list(),
    configPath: awsConfig.configPath(),
    lastUsed: settings.get('profile'),
  }));

  // Profile mode: the CLI resolves the credentials, so nothing secret enters this
  // process. Verification is the same get-caller-identity as the paste flow.
  handle('creds:profile', async (name) => {
    const profile = awsConfig.get(name);
    if (!profile) throw new Error(`Profile "${name}" is not in ~/.aws/config.`);

    // Precedence for the region: what the profile itself says, then the one
    // remembered from last session.
    const previous = creds.snapshot();
    creds.setProfile({ name, region: profile.region || settings.get('region') || null });

    try {
      creds.setIdentity(await aws.getCallerIdentity());
      creds.markVerified();
    } catch (err) {
      // A failed switch keeps what was working; a failed first load leaves nothing.
      if (previous.creds) creds.restore(previous);
      else creds.clear();
      throw err;
    }
    // Best-effort, and a real win for SSO profiles: a genuine expiry countdown.
    creds.setExpiration(await aws.credentialExpiration());
    await tunnels.reviewCredentials();

    settings.set('profile', name);
    if (creds.summary().region) settings.set('region', creds.summary().region);
    broadcast();
    return creds.summary();
  });

  handle('creds:clear', () => {
    tunnels.stopAll();
    creds.clear();
    broadcast();
    return creds.summary();
  });

  handle('creds:region', (r) => {
    const next = creds.setRegion(r);
    settings.set('region', next);
    broadcast();
    return next;
  });

  handle('instances:list', () => aws.listInstances());
  handle('port:free', () => tunnels.findFreePort());
  // Starting has the same port-conflict question as reconnecting, so it answers the
  // same way: as data the UI can turn into a prompt, not an error string.
  handle('tunnel:start', async (opts) => {
    try {
      return { started: true, tunnel: await tunnels.start(opts) };
    } catch (err) {
      if (err.needsConfirmation) {
        return { started: false, needsConfirmation: true, conflict: err.conflict };
      }
      throw err;
    }
  });
  // A foreign process on the port is not an error the renderer should just print —
  // it is a question. Report it as data so the card can offer the choice.
  handle('tunnel:reconnect', async (id, allowForeignKill = false) => {
    try {
      return { started: true, tunnel: await tunnels.reconnect(id, { allowForeignKill }) };
    } catch (err) {
      if (err.needsConfirmation) {
        return { started: false, needsConfirmation: true, conflict: err.conflict };
      }
      throw err;
    }
  });
  handle('tunnel:auto', (id, enabled) => tunnels.setAutoReconnect(id, enabled));
  handle('tunnel:stop', (id) => tunnels.stop(id));
  handle('tunnel:remove', (id) => tunnels.remove(id));
}

module.exports = { register };
