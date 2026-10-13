'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { app } = require('electron');
const roleArn = require('./roleArn');

// Small JSON file in the OS-standard per-user app data directory:
//   macOS   ~/Library/Application Support/Tunnel Keeper/settings.json
//   Windows %APPDATA%\Tunnel Keeper\settings.json
// Preferences only — credentials are never persisted anywhere.
const DEFAULTS = {
  region: null,
  profile: null,
  // Role ARNs the user has assumed, keyed by the account of the base identity they
  // assumed them from — so switching credentials does not offer roles belonging to an
  // account you are no longer in. ARNs are not secrets; the credentials they yield are,
  // and those are never written here.
  roles: {},
};

const MAX_RECENT = 8;

// Whatever is on disk is re-validated rather than trusted: a hand-edited file would
// otherwise feed a malformed ARN straight to the CLI.
function cleanRoles(raw) {
  const out = {};
  if (!raw || typeof raw !== 'object') return out;
  for (const [account, list] of Object.entries(raw)) {
    if (!/^\d{12}$/.test(account) || !Array.isArray(list)) continue;
    const valid = list.filter((a) => typeof a === 'string' && roleArn.isValid(a)).slice(0, MAX_RECENT);
    if (valid.length) out[account] = valid;
  }
  return out;
}

let cache = null;

function file() {
  return path.join(app.getPath('userData'), 'settings.json');
}

function load() {
  if (cache) return cache;
  try {
    const parsed = JSON.parse(fs.readFileSync(file(), 'utf8'));
    // Ignore anything unexpected on disk rather than trusting it into the app.
    cache = {
      region: typeof parsed.region === 'string' ? parsed.region : null,
      profile: typeof parsed.profile === 'string' ? parsed.profile : null,
      roles: cleanRoles(parsed.roles),
    };
  } catch {
    cache = { ...DEFAULTS };
  }
  return cache;
}

function save(patch) {
  const next = { ...load(), ...patch };
  cache = next;
  try {
    fs.mkdirSync(path.dirname(file()), { recursive: true });
    fs.writeFileSync(file(), JSON.stringify(next, null, 2));
  } catch (err) {
    // A read-only profile should not stop the app from working this session.
    console.error(`[settings] could not save: ${err.message}`);
  }
  return next;
}

const get = (key) => load()[key];
const set = (key, value) => save({ [key]: value });

// Most-recent-first, so the field can offer the last role used without a picker.
function recentRoles(account) {
  if (!/^\d{12}$/.test(String(account || ''))) return [];
  return (load().roles[account] || []).slice();
}

function rememberRole(account, arn) {
  if (!/^\d{12}$/.test(String(account || '')) || !roleArn.isValid(arn)) return recentRoles(account);
  const roles = { ...load().roles };
  const next = [arn, ...(roles[account] || []).filter((a) => a !== arn)].slice(0, MAX_RECENT);
  roles[account] = next;
  save({ roles });
  return next;
}

function forgetRole(account, arn) {
  const roles = { ...load().roles };
  if (!roles[account]) return [];
  roles[account] = roles[account].filter((a) => a !== arn);
  if (!roles[account].length) delete roles[account];
  save({ roles });
  return roles[account] || [];
}

module.exports = { get, set, load, file, recentRoles, rememberRole, forgetRole };
