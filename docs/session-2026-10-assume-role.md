# Session record — assume-role support, and CLAUDE.md corrections

**Date:** 2026-10-07 → 2026-10-09
**Repo:** `tunnel-keeper` (Electron + AWS CLI v2 SSM tunnel manager)
**Base commit:** `ae1f44e` (Initial commit)
**Status: all work below is UNCOMMITTED** — 12 files modified, 2 added.

This is a tool-agnostic record of what changed and why, written so any agent or person
can pick the work up. Durable rules live in `CLAUDE.md`; this file is the narrative,
the decisions, and the verification recipes. If you use an agent that reads `AGENTS.md`
rather than `CLAUDE.md`, point it at `CLAUDE.md` — the content is vendor-neutral.

---

## 1. Background: the bug that motivated the feature

An earlier session diagnosed why tunnels created *before* a credential expiry kept
reconnecting in a loop after fresh credentials were loaded, while tunnels created after
were stable. Root cause, now fixed and described in `CLAUDE.md`:

`meta.sessionId` was captured only while empty and never reset by `launch()`, so a
reconnected tunnel kept its *first* session's id forever. The 60-second health check
compared that dead id against AWS's active-session list, declared a healthy tunnel dead,
and reconnected — and because `STABLE_MS` (10s) is shorter than `TUNNEL_INTERVAL_MS`
(60s), the retry budget reset before `MAX_ATTEMPTS` could stop it. Unbounded flap.

**That history is the main justification for the auto-renew in part 3.** A lapsed
credential is what starts this class of failure, so refreshing before expiry prevents it
rather than recovering from it.

---

## 2. CLAUDE.md corrections (3 items, verified against the working copy)

| # | Correction |
| --- | --- |
| 1 | `npm run dist` (current platform only) was undocumented in the Commands block |
| 2 | The `holder()` note implied the function was removed. `ports.holder()` still exists and is exported (`src/ports.js:76,177`) but has **no callers**; what callers use is the `conflict().holder` *field* (foreign-listener-first) in `health.js:99`, `main.js:132,134`, `renderer.js:574` |
| 3 | The AWS-CLI stubbing recipe was stale — `launch()` now calls `creds.principal()`, and a harness omitting that stub gets `null` principals, which *silently* disables the renewal-vs-switch path instead of failing loudly |

### Still open, not changed

`README.md:239` documents the **Online** badge as "within the last 11 minutes".
The code says 4 minutes (`VERIFY_STALE_S = 4 * 60`, `src/creds.js`), and `README.md:226`
already says 2–4 minutes. One of the two is wrong; the intended value was never
confirmed, so both were left alone.

---

## 3. Assume-role support

### The constraint that shaped the design

**AWS has no API that lists the roles an identity may assume.** There is no entitlement
lookup for an IAM user or STS session. Only four routes exist:

| Route | Authoritative? | Cost |
| --- | --- | --- |
| Identity Center — `sso:ListAccounts` + `sso:ListAccountRoles` | **Yes** | requires SSO |
| `role_arn` entries in `~/.aws/config` | curated, unverified | free — already supported |
| `iam:ListRoles` + trust-policy inspection | a guess without `SimulatePrincipalPolicy` | IAM read, usually denied |
| Type the ARN, discover by attempting | only on failure | none |

The target machine has **no SSO cache and no `role_arn` profiles** (checked via
`awsConfig.list()`), so routes 1 and 2 were unavailable and a verified role picker was
not buildable. The feature therefore became **a validated text field with memory**, and
eligibility is learned solely from the assume call failing.

> Note for profile users: a `~/.aws/config` profile with `role_arn` + `source_profile`
> already works today with **zero code** — `awsConfig.sourceOf()` detects it, the CLI
> performs the assume itself and refreshes it, and `aws configure export-credentials`
> yields a real expiry. Prefer that when the roles can live in `~/.aws`.

### What was built

| File | Change |
| --- | --- |
| `src/roleArn.js` | **new** — IAM role ARN validation. No dependencies, so `settings.js` can re-validate from disk without a require cycle |
| `src/assume.js` | **new** — holds a role and refreshes it; one code path shared by the user action and the background refresh |
| `src/creds.js` | holds base **and** assumed credentials; `env()` prefers assumed, `baseEnv()` added; expiry/`expirySource`/`markExpired()` follow the set actually in use |
| `src/aws.js` | `assumeRole()`; `cli()` gained an `env` override; `explain()` maps assume-specific failures |
| `src/health.js` | `renewAssumedRole()` runs at the head of `tick()` |
| `src/ipc.js` | `creds:assume`, `creds:revert`, `creds:roles`; `assume.forget()` on any credential replacement |
| `src/settings.js` | remembered role ARNs keyed by base account, re-validated on read |
| `src/main.js` | the identity-changed dialog distinguishes a deliberate assume/revert from a surprise |
| `src/tunnels.js` | `reviewCredentials(context)` carries `deliberate` through to the event |
| renderer | role field + datalist, MFA fields revealed only when AWS asks, "Back to base" |

### Decisions, and what was rejected

**Validation is IAM-role-ARN-specific, not generic ARN.** It requires `iam` (not `sts`),
a 12-digit account, and a valid role-name charset with optional path. The
`arn:aws:sts::…:assumed-role/Role/session` form returned by `get-caller-identity` gets
its own message naming the exact fix, because it is the paste people actually make and
otherwise surfaces much later as an opaque `AccessDenied`.
The UI states that format validation is **not** an eligibility check.

**Only the ARN is persisted; derived keys never touch disk.** The credentials from
`sts assume-role` are as live as a pasted set. The AWS CLI does cache assumed credentials
under `~/.aws/cli/cache`, so there is precedent — *rejected*, because it would break the
one security property the whole design rests on, to save a sub-second call the background
refresh already makes.

**ARNs are keyed by the base identity's account**, so changing credentials does not offer
roles from an account you have left. An account id is not a secret.

**`sts assume-role` must run with `baseEnv()`.** A role cannot assume on its own behalf,
and re-assuming on expiry only works from base credentials that have not expired with it.
This is why `creds.js` keeps both sets.

**Auto-renew at 5 minutes before expiry** (`RENEW_BEFORE_S`), which must stay comfortably
above `CRED_INTERVAL_MS` (2 min) so one slow check cannot let the role lapse. Roles that
required MFA are **excluded** — no unattended call can supply a fresh code, so retrying
would hammer a wall. (The usual workaround, not implemented: `sts get-session-token` with
MFA once, then assume off that without MFA.)

**Assuming a role deliberately changes the principal**, so existing tunnels are marked
stale by the same machinery as any other identity change — correct, since the new role
may not reach the same instances. Only the *wording* of the prompt changes, via
`context.deliberate`.

### Side benefit worth knowing

`sts assume-role` returns an authoritative `Expiration`. While a role is held, paste mode
gets a **real countdown** (`expirySource: 'supplied'`) where it previously had none —
the case `CLAUDE.md` forbids inventing a clock for.

---

## 4. Verification performed

No test framework exists in this repo; verification means throwaway harnesses. Everything
below was exercised against a **stubbed AWS CLI** — nothing ran against real AWS, and no
real credentials were used.

| Layer | What passed |
| --- | --- |
| Pure logic | 10 ARN cases (paths, GovCloud, console URLs, the `sts` mix-up, whitespace) |
| Error mapping | real AWS error text → denial / MFA-required / duration-exceeded |
| `settings` (Electron) | recents ordering, per-account keying, tampered file scrubbed on reload, **no secrets in the file** |
| `assume` flow | bad ARN → denied → assume → renew → MFA guard → revert; base identity restored exactly after every failure |
| Critical property | **every `sts assume-role` ran with the base key**, never the role's — asserted from a log the stub writes |
| `health.tick()` | near-expiry role re-assumed; key rotated, countdown 120s → 3600s, tunnels untouched |
| IPC + renderer | real bridge in a hidden window: bad ARN message, assume, identity header, datalist, countdown, revert — **no console errors**, no stray listeners |

### Reusable stub

The stub that drove all of the above. Reads `FAKE_TTL` (seconds), `FAKE_DENY`, `FAKE_LOG`:

```sh
#!/bin/sh
case "$*" in
  *"assume-role"*)
    if [ -n "$FAKE_DENY" ]; then
      echo "An error occurred (AccessDenied) when calling the AssumeRole operation: User: arn:aws:iam::111111111111:user/bob is not authorized to perform: sts:AssumeRole on resource: arn:aws:iam::222222222222:role/Nope" >&2
      exit 255
    fi
    echo "assume-role saw key=$AWS_ACCESS_KEY_ID" >> "$FAKE_LOG"
    EXP=$(node -e "console.log(new Date(Date.now()+${FAKE_TTL:-3600}*1000).toISOString())")
    N=$(grep -c assume-role "$FAKE_LOG")
    echo "{\"Credentials\":{\"AccessKeyId\":\"ASIAROLE$N\",\"SecretAccessKey\":\"rsec\",\"SessionToken\":\"rtok\",\"Expiration\":\"$EXP\"}}"
    ;;
  *"get-caller-identity"*)
    case "$AWS_ACCESS_KEY_ID" in
      ASIAROLE*) echo '{"Account":"222222222222","Arn":"arn:aws:sts::222222222222:assumed-role/Admin/tunnel-keeper","UserId":"AROA:tk"}' ;;
      *)         echo '{"Account":"111111111111","Arn":"arn:aws:iam::111111111111:user/bob","UserId":"AIDABOB"}' ;;
    esac
    ;;
  *) echo '{}' ;;
esac
```

Point the app at it with `binaries.awsOrThrow = () => '/path/to/fake-aws.sh'` and
`binaries.augmentedPath = () => process.env.PATH`. Modules that touch `settings` need
`npx electron harness.js` rather than plain `node`, because `settings.js` uses
`app.getPath`.

---

## 5. Open items

1. **`README.md:239` says 11 minutes; the code says 4.** Decide which is intended and fix
   the other. (Not a code bug — `VERIFY_STALE_S` is internally consistent at 4 min.)
2. **Same-account assume will prompt to dismiss tunnels.** Correct in general, but if the
   assumed role reaches the same instances, the prompt is friction. Worth observing in
   real use before narrowing the rule — narrowing it safely needs more than the principal
   to compare.
3. **MFA roles cannot auto-renew.** If the real-world roles require MFA, the session-token
   pattern above would need implementing for unattended refresh to work at all.
4. **Nothing is committed.** Review `git diff` before committing.
5. **Untested against real AWS.** One smoke test at a real desk is still owed: load
   credentials, assume a real role, confirm the countdown appears and a tunnel opens.
