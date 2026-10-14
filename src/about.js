'use strict';

const { execFile } = require('node:child_process');
const { app, dialog, clipboard, BrowserWindow } = require('electron');
const binaries = require('./binaries');

// The installed AWS CLI version, since most support questions start there. Never
// throws: a missing CLI is itself the useful answer.
function awsCliVersion() {
  return new Promise((resolve) => {
    let bin;
    try {
      bin = binaries.awsOrThrow();
    } catch {
      return resolve('not found');
    }
    execFile(bin, ['--version'], { timeout: 5000, env: { ...process.env, PATH: binaries.augmentedPath() } },
      (err, stdout, stderr) => {
        if (err) return resolve('could not be run');
        // "aws-cli/2.15.0 Python/3.11.6 Darwin/23.1.0 ..." -> "2.15.0"
        const m = /aws-cli\/(\S+)/.exec(`${stdout} ${stderr}`);
        resolve(m ? m[1] : 'unknown');
      });
  });
}

// Cheap and synchronous, for the version shown in the window header.
// Read from package.json, not app.getVersion(): when Electron is launched with a
// script rather than the app directory, getVersion() reports Electron's own version.
function basic() {
  const pkg = require('../package.json');
  return { name: pkg.productName || app.getName(), version: pkg.version };
}

async function info() {
  return {
    ...basic(),
    electron: process.versions.electron,
    chrome: process.versions.chrome,
    node: process.versions.node,
    platform: `${process.platform} ${process.arch}`,
    awsCli: await awsCliVersion(),
    sessionManagerPlugin: binaries.hasSessionManagerPlugin() ? 'installed' : 'not found',
  };
}

function describe(i) {
  return [
    `Version ${i.version}`,
    '',
    `AWS CLI: ${i.awsCli}`,
    `session-manager-plugin: ${i.sessionManagerPlugin}`,
    `Electron ${i.electron} · Chromium ${i.chrome} · Node ${i.node}`,
    `Platform: ${i.platform}`,
  ].join('\n');
}

async function show() {
  const i = await info();
  const parent = BrowserWindow.getFocusedWindow() || undefined;
  const opts = {
    type: 'info',
    title: `About ${i.name}`,
    message: `${i.name} ${i.version}`,
    detail: describe(i),
    buttons: ['OK', 'Copy details'],
    defaultId: 0,
    cancelId: 0,
    noLink: true,
  };
  const { response } = parent ? await dialog.showMessageBox(parent, opts) : await dialog.showMessageBox(opts);
  if (response === 1) clipboard.writeText(`${i.name} ${i.version}\n${describe(i)}`);
  return i;
}

module.exports = { basic, info, show };
