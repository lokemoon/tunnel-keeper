'use strict';

const fs = require('node:fs');
const path = require('node:path');

// A GUI app launched from Finder or the Start Menu does not inherit the shell's
// PATH — macOS gives it roughly /usr/bin:/bin:/usr/sbin:/sbin. Neither `aws` nor
// `session-manager-plugin` lives there, so without this every packaged install
// would fail with "AWS CLI not found". The plugin is located by the CLI through
// PATH, so extending PATH covers it too.
const EXTRA_DIRS = {
  darwin: [
    '/opt/homebrew/bin',
    '/usr/local/bin',
    '/opt/local/bin',
    '/usr/local/aws-cli',
    '/opt/aws-cli/bin',
    '/usr/local/sessionmanagerplugin/bin',
  ],
  win32: [
    'C:\\Program Files\\Amazon\\AWSCLIV2',
    'C:\\Program Files (x86)\\Amazon\\AWSCLIV2',
    'C:\\Program Files\\Amazon\\SessionManagerPlugin\\bin',
    'C:\\Program Files (x86)\\Amazon\\SessionManagerPlugin\\bin',
  ],
  linux: ['/usr/local/bin', '/usr/bin', '/snap/bin'],
};

const EXE = process.platform === 'win32' ? '.exe' : '';

function homeDirs() {
  const home = process.env.HOME || process.env.USERPROFILE;
  if (!home) return [];
  return [path.join(home, '.local', 'bin'), path.join(home, 'bin')];
}

function searchDirs() {
  const sep = process.platform === 'win32' ? ';' : ':';
  const fromPath = (process.env.PATH || '').split(sep).filter(Boolean);
  const extra = [...(EXTRA_DIRS[process.platform] || []), ...homeDirs()];
  // Keep the user's own PATH first so a deliberately chosen install wins.
  return [...new Set([...fromPath, ...extra])];
}

// PATH for spawned processes, widened so the CLI can find its plugin.
function augmentedPath() {
  const sep = process.platform === 'win32' ? ';' : ':';
  return searchDirs().join(sep);
}

let cached = null;

// Absolute path to the aws executable, so we never depend on PATH resolution in
// the spawn itself. Returns null when it genuinely is not installed.
function findAws() {
  if (cached && fs.existsSync(cached)) return cached;
  for (const dir of searchDirs()) {
    const candidate = path.join(dir, `aws${EXE}`);
    try {
      if (fs.statSync(candidate).isFile()) {
        cached = candidate;
        return candidate;
      }
    } catch {}
  }
  return null;
}

function awsOrThrow() {
  const bin = findAws();
  if (!bin) {
    throw new Error(
      'AWS CLI not found. Install AWS CLI v2, then reopen this app. ' +
        (process.platform === 'darwin'
          ? 'On macOS: brew install awscli'
          : 'On Windows: install the AWSCLIV2 MSI from AWS.')
    );
  }
  return bin;
}

function hasSessionManagerPlugin() {
  for (const dir of searchDirs()) {
    try {
      if (fs.statSync(path.join(dir, `session-manager-plugin${EXE}`)).isFile()) return true;
    } catch {}
  }
  return false;
}

module.exports = { findAws, awsOrThrow, hasSessionManagerPlugin, augmentedPath };
