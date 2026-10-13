'use strict';

// Validation for the "assume role" field. Deliberately its own module with no
// dependencies: `settings.js` must re-validate a remembered ARN as it reads it back
// off disk, and requiring `creds.js` from there would be a cycle.
//
// This is a *format* check and nothing more. Whether the credentials currently
// loaded may actually assume the role is not discoverable — AWS has no API that
// lists assumable roles — so eligibility is only ever learned from the assume call
// failing. Validating first just avoids a pointless CLI round-trip.

// arn:<partition>:iam::<account>:role/<optional path>/<name>
const ROLE = /^arn:(aws|aws-cn|aws-us-gov):iam::(\d{12}):role\/(.+)$/;

// The ARN `sts get-caller-identity` hands back for an assumed role. It looks close
// enough to be pasted by mistake constantly, and fails later as an opaque
// AccessDenied, so it gets its own message naming the exact fix.
const ASSUMED = /^arn:(?:aws|aws-cn|aws-us-gov):sts::(\d{12}):assumed-role\/([^/]+)(?:\/.*)?$/;

// IAM role names, and each path segment, use this charset.
const SEGMENT = /^[\w+=,.@-]{1,64}$/;

// Returns { ok: true, arn, account, name, path } or { ok: false, error }.
function parse(input) {
  const arn = String(input == null ? '' : input).trim();
  if (!arn) return { ok: false, error: 'Enter the ARN of the role to assume.' };

  const assumed = ASSUMED.exec(arn);
  if (assumed) {
    return {
      ok: false,
      error:
        'That is the assumed-role ARN from get-caller-identity, not the role ARN. ' +
        `Use arn:aws:iam::${assumed[1]}:role/${assumed[2]}`,
    };
  }

  const m = ROLE.exec(arn);
  if (!m) {
    return {
      ok: false,
      error:
        'Not an IAM role ARN. The form is arn:aws:iam::123456789012:role/RoleName — ' +
        'a 12-digit account number, and "iam" rather than "sts".',
    };
  }

  const [, partition, account, tail] = m;
  // A trailing slash, a doubled slash, or a pasted console URL all land here rather
  // than becoming a confusing AccessDenied later.
  const segments = tail.split('/');
  if (segments.some((s) => !SEGMENT.test(s))) {
    return {
      ok: false,
      error:
        'The role name is not valid. It may be up to 64 characters of letters, digits ' +
        'or _+=,.@- and must not end in a slash.',
    };
  }

  return {
    ok: true,
    arn,
    partition,
    account,
    name: segments[segments.length - 1],
    path: segments.length > 1 ? `/${segments.slice(0, -1).join('/')}/` : '/',
  };
}

const isValid = (input) => parse(input).ok;

// A session name is stamped on every API call the role makes, so it has to be
// something CloudTrail can read back. Same charset as a role name.
function sessionName(preferred) {
  const s = String(preferred || '').trim();
  if (SEGMENT.test(s)) return s;
  return 'tunnel-keeper';
}

module.exports = { parse, isValid, sessionName };
