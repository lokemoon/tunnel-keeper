'use strict';

const creds = require('./creds');
const aws = require('./aws');
const roleArn = require('./roleArn');
const settings = require('./settings');
const tunnels = require('./tunnels');

// Assuming a role, and keeping it assumed. Shared by `ipc.js` (the user asking) and
// `health.js` (the unattended refresh) so both go through exactly one sequence.
//
// What makes this worth having at all is not the role switching but the Expiration:
// sts:assume-role states when the derived credentials die, which is the one case where
// paste mode gets a trustworthy clock, and the only way an unattended refresh can know
// when to act. Without it the tunnels simply die on the hour.

// Refreshed this far ahead of expiry. Comfortably more than one health interval, so a
// single missed or slow check cannot let the credentials lapse.
const RENEW_BEFORE_S = 5 * 60;

// The role being held, kept outside creds.js because it is not itself a credential.
let held = null; // { roleArn, sessionName, durationSeconds, usedMfa, baseAccount }
let renewing = false;

function snapshotIdentity() {
  const s = creds.summary();
  return { account: s.account, arn: s.arn, username: s.username };
}

// One attempt: swap in the role's credentials and confirm with AWS who we now are.
// On any failure the base identity is put back exactly as it was, because a half
// assumed state would leave every later call running as nobody in particular.
async function exchange({ arn, sessionName, durationSeconds, mfaSerial, tokenCode }) {
  const baseIdentity = snapshotIdentity();
  const credentials = await aws.assumeRole({
    roleArn: arn,
    sessionName,
    durationSeconds,
    mfaSerial,
    tokenCode,
  });

  creds.setAssumed({
    roleArn: arn,
    roleName: roleArn.parse(arn).name,
    sessionName,
    credentials,
  });

  try {
    creds.setIdentity(await aws.getCallerIdentity());
    creds.markVerified();
  } catch (err) {
    creds.clearAssumed();
    creds.setIdentity(baseIdentity);
    throw err;
  }
  return baseIdentity;
}

// The user asking to assume a role.
async function assume({ roleArn: input, sessionName, durationSeconds, mfaSerial, tokenCode }) {
  if (!creds.has()) throw new Error('Load credentials first.');

  // Format only. Whether these credentials *may* assume the role is not knowable in
  // advance — AWS has no API that lists assumable roles — so the assume call failing
  // is the eligibility answer, and aws.explain() maps it to say so.
  const parsed = roleArn.parse(input);
  if (!parsed.ok) throw new Error(parsed.error);

  const session = roleArn.sessionName(sessionName);
  const baseIdentity = await exchange({
    arn: parsed.arn,
    sessionName: session,
    durationSeconds,
    mfaSerial,
    tokenCode,
  });

  held = {
    roleArn: parsed.arn,
    sessionName: session,
    durationSeconds: durationSeconds || null,
    // A role that needed MFA cannot be refreshed unattended: a fresh code is required
    // every time, and there is nobody to type one. Recorded so the refresh does not
    // retry forever against a wall.
    usedMfa: !!(mfaSerial && tokenCode),
    baseAccount: baseIdentity.account || null,
  };

  // Remembered against the account the role was assumed *from*, so changing
  // credentials does not offer roles from an account you are no longer in.
  if (held.baseAccount) settings.rememberRole(held.baseAccount, parsed.arn);

  // The principal has deliberately changed, so existing tunnels have to be reviewed
  // exactly as they are after any other credential change.
  await tunnels.reviewCredentials({ deliberate: 'assume', role: parsed.name });
  return creds.summary();
}

// Back to the base identity.
async function revert() {
  if (!creds.isAssumed()) return creds.summary();
  held = null;
  creds.clearAssumed();
  try {
    creds.setIdentity(await aws.getCallerIdentity());
    creds.markVerified();
  } catch {
    // The base credentials may themselves have expired while the role was held. The
    // existing expiry and offline machinery reports that; nothing to add here.
  }
  await tunnels.reviewCredentials({ deliberate: 'revert' });
  return creds.summary();
}

// Does the held role need refreshing now? Unlike everything else in health.js this is
// acted on rather than merely reported, because letting it lapse kills every tunnel.
function dueForRenewal() {
  if (!held || !creds.isAssumed() || renewing) return false;
  if (held.usedMfa) return false;
  const secs = creds.expiresInSeconds();
  return secs !== null && secs <= RENEW_BEFORE_S;
}

// Re-assume from the base credentials, which is why creds.js keeps them. Returns a
// reason string on failure, or null on success.
async function renew() {
  if (!held) return 'No role is being held.';
  renewing = true;
  try {
    await exchange({
      arn: held.roleArn,
      sessionName: held.sessionName,
      durationSeconds: held.durationSeconds,
    });
    return null;
  } catch (err) {
    return err.message;
  } finally {
    renewing = false;
  }
}

function current() {
  if (!held || !creds.isAssumed()) return null;
  return { ...held, expiresInSeconds: creds.expiresInSeconds() };
}

// Credentials were replaced wholesale, so a role held against the previous ones is
// meaningless. Called by ipc.js on load/clear.
function forget() {
  held = null;
}

function recentFor(account) {
  return settings.recentRoles(account);
}

module.exports = { assume, revert, renew, dueForRenewal, current, forget, recentFor, RENEW_BEFORE_S };
