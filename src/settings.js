'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { app } = require('electron');

// Small JSON file in the OS-standard per-user app data directory:
//   macOS   ~/Library/Application Support/Tunnel Keeper/settings.json
//   Windows %APPDATA%\Tunnel Keeper\settings.json
// Preferences only — credentials are never persisted anywhere.
const DEFAULTS = {
  region: null,
  profile: null,
};

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

module.exports = { get, set, load, file };
