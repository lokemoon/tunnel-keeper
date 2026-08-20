'use strict';

const fs = require('node:fs');
const path = require('node:path');

// Electron ships ~55 locale .pak files (39 MB on Windows). The UI is English-only,
// so keep en-US and drop the rest; Chromium falls back to en-US for anything it
// cannot find.
//
// Only the Windows/Linux `locales/` directory is touched. macOS keeps its locale
// data in .lproj directories (handled by build.electronLanguages) while the
// framework's Resources directory holds required files like resources.pak —
// deleting from there produces a running but subtly broken app.
const KEEP = new Set(['en-US.pak']);

// Locale paks are named like `en-US.pak`, `zh_CN.pak`, `pt.pak`. Anything else in
// this directory is left alone rather than assumed disposable.
const LOCALE_PAK = /^[a-z]{2,3}([-_][A-Za-z0-9]+)?\.pak$/;

exports.default = async function afterPack(context) {
  const dir = path.join(context.appOutDir, 'locales');

  let entries;
  try {
    entries = fs.readdirSync(dir);
  } catch {
    return; // macOS, or a layout without a locales dir
  }

  let freed = 0;
  for (const name of entries) {
    if (KEEP.has(name) || !LOCALE_PAK.test(name)) continue;
    const file = path.join(dir, name);
    freed += fs.statSync(file).size;
    fs.rmSync(file);
  }

  if (freed) {
    console.log(`  • stripped unused locales  freed=${(freed / 1024 / 1024).toFixed(1)} MB`);
  }
};
