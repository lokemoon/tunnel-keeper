'use strict';

const { execFile } = require('node:child_process');
const creds = require('./creds');
const binaries = require('./binaries');

// Run the AWS CLI with the in-memory credentials. Never logs argv values that
// could carry a secret — nothing secret is ever passed as an argument anyway.
// The CLI's own wording ("InvalidClientTokenId", "SignatureDoesNotMatch") does not
// tell anyone what to actually do, so map the ones we expect onto a fix.
function explain(raw, err) {
  const s = raw || '';
  if (err && err.code === 'ENOENT') {
    return 'AWS CLI not found. Install the AWS CLI v2 and make sure `aws` is on your PATH.';
  }
  if (err && err.killed) {
    return 'AWS timed out. Check your network or VPN connection and try again.';
  }
  // The same AWS error needs different advice depending on where the credentials
  // came from: you re-copy a paste, but you fix a profile in ~/.aws.
  const profile = creds.summary().profile;

  if (/ExpiredToken|token included in the request is expired/i.test(s)) {
    return profile
      ? `The credentials for profile "${profile}" have expired. Refresh them (for SSO: aws sso login --profile ${profile}).`
      : 'These credentials have already expired. Copy a fresh set from your access portal.';
  }
  if (/InvalidClientTokenId|security token included in the request is invalid/i.test(s)) {
    return profile
      ? `AWS rejected the credentials for profile "${profile}" as invalid. Check that profile in ~/.aws.`
      : 'AWS rejected these credentials as invalid. The session token is usually truncated — re-copy the whole block.';
  }
  if (/SignatureDoesNotMatch/i.test(s)) {
    return profile
      ? `AWS rejected the secret access key for profile "${profile}". Check that profile in ~/.aws/credentials.`
      : 'AWS rejected the secret access key. It looks incomplete — re-copy the whole block.';
  }
  // SSO sessions are refreshed by a login, not by anything this app can do.
  if (/SSO session .* (expired|invalid)|Error loading SSO Token|sso-session|Token has expired and refresh failed/i.test(s)) {
    const profile = creds.summary().profile;
    return `That profile's SSO session has expired. Run: aws sso login${profile ? ` --profile ${profile}` : ''}`;
  }
  if (/Unable to locate credentials/i.test(s)) {
    if (profile) {
      return (
        `Profile "${profile}" has no usable credentials. Add keys to ~/.aws/credentials, ` +
        `or if it is an SSO profile run: aws sso login --profile ${profile}`
      );
    }
    return 'AWS saw no credentials at all. Re-copy the block and try again.';
  }
  // "profile ()" means an empty AWS_PROFILE reached the CLI — a bug on our side,
  // not a missing profile.
  if (/config profile \(\s*\) could not be found/i.test(s)) {
    return 'A stale AWS_PROFILE is interfering. Restart the app and load your credentials again.';
  }
  if (/ProfileNotFound|config profile .* could not be found/i.test(s)) {
    const profile = creds.summary().profile;
    return profile
      ? `Profile "${profile}" is no longer in ~/.aws/config. Pick another profile.`
      : 'That AWS profile could not be found in ~/.aws/config.';
  }
  // Assume-role failures come first: they are the only way to learn whether you are
  // eligible for a role, so they must say that rather than read as a generic denial.
  if (/not authorized to perform: sts:AssumeRole/i.test(s)) {
    const role = (/on resource:?\s*(\S+)/i.exec(s) || [])[1];
    return (
      `These credentials are not allowed to assume ${role || 'that role'}. ` +
      "Check the role's trust policy lists this identity, or pick a different role."
    );
  }
  if (/MultiFactorAuthentication|MFA|serial-number/i.test(s) && /required|invalid|failed/i.test(s)) {
    return 'That role requires MFA. Enter the current code from your authenticator and try again.';
  }
  if (/DurationSeconds exceeds the MaxSessionDuration/i.test(s)) {
    return "Requested a longer session than the role allows. Lower it, or raise the role's maximum session duration.";
  }
  if (/AccessDenied|not authorized|UnauthorizedOperation/i.test(s)) {
    return `Your role is not permitted to do this: ${s.split('\n').pop()}`;
  }
  if (/Could not connect|EndpointConnectionError|NetworkError|getaddrinfo/i.test(s)) {
    return 'Cannot reach AWS. Check your network or VPN connection.';
  }
  return s.split('\n').slice(-2).join(' ').trim() || 'The AWS CLI failed with no output.';
}

const EXPIRED = /ExpiredToken|token included in the request is expired/i;

// Cannot-reach-AWS conditions. These say nothing about whether the credentials are
// still valid, only that we currently have no way to find out.
const UNREACHABLE =
  /Could not connect|EndpointConnectionError|ConnectTimeout|ReadTimeout|NetworkError|getaddrinfo|Temporary failure in name resolution|SSLError|ProxyError|Connection was closed|Network is unreachable|Name or service not known/i;

// Tags the error so callers can act on the kind rather than re-matching strings.
function classify(raw, err) {
  if (err && err.code === 'ENOENT') return 'missing-cli';
  if (EXPIRED.test(raw)) return 'expired';
  if (err && err.killed) return 'unreachable'; // our own timeout elapsed
  if (UNREACHABLE.test(raw)) return 'unreachable';
  if (/AccessDenied|not authorized|UnauthorizedOperation/i.test(raw)) return 'denied';
  return 'other';
}

let onExpired = () => {};

// Lets the app react the moment AWS tells us the session is dead, from whichever
// call happened to notice.
function onCredentialsExpired(fn) {
  onExpired = fn;
}

// `env` overrides which credentials the call is made with. It exists for exactly one
// case — sts:assume-role, which cannot run as the role it is acquiring — so callers
// pass creds.baseEnv(). Everything else uses whatever is currently in effect.
function cli(args, { timeout = 30000, env = null } = {}) {
  return new Promise((resolve, reject) => {
    const bin = binaries.awsOrThrow();
    execFile(bin, args, { env: env || creds.env(), timeout, maxBuffer: 32 * 1024 * 1024 }, (err, stdout, stderr) => {
      if (err) {
        const raw = (stderr || err.message || '').trim();
        const kind = classify(raw, err);
        if (kind === 'expired') {
          creds.markExpired();
          onExpired();
        }
        const wrapped = new Error(explain(raw, err));
        wrapped.kind = kind;
        return reject(wrapped);
      }
      // A successful call is proof AWS is reachable right now.
      creds.markReachable();
      resolve(stdout);
    });
  });
}

async function cliJson(args, opts) {
  const out = await cli([...args, '--output', 'json'], opts);
  return out.trim() ? JSON.parse(out) : null;
}

// Profile mode can do something paste mode never can: ask the CLI when the
// credentials it resolved actually expire. Only the Expiration is kept — the keys in
// this output are deliberately discarded.
async function credentialExpiration() {
  if (!creds.isProfile()) return null;
  try {
    const out = await cli(['configure', 'export-credentials', '--format', 'process'], { timeout: 20000 });
    const parsed = JSON.parse(out);
    return parsed.Expiration || null;
  } catch {
    // Static-key profiles have no expiry, and older CLIs lack the command.
    return null;
  }
}

// Exchanges the base credentials for a role's. Runs with creds.baseEnv() because an
// assumed set cannot assume on its own behalf, and because re-assuming on expiry has
// to work from credentials that have not expired with it.
//
// Eligibility is not checkable in advance — AWS has no API listing assumable roles —
// so an AccessDenied here IS the eligibility answer, and explain() maps it to say so.
async function assumeRole({ roleArn, sessionName, durationSeconds, mfaSerial, tokenCode }) {
  const args = [
    'sts', 'assume-role',
    '--role-arn', roleArn,
    '--role-session-name', sessionName,
  ];
  if (durationSeconds) args.push('--duration-seconds', String(durationSeconds));
  // Only sent when the role's trust policy demands it; an unnecessary serial is itself
  // an error, so these travel together or not at all.
  if (mfaSerial && tokenCode) args.push('--serial-number', mfaSerial, '--token-code', tokenCode);

  const res = await cliJson(args, { env: creds.baseEnv(), timeout: 20000 });
  if (!res || !res.Credentials) throw new Error('assume-role returned no credentials.');
  return res.Credentials;
}

async function getCallerIdentity() {
  const r = await cliJson(['sts', 'get-caller-identity']);
  const arn = r.Arn || '';
  // arn:aws:sts::123:assumed-role/Role/session  ->  session
  // arn:aws:iam::123:user/name                  ->  name
  const username = arn.split('/').pop() || r.UserId || 'unknown';
  return { account: r.Account, arn, userId: r.UserId, username };
}

// Instances that SSM can actually reach, joined with their EC2 metadata so the
// list only ever offers targets a tunnel can succeed against.
async function listInstances() {
  const ssm = await cliJson([
    'ssm', 'describe-instance-information',
    '--filters', 'Key=PingStatus,Values=Online',
    '--max-results', '50',
  ]);
  const online = new Map();
  for (const i of (ssm && ssm.InstanceInformationList) || []) {
    if (i.InstanceId && i.InstanceId.startsWith('i-')) online.set(i.InstanceId, i);
  }
  if (online.size === 0) return [];

  const ids = [...online.keys()];
  const ec2 = await cliJson(['ec2', 'describe-instances', '--instance-ids', ...ids]);
  const rows = [];
  for (const res of (ec2 && ec2.Reservations) || []) {
    for (const inst of res.Instances || []) {
      const info = online.get(inst.InstanceId) || {};
      const nameTag = (inst.Tags || []).find((t) => t.Key === 'Name');
      const platform = (inst.Platform || info.PlatformType || '').toLowerCase();
      rows.push({
        id: inst.InstanceId,
        name: (nameTag && nameTag.Value) || inst.InstanceId,
        privateIp: inst.PrivateIpAddress || '',
        type: inst.InstanceType || '',
        windows: platform.includes('windows'),
        platform: info.PlatformName || inst.Platform || 'Linux',
      });
    }
  }
  rows.sort((a, b) => a.name.localeCompare(b.name));
  return rows;
}

module.exports = {
  cli, cliJson, getCallerIdentity, credentialExpiration, listInstances, onCredentialsExpired,
  assumeRole,
};
