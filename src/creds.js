'use strict';

// Credential handling. Secrets live only in this module's memory, never on disk,
// never sent to the renderer. The renderer only ever sees the redacted summary.

let creds = null; // { accessKeyId, secretAccessKey, sessionToken, expiration, region }
let identity = null; // { account, arn, userId, username }
let defaultRegion = null; // remembered from the previous session

const KEYS = {
  AWS_ACCESS_KEY_ID: 'accessKeyId',
  AWS_SECRET_ACCESS_KEY: 'secretAccessKey',
  AWS_SESSION_TOKEN: 'sessionToken',
  AWS_SECURITY_TOKEN: 'sessionToken',
  AWS_DEFAULT_REGION: 'region',
  AWS_REGION: 'region',
  AWS_CREDENTIAL_EXPIRATION: 'expiration',
};

// Strips one matched layer of surrounding quotes, plus a trailing `;` or `,` that
// shell/PowerShell snippets often carry. Values are left untouched if the quotes
// don't match, since an unbalanced quote means the paste was truncated.
function unquote(value) {
  let v = value.trim().replace(/[;,]+$/, '').trim();
  const first = v[0];
  if ((first === '"' || first === "'") && v.length > 1 && v[v.length - 1] === first) {
    v = v.slice(1, -1);
  }
  // Escaped quotes survive a JSON-ish paste; secrets never legitimately contain \".
  return v.replace(/\\(["'\\])/g, '$1');
}

// Accepts any of the shapes people actually paste. The AWS console alone hands out
// three different assignment syntaxes depending on which tab you copy from:
//   export AWS_ACCESS_KEY_ID="..."      (macOS/Linux tab, quoted or bare)
//   set AWS_ACCESS_KEY_ID=...           (Windows cmd tab)
//   $env:AWS_ACCESS_KEY_ID="..."        (PowerShell tab)
//   [profile]\naws_access_key_id = ...  (ini block)
//   {"Credentials":{"AccessKeyId":...}} (aws sts assume-role / get-session-token output)
//   {"AccessKeyId":..., ...}            (credential_process output)
function parse(text) {
  const raw = String(text || '').trim();
  if (!raw) throw new Error('Nothing to parse.');

  const out = {};
  if (raw.startsWith('{')) {
    let json;
    try {
      json = JSON.parse(raw);
    } catch {
      throw new Error('Looks like JSON but could not be parsed.');
    }
    const c = json.Credentials || json.credentials || json;
    out.accessKeyId = c.AccessKeyId || c.accessKeyId;
    out.secretAccessKey = c.SecretAccessKey || c.secretAccessKey;
    out.sessionToken = c.SessionToken || c.sessionToken;
    out.expiration = c.Expiration || c.expiration;
    out.region = json.Region || json.region;
  } else {
    for (const line of raw.split(/\r?\n/)) {
      const m = line.match(
        /^\s*(?:export\s+|set\s+|setx\s+|\$env:|\$Env:|SETX\s+)?\$?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+?)\s*$/
      );
      if (!m) continue;
      const name = m[1].toUpperCase();
      const field = KEYS[name] || KEYS['AWS_' + name];
      if (!field) continue;
      const value = unquote(m[2]);
      if (value) out[field] = value;
    }
  }

  // Name exactly what is missing — "could not parse" sends people hunting for a
  // format problem when the real cause is usually a half-copied block.
  const missing = [];
  if (!out.accessKeyId) missing.push('AWS_ACCESS_KEY_ID');
  if (!out.secretAccessKey) missing.push('AWS_SECRET_ACCESS_KEY');
  if (missing.length) {
    const found = Object.keys(out).length;
    throw new Error(
      found
        ? `Missing ${missing.join(' and ')}. Copy the whole block, not just part of it.`
        : 'No credentials found in that text. Paste the full block from your AWS access portal.'
    );
  }
  // ASIA keys are temporary and are useless without their session token.
  if (/^ASIA/i.test(out.accessKeyId) && !out.sessionToken) {
    throw new Error(
      'AWS_SESSION_TOKEN is missing. Temporary (ASIA…) keys need it — it is the long third line.'
    );
  }
  if (out.expiration) {
    const t = new Date(out.expiration);
    out.expiration = isNaN(t) ? null : t.toISOString();
  }
  return out;
}

function set(parsed) {
  creds = { ...parsed, mode: 'paste' };
  creds.loadedAt = Date.now();
  identity = null;
}

// How long ago AWS last confirmed these credentials work, in seconds.
function verifiedAgo() {
  if (!creds || !creds.verifiedAt) return null;
  return Math.round((Date.now() - creds.verifiedAt) / 1000);
}

// Two missed 2-minute health checks. This is only the fallback for failures the OS
// does not report — a captive portal or a dropped VPN, where the interface still
// looks up. An outright network loss is reported immediately via markUnreachable().
const VERIFY_STALE_S = 4 * 60;

// Set when we positively know we cannot reach AWS, so the UI does not have to wait
// out the staleness window. Deliberately distinct from expiry: unreachable means we
// do not know, whereas expired is something only AWS can tell us.
let unreachable = null; // { reason, since }

function markUnreachable(reason) {
  if (!unreachable) unreachable = { reason, since: Date.now() };
  else unreachable.reason = reason;
}

function markReachable() {
  unreachable = null;
}

// "Online" is a claim about right now: AWS answered us recently and accepted these
// credentials, and nothing since has told us the connection is gone. Computed here so
// the window and the tray cannot disagree about it.
function isOnline() {
  if (!creds) return false;
  if (unreachable) return false;
  const secs = expiresInSeconds();
  if (secs !== null && secs <= 0) return false;
  const ago = verifiedAgo();
  return ago !== null && ago <= VERIFY_STALE_S;
}

// Why we are not online, so the UI can distinguish "no network" from "not checked".
function offlineReason() {
  if (!creds) return null;
  if (unreachable) return unreachable.reason;
  const secs = expiresInSeconds();
  if (secs !== null && secs <= 0) return 'AWS has rejected these credentials.';
  const ago = verifiedAgo();
  if (ago === null) return 'AWS has not confirmed these credentials yet.';
  if (ago > VERIFY_STALE_S) {
    return `AWS has not answered for ${Math.round(ago / 60)} minutes, so these credentials cannot be confirmed.`;
  }
  return null;
}

// Profile mode: the CLI resolves the credentials itself from ~/.aws, so this app
// never sees or holds a secret at all. It also means the CLI can refresh an SSO
// session on its own, which pasted credentials can never do.
function setProfile({ name, region }) {
  if (!name) throw new Error('Pick a profile.');
  creds = {
    mode: 'profile',
    profile: name,
    region: region ? validRegion(region) : null,
    loadedAt: Date.now(),
  };
  identity = null;
}

function isProfile() {
  return !!creds && creds.mode === 'profile';
}

// Only meaningful in profile mode, where the CLI can be asked when the resolved
// credentials expire.
function setExpiration(iso) {
  if (!creds || !iso) return;
  const t = new Date(iso);
  if (!isNaN(t)) creds.expiration = t.toISOString();
}

// There is no API that reveals a token's expiry, so when AWS rejects credentials
// as expired that rejection is the only authoritative signal we ever get. Record
// it so the UI and tray can stop pretending the session is still good.
function markExpired() {
  if (!creds) return;
  creds.expiration = new Date(Date.now() - 1000).toISOString();
  creds.expiryObserved = true;
}

function setIdentity(id) {
  identity = id;
}

// Who these credentials are, reduced to the parts that must still match for a tunnel
// opened earlier to keep meaning anything: the account, the role or user, and the
// region. A renewal of the same access keeps reaching the same instance; a different
// account, role or region does not.
//
// The session name is deliberately dropped. get-caller-identity returns an
// assumed-role ARN as arn:aws:sts::123:assumed-role/Role/<session>, and that last
// segment is fresh on every renewal — comparing raw ARNs would call every legitimate
// refresh a different identity, which is precisely the case this exists to permit.
function principalOf(id, reg) {
  if (!id || !id.arn) return null;
  const assumed = /^arn:[^:]*:sts::(\d+):assumed-role\/([^/]+)\//.exec(id.arn);
  return {
    account: id.account || (assumed && assumed[1]) || null,
    who: assumed ? `assumed-role/${assumed[2]}` : id.arn.replace(/^arn:[^:]*:iam::\d+:/, ''),
    region: reg || null,
  };
}

function principal() {
  if (!creds || !identity) return null;
  return principalOf(identity, region());
}

// true / false / null, where null means at least one side is unknown. Callers must
// treat null as "do not act": neither resuming a tunnel nor declaring it orphaned is
// safe on a guess about which credentials it belongs to.
function samePrincipal(a, b) {
  if (!a || !b) return null;
  return a.account === b.account && a.who === b.who && a.region === b.region;
}

function describePrincipal(p) {
  if (!p) return 'an unknown identity';
  const where = p.region ? ` in ${p.region}` : '';
  return `${p.who || 'unknown'} (account ${p.account || 'unknown'})${where}`;
}

// The last time AWS actually accepted these credentials. Without this the UI can
// only report what it was told at load time, which is how it came to claim "Active"
// after 21 hours.
function markVerified() {
  if (creds) creds.verifiedAt = Date.now();
  markReachable();
}

function validRegion(next) {
  const r = String(next || '').trim();
  if (!/^[a-z]{2}(-[a-z]+)+-\d$/.test(r)) throw new Error(`Not a valid region: ${r}`);
  return r;
}

// Pasted blocks often omit the region, so the UI has to be able to correct it
// without making the user re-paste their credentials.
function setRegion(next) {
  if (!creds) throw new Error('No credentials loaded.');
  creds.region = validRegion(next);
  return creds.region;
}

// The region remembered from the previous session. Applied when the paste does
// not carry one, and settable before any credentials exist.
function setDefaultRegion(next) {
  defaultRegion = validRegion(next);
  return defaultRegion;
}

function clear() {
  if (creds) {
    // Best-effort scrub before dropping the reference.
    for (const k of Object.keys(creds)) creds[k] = '';
  }
  creds = null;
  identity = null;
  unreachable = null;
}

function has() {
  return !!creds;
}

// Precedence: the paste, then the region remembered from last session, then the
// surrounding environment, then a final fallback.
function region() {
  return (
    (creds && creds.region) ||
    defaultRegion ||
    process.env.AWS_REGION ||
    process.env.AWS_DEFAULT_REGION ||
    'eu-west-1'
  );
}

// Environment for spawned aws/session-manager-plugin processes.
function env() {
  if (!creds) throw new Error('No credentials loaded.');
  const e = { ...process.env };

  // A profile must be *absent*, not empty: botocore reads AWS_PROFILE='' as a
  // profile literally named "", then fails with "The config profile () could not
  // be found". Deleting the keys is what actually forces env-var credentials.
  delete e.AWS_PROFILE;
  delete e.AWS_DEFAULT_PROFILE;
  // Any inherited session token would otherwise outlive a key/secret-only paste
  // and get mismatched against it.
  delete e.AWS_SESSION_TOKEN;
  delete e.AWS_SECURITY_TOKEN;
  // Inherited keys take precedence over a profile in botocore, so they have to go
  // for profile mode to mean anything.
  delete e.AWS_ACCESS_KEY_ID;
  delete e.AWS_SECRET_ACCESS_KEY;

  if (creds.mode === 'profile') {
    e.AWS_PROFILE = creds.profile;
  } else {
    e.AWS_ACCESS_KEY_ID = creds.accessKeyId;
    e.AWS_SECRET_ACCESS_KEY = creds.secretAccessKey;
    if (creds.sessionToken) e.AWS_SESSION_TOKEN = creds.sessionToken;
  }
  e.AWS_DEFAULT_REGION = region();
  e.AWS_REGION = region();
  e.AWS_EC2_METADATA_DISABLED = 'true';
  // So the CLI can locate session-manager-plugin when we were launched from the
  // desktop rather than a shell.
  e.PATH = require('./binaries').augmentedPath();
  return e;
}

function tail(s, n = 4) {
  if (!s) return '';
  return String(s).slice(-n);
}

function expiresInSeconds() {
  if (!creds || !creds.expiration) return null;
  return Math.round((new Date(creds.expiration).getTime() - Date.now()) / 1000);
}

// The only credential-shaped thing the renderer is allowed to see.
function summary() {
  // Even with nothing loaded, report the region so the UI can show the one
  // remembered from last session.
  if (!creds) return { loaded: false, online: false, region: region() };
  const secs = expiresInSeconds();
  const profileMode = creds.mode === 'profile';
  return {
    loaded: true,
    mode: creds.mode,
    profile: profileMode ? creds.profile : null,
    // In profile mode there is no key here to redact — the CLI holds it.
    accessKeyIdRedacted: profileMode ? null : '****************' + tail(creds.accessKeyId),
    hasSessionToken: !!creds.sessionToken,
    region: region(),
    regionSupplied: !!creds.region,
    expiration: creds.expiration || null,
    expiresInSeconds: secs,
    expired: secs !== null && secs <= 0,
    // How we know the expiry, so the UI can be honest instead of guessing:
    // 'supplied' came from the paste or was reported by the CLI for a profile,
    // 'observed' means AWS rejected it as expired, 'unknown' means it genuinely
    // cannot be determined.
    expirySource: creds.expiryObserved ? 'observed' : creds.expiration ? 'supplied' : 'unknown',
    loadedAt: creds.loadedAt || null,
    verifiedAt: creds.verifiedAt || null,
    verifiedAgoSeconds: verifiedAgo(),
    // Whether AWS has confirmed these credentials recently enough to be trusted now.
    online: isOnline(),
    offlineReason: offlineReason(),
    staleAfterSeconds: VERIFY_STALE_S,
    account: identity && identity.account,
    arn: identity && identity.arn,
    username: identity && identity.username,
  };
}

module.exports = {
  parse, set, setProfile, isProfile, isOnline, offlineReason, markUnreachable, markReachable,
  setExpiration, setIdentity, markVerified, setRegion,
  principal, samePrincipal, describePrincipal,
  setDefaultRegion, markExpired, clear, has, env, region, summary, expiresInSeconds,
};
