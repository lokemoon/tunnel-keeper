'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

// Reads the shared AWS config so profiles can be listed without shelling out.
// Only non-secret fields are ever taken from these files — the app reads profile
// names, regions and how each profile gets its credentials, and never touches
// aws_access_key_id or aws_secret_access_key.

function configPath() {
  return process.env.AWS_CONFIG_FILE || path.join(os.homedir(), '.aws', 'config');
}

function credentialsPath() {
  return process.env.AWS_SHARED_CREDENTIALS_FILE || path.join(os.homedir(), '.aws', 'credentials');
}

// Minimal ini reader: sections, `key = value`, `#`/`;` comments. Nested
// sub-sections (used by some SSO setups) are flattened, which is fine here since
// we only look at top-level keys.
function parseIni(text) {
  const out = new Map();
  let section = null;
  for (const raw of String(text).split(/\r?\n/)) {
    const line = raw.replace(/[#;].*$/, '').trim();
    if (!line) continue;
    const head = line.match(/^\[(.+)\]$/);
    if (head) {
      section = head[1].trim();
      if (!out.has(section)) out.set(section, {});
      continue;
    }
    if (!section) continue;
    const kv = line.match(/^([A-Za-z0-9_.-]+)\s*=\s*(.*)$/);
    if (kv) out.get(section)[kv[1].toLowerCase()] = kv[2].trim();
  }
  return out;
}

function readIni(file) {
  try {
    return parseIni(fs.readFileSync(file, 'utf8'));
  } catch {
    return new Map();
  }
}

// How this profile obtains credentials — used to explain what to do when it fails.
function sourceOf(cfg, hasStaticKeys) {
  if (cfg.sso_session || cfg.sso_start_url) return 'sso';
  if (cfg.credential_process) return 'process';
  if (cfg.role_arn) return 'role';
  if (hasStaticKeys || cfg.aws_access_key_id) return 'keys';
  return 'unknown';
}

const LABEL = {
  sso: 'SSO',
  process: 'credential process',
  role: 'assumed role',
  keys: 'static keys',
  unknown: 'no credentials configured',
};

// Every profile the CLI would recognise, merged across both files.
function list() {
  const config = readIni(configPath());
  const credentials = readIni(credentialsPath());
  const profiles = new Map();

  const put = (name, cfg, hasStaticKeys) => {
    const existing = profiles.get(name) || {};
    profiles.set(name, { ...existing, ...cfg, hasStaticKeys: existing.hasStaticKeys || hasStaticKeys });
  };

  for (const [section, cfg] of config) {
    // `[default]` has no prefix; everything else is `[profile name]`.
    // `[sso-session x]` and `[services x]` are not profiles.
    if (/^(sso-session|services)\s/i.test(section)) continue;
    if (section === 'default') put('default', cfg, false);
    else {
      const m = section.match(/^profile\s+(.+)$/i);
      if (m) put(m[1].trim(), cfg, false);
    }
  }

  for (const [section, cfg] of credentials) {
    put(section, cfg, !!(cfg.aws_access_key_id && cfg.aws_secret_access_key));
  }

  return [...profiles.entries()]
    .map(([name, cfg]) => {
      const source = sourceOf(cfg, cfg.hasStaticKeys);
      return {
        name,
        region: cfg.region || null,
        source,
        sourceLabel: LABEL[source],
        usable: source !== 'unknown',
      };
    })
    .sort((a, b) => (a.name === 'default' ? -1 : b.name === 'default' ? 1 : a.name.localeCompare(b.name)));
}

function get(name) {
  return list().find((p) => p.name === name) || null;
}

module.exports = { list, get, configPath, credentialsPath, parseIni };
