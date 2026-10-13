# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
npm start          # run in development
npm run dist       # package for the current platform only
npm run dist:mac   # macOS universal .dmg (must run on a Mac)
npm run dist:win   # Windows NSIS installer + portable .exe (cross-builds from macOS, no Wine)
npm run dist:all   # both
```

There is no test framework, linter, or build step — the source ships as-is into the asar.
See "Verifying renderer changes" below for how to actually exercise the UI.

## What this is

An Electron front-end that shells out to the real **AWS CLI v2** to open SSM
port-forwarding tunnels. It does not use the AWS SDK; every AWS interaction is an
`execFile`/`spawn` of the `aws` binary. AWS CLI v2 and `session-manager-plugin` must be
installed on the machine — they are not bundled.

## Module map

Main process, roughly outermost to innermost:

| File | Responsibility |
| --- | --- |
| `main.js` | window + tray lifetime, 1s broadcast, `powerMonitor`/network watching, confirm dialogs |
| `ipc.js` | all IPC handlers, each returning `{ok, data\|error}` |
| `preload.js` | context-isolated bridge; every call resolves, never rejects |
| `health.js` | the supervisor — re-verifies credentials, inspects tunnels for zombies |
| `connectivity.js` | online↔offline transition notifications (dependency-injected, testable) |
| `creds.js` | credential state, redaction, expiry, `online` computation, subprocess `env()` |
| `aws.js` | AWS CLI wrapper, error classification (`err.kind`) and message mapping |
| `awsConfig.js` | parses `~/.aws/config` + `credentials` into a profile list (non-secret fields only) |
| `assume.js` | holds an assumed role and refreshes it before expiry; shared by `ipc.js` and `health.js` |
| `roleArn.js` | IAM role ARN validation — no dependencies, so `settings.js` can re-validate from disk |
| `tunnels.js` | tunnel records and lifecycle: start, reconnect, retry, sweep, port claims |
| `ports.js` | free-port allocation, in-use checks, who-holds-this-port conflict reports |
| `procs.js` | process discovery and termination (`pgrep`/CIM, SIGTERM→SIGKILL, `taskkill /T`) |
| `owned.js` | registry of pids this app spawned — the ownership authority |
| `binaries.js` | locates `aws`/`session-manager-plugin` outside the GUI `PATH` |
| `tray.js` / `trayIcon.js` | tray state and menu / PNG icons generated at runtime |
| `notify.js` | desktop notifications |
| `settings.js` | persisted preferences (region, last profile) — never credentials |

Renderer is `src/renderer/{index.html,renderer.js,styles.css}` only.

## Architecture

Plain Electron with no bundler and no framework. `src/renderer/renderer.js` is a classic
script, so no `import`/`export` there.

**Secrets never leave the main process.** `src/creds.js` holds the parsed STS credentials
in module-level memory — never written to disk or to a profile. The renderer receives only
`creds.summary()`, which exposes a redacted key id and identity metadata. When adding a
field to the summary, keep it non-secret. Credentials reach subprocesses via
`creds.env()`, never as argv.

**State flows one way.** `src/main.js` broadcasts `{creds, tunnels}` to the renderer every
second (this also drives the expiry countdown). The renderer is close to stateless: it
re-renders from whatever it is handed. To surface something new in the UI, add it to the
broadcast payload rather than having the renderer query for it.

**Every IPC handler returns `{ok, data|error}`.** `src/ipc.js` wraps each one so a thrown
error becomes `{ok: false, error: message}`. `src/preload.js` additionally guarantees calls
**resolve rather than reject** even when a handler is missing or main crashes — a rejected
invoke used to leave the UI stuck on a "working…" message forever. Preserve both
properties when adding channels.

Handlers live in `src/ipc.js`, separate from window creation, so they can be registered
against a test window without launching the real app.

**Credentials arrive by one of two routes and `creds.mode` distinguishes them.** `'paste'`
holds secrets in memory as described above; `'profile'` holds only a profile name and sets
`AWS_PROFILE`, so the CLI resolves the credentials and this process never sees a secret.
`env()` must delete inherited `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` for profile mode to
mean anything — env vars outrank a profile in botocore. Error text in `aws.js` branches on
`creds.summary().profile`: telling someone to "re-copy the block" when they picked a profile
is wrong advice.

**Do not restructure the paste flow.** The user considers it already as simple as it should
be; profile selection was added alongside it, not in place of it. Both routes must stay
visible without a mode switch.

**Credential expiry is not discoverable in paste mode.** No AWS API returns it and the session token is
opaque, so `summary().expirySource` is `'supplied'` (the paste stated it), `'observed'`
(a call came back `ExpiredToken`, via `aws.onCredentialsExpired` → `creds.markExpired()`),
or `'unknown'`. Never invent a countdown for the `unknown` case — showing a wrong clock for
credentials is worse than showing none. In profile mode it *is* discoverable, via
`aws.credentialExpiration()` (`aws configure export-credentials`); read only the `Expiration`
from that output and discard the keys.

**An assumed role is a third credential layer, and only the ARN may touch disk.**
`creds.js` keeps `creds` (base) *and* `assumed`; `env()` hands subprocesses the assumed set
when there is one, and `baseEnv()` exists for the single call that must not use it —
`sts:assume-role` cannot run as the role it is acquiring, and re-assuming on expiry only
works from base credentials that have not expired with it. The derived keys are as live as a
paste and are memory-only; `settings.js` persists the **role ARN** keyed by the *base*
account, so changing credentials cannot offer roles from an account you have left. Three
consequences: `env()` must not set `AWS_PROFILE` while assumed or botocore resolves the base
profile again and the assume was pointless; `expiresInSeconds()` and `expirySource` must read
the assumed set, since it is what every call uses and it dies first; and `markExpired()`
records the rejection on whichever set AWS actually rejected, or reverting inherits an
expiry that was never claimed about the base.

**Which roles you may assume is not discoverable, and the UI must not pretend otherwise.**
AWS has no API listing assumable roles — Identity Center's `sso:ListAccountRoles` is the only
authoritative source and needs SSO, which this setup does not use. So `roleArn.js` validates
*shape only* (and names the `sts` assumed-role ARN mix-up specifically, because it is the
paste people actually make), and eligibility is learned solely from `sts:assume-role` failing.
That is why `explain()` maps `not authorized to perform: sts:AssumeRole` to a sentence about
trust policies: that error is the feature, not an edge case.

**A role refresh is the one thing `health.js` fixes rather than reports.** Everything else in
that module re-verifies state and tells the truth about it; `renewAssumedRole()` acts, because
a lapsed role kills every tunnel running on it. It runs before the credential check so a
refreshed role is never first reported expired, and skips roles that required MFA — no
unattended call can supply a fresh code, so retrying would hammer a wall. `RENEW_BEFORE_S`
(5 min) must stay comfortably above `CRED_INTERVAL_MS`, or one slow check lets the role lapse.

**Assuming a role deliberately changes the principal.** `reviewCredentials()` therefore marks
existing tunnels stale exactly as any other identity change does — which is correct, since the
new role may not reach the same instances — and takes a `context.deliberate` of `'assume'` or
`'revert'` that changes *only the wording* of the prompt. A dialog reading as though something
unexpected happened is wrong when the user just asked for it.

**`settings.js` is for preferences only, never credentials.** It persists to the Electron
`userData` directory and re-validates whatever it reads back off disk.

**The tray owns the app's lifetime.** `window-all-closed` deliberately does nothing and the
window's `close` is intercepted to hide, so tunnels survive closing the window; only
`before-quit` tears things down. `tray.update()` diffs before rebuilding the menu, because
rebuilding on the 1s tick closes the menu while the user is in it.

**State must be verified, not assumed — `health.js` exists because it wasn't.** Two bugs came
from trusting last-known state: the badge read "Active" for 21 hours because `expired` is
false whenever expiry is unknown and nothing ever re-checked, and tunnels read "Open" after a
sleep because death was detected *only* by `proc.on('exit')`. Credentials are re-verified on
an interval and on resume/unlock/network-restored (`powerMonitor` + `net.isOnline` polling in
`main.js`); tunnels are inspected for process liveness, a listening port, and continued
ownership of that port.

**Connectivity notifications live in `src/connectivity.js`, deliberately separated from
`main.js` and dependency-injected** (`isForeground`, `getState`, `onLost`, `onRestored`), because
the rules are worth testing without a window, a network, or real toasts. Two invariants: never
notify while `isForeground()` — the UI is the notification, and doubling it is the bug being
avoided — and never notify until the new state has survived `SETTLE_MS`, or a blip fires a
lost/restored pair. The first observation after credentials load is adopted silently.

Note when testing notifications: `electron.Notification` and `electron.Tray` are non-writable,
non-configurable exports, so they cannot be patched. `notify.js`'s internal `show` is likewise
unreachable from outside. The seam that works is wrapping the functions `main.js` actually
calls on the `notify` module object.

**Offline must be reported from evidence, not just from a timer.** Three paths set it:
`net.isOnline()` going false (polled both directions in `main.js`), a health check failing with
`err.kind === 'unreachable'` (classified in `aws.js`), and the `VERIFY_STALE_S` fallback for
breaks the OS does not report. Recovery requires a *successful* call — `creds.markVerified()`
and any successful `cli()` clear the flag; the interface returning does not. Never let an
unreachable result set expiry: `denied` and `other` leave reachability untouched, since AWS
answered.

**"Online" requires a recent successful remote check.** `creds.isOnline()` — not the renderer —
owns that rule (`verifiedAt` within `VERIFY_STALE_S`, and not expired) and publishes it as
`summary().online`, so the window and the tray can never disagree. A green badge is a claim
about the present; for credentials a wrong one is worse than an uncertain one.

**When `creds.online` is false, no tunnel may be shown as running.** Its SSM session runs over
the same AWS connection, so "Open" would be a false claim; both `renderTunnels()` and
`tray.buildMenu()` overlay "Offline" instead. This is presentation only — tunnel state is not
mutated and nothing is torn down, so the real state reappears when connectivity does. Any
signature/diff key used to skip repaints must include `credsOnline`, or the overlay will not
repaint when it flips.

**A tunnel can be dead while its process is healthy.** That is the zombie case, and only
`ssm:DescribeSessions` detects it definitively; the local signals cannot. `health.reportDead`
routes such a tunnel through the normal drop path so cleanup, notification, and auto-reconnect
all behave identically. A failure to reach AWS must never be treated as expiry — offline is
not expired.

**A manual stop is not a disconnect.** `tunnels.onEvent` emits `dropped` only when the
process exits without `meta.stopping` set, including on exit code 0 (the CLI exits cleanly
when the remote end closes). Notifying on user-initiated stops was explicitly not wanted.

**Port checks must cover IPv4 *and* IPv6, and must consider every listener.** The loopbacks
are separate address spaces: a process on `[::1]:5985` does not block a bind to
`127.0.0.1:5985`, so probing only IPv4 let a tunnel start alongside a foreign IPv6 listener —
and since macOS and Windows resolve `localhost` to `::1` first, clients silently reached the
wrong process. `ports.inUse()` probes both loopbacks (which also catches wildcard `0.0.0.0`/`::`
binds), `ports.listeners()` returns **all** holders with their families (the first-match
`ports.holder()` hid the IPv6 one behind our own IPv4 one; it is still exported but has no
callers left — the `holder` that `health.js`, `main.js` and the renderer use is the
*foreign-listener-first field* on `conflict()`), `findFreePort()` verifies both stacks, and
`conflict().ours` is true only when *every* listener is ours — one foreign listener alongside our
own still needs the user's say-so. `clearPort()` and `sweep()` must terminate all holders, since
the port is only free once both stacks are.

**A listed tunnel owns its local port until its card is dismissed.** `claimant()` in
`tunnels.js` rejects `start()` for a port held by *any* existing record regardless of status —
checked before touching the OS, so nothing is killed for a port that was never going to be
granted — and auto-allocation retries until it finds an unclaimed port. This follows from
same-port Reconnect: a stopped card is a live reservation, so two cards must never share an
address. `remove()` is what releases it.

**Reconnect is strictly same-port, and that is a requirement, not an implementation
detail.** The whole value is that a saved client config keeps working, so an occupied port
must never be reallocated — it is either cleared or reported.

**`session-manager-plugin`, not the AWS CLI, binds the local port, and it can outlive its
parent.** Tunnels are therefore spawned `detached` (own process group) and torn down by
group, with `sweep()` killing the tunnel's recorded pids afterwards. Without this a stopped
tunnel leaves an orphan holding the port and the next same-port reconnect fails.

**`tunnels.remove()` must not forget the tunnel's pids.** A leftover process can outlive its
card, and forgetting its pid makes the app prompt the user about its own orphan. `sweep()`
forgets each pid as it confirms it dead, and `owned.prune()` drops exited pids before any
ownership test so a reused pid cannot be mistaken for ours.

**`owned.js` is the ownership authority and is pid-exact by design.** Only a pid this app
recorded may be killed without asking; a same-named process the user started is foreign and
must prompt. `clearPort()` raises `NeedsConfirmation` (carrying the conflict details) rather
than deciding for the user — including on automatic retries, which ask once per drop via the
`confirm-kill` event, not once per attempt. `terminate()` and `clearPort()` both refuse
`process.pid`/`process.ppid`: the app must never signal itself.

**Teardown timing is load-bearing.** `stop()` sweeps 1.5s after SIGTERM; a slower escalation
means a prompt reconnect finds its own port still held.

**A reconnect only counts as recovered after `STABLE_MS`.** Resetting `meta.attempts` the
moment the process reports "port opened" let a flapping session reset its retry budget every
cycle and reconnect forever, firing a "restored" notification each time. Attempts reset from
a stability timer instead. Auth failures (`UNRECOVERABLE`) are never retried.

**A tunnel record survives reconnects.** `launch(t)` relaunches the CLI for an existing
record so the id, local port, and auto-reconnect toggle persist; only `start()` creates one.

**Anything that describes the *session* must be reset by `launch()`, not left on the record.**
`sessionId` is captured only while empty, so a stale one made `health.serverSideDead()` compare
a live session against a dead id, declare a healthy tunnel dead every 60s, and reconnect
forever — and because `STABLE_MS` (10s) is shorter than `TUNNEL_INTERVAL_MS` (60s), `attempts`
reset before `MAX_ATTEMPTS` could ever stop it. Only records that had reconnected at least once
were affected, which is why a tunnel created after a credential refresh looked fine. `logMark`
exists for the same reason: `t.log` outlives a session, so `lastMeaningfulLine()` scans only the
current launch — otherwise a pre-refresh `ExpiredToken` line is read as the new failure and
`UNRECOVERABLE` abandons a recoverable tunnel.

**A tunnel belongs to the credentials that opened it, and a renewal is not a switch.**
`launch()` stamps `meta.principal` from `creds.principal()` — account, role/user, and region,
with the assumed-role *session name deliberately stripped*, since it changes on every renewal
and comparing raw ARNs would call each refresh a different identity. After any successful
credential load `ipc.js` calls `tunnels.reviewCredentials()`: same principal resumes tunnels
that `suspendForExpiry()` stopped (restoring their auto-reconnect), a different one marks them
`stale` and emits `credentials-changed` so `main.js` can offer to dismiss them. Never tear the
cards down automatically — their local ports may be written into client configs. `samePrincipal`
returns `null` when either side is unknown and that must mean *do nothing*: neither resuming nor
orphaning a tunnel is safe on a guess. A live tunnel marked stale keeps its status, because AWS
has not rejected the session it is actually running.

**Renderer card state needs an explicit repaint.** `renderTunnels` diffs on a signature, so
after mutating renderer-local state such as `cardMessages`, call `repaintTunnels()` — waiting
for the next 1s broadcast leaves buttons stuck mid-action.

**The renderer has two palettes and the local clock picks one.** `styles.css` defines the
night palette on `:root` and the day palette on `:root[data-theme='light']`; `applyTheme()`
in `renderer.js` sets that attribute (light from 07:00 to 18:59 local) on load and re-checks
every minute, so a window left open across dusk flips without a restart. It is deliberately
driven by the clock rather than the OS appearance setting, and there is no manual override.
Two consequences: **every colour must be a variable declared in both blocks** — a hardcoded
hex anywhere in `styles.css` will not flip — and the light palette darkens `--accent`/`--ok`/
`--warn`/`--err` rather than reusing the dark ones, which would not carry contrast on a light
background. `applyTheme()` writes only when the value actually changes, so it cannot fight the
1s broadcast repaint. The tray icons (`trayIcon.js`) are not themed.

**Error messages are mapped, not passed through.** `explain()` in `src/aws.js` translates
CLI errors (`ExpiredToken`, `SignatureDoesNotMatch`, `InvalidClientTokenId`, ENOENT,
timeouts) into a stated fix. `creds.parse()` names the specific missing variable. The UI
renders these via `status()` in the renderer, which must always be given a terminal state
after a busy state, and must always re-enable its button.

## The tunnel record

One record per card in the Tunnels section, keyed by `id` (`t1`, `t2`, …). `list()` returns
copies of `meta` plus a log tail, so **mutating what `list()` returns does nothing** — go
through a `tunnels.*` function.

`status` is the single source of truth for what a tunnel is doing:

| status | meaning |
| --- | --- |
| `starting` | process spawned, local listener not yet confirmed |
| `open` | the plugin reported the port is accepting connections |
| `reconnecting` | dead, waiting out a backoff before an automatic retry (`retryAt`, `attempts`) |
| `stopping` / `stopped` | the user asked it to stop |
| `error` | dead and not retrying; `dropped: true` distinguishes "died on its own" from "failed to start" |

Other fields that carry meaning across files: `autoReconnect` (per-tunnel toggle),
`sessionId` (captured from CLI output, used for the `DescribeSessions` zombie check),
`reason` (why it died, shown on the card), and `cleared` (what had to be stopped to free the
port, returned from `start()`/`reconnect()` so the UI can say so).

Statuses `open`/`starting` are additionally overlaid as **Offline** in both the renderer and
the tray when `creds.online` is false — that is presentation, not a status value.

## Timings and thresholds

Spread across four files and interdependent — changing one in isolation has caused bugs.

| Constant | File | Value | Why |
| --- | --- | --- | --- |
| `CRED_INTERVAL_MS` | `health.js` | 2 min | how often credentials are re-verified |
| `TUNNEL_INTERVAL_MS` | `health.js` | 60 s | how often tunnels are health-checked |
| `WAKE_GRACE_MS` | `health.js` | 6 s | pause after resume before checking, or sessions read as falsely dead |
| `VERIFY_STALE_S` | `creds.js` | 4 min | past this, "Online" is no longer claimed — currently exactly 2× `CRED_INTERVAL_MS`, so two consecutive failed checks flip it; never set it below that or a single slow check reads as offline |
| `SETTLE_MS` | `connectivity.js` | 4 s | a transition must hold this long before it is announced |
| `STABLE_MS` | `tunnels.js` | 10 s | a reconnect must hold this long to count as recovered |
| `MAX_ATTEMPTS` / `BACKOFF_MS` | `tunnels.js` | 3 / 2s,5s,10s | bounded auto-reconnect |
| sweep delay | `tunnels.js` | 1.5 s | teardown escalation; slower leaves a reconnect finding its own port held |

## Constraints that have already caused bugs

**Never declare a top-level `const hub` in the renderer.** `contextBridge` installs `hub`
as a non-configurable global; a colliding lexical declaration throws
`Identifier 'hub' has already been declared`, which aborts the entire script and silently
kills every event listener. The bridge is aliased as `api` for this reason.

**Never spawn a bare `aws`.** A GUI app launched from Finder or the Start Menu gets roughly
`/usr/bin:/bin:/usr/sbin:/sbin` — Homebrew and `Program Files` are absent, so PATH
resolution fails for every packaged install. Use `binaries.awsOrThrow()` for an absolute
path, and `binaries.augmentedPath()` for the `PATH` handed to subprocesses (the CLI needs
it to find `session-manager-plugin` itself).

**To suppress a profile, delete the env var — do not set it to `''`.** botocore reads
`AWS_PROFILE=''` as a profile literally named `""` and fails with
`The config profile () could not be found`.

**The renderer CSP is strict** (`default-src 'none'`, `script-src 'self'`): no inline
scripts or styles, no external fonts, images, or CDN. Build DOM nodes with
`createElement`/`textContent`; do not interpolate values into `innerHTML`.

**`renderTunnels()` diffs against a JSON signature** before re-rendering, because the 1s
broadcast would otherwise collapse any open `<details>` element every tick.

**macOS ships as a single universal DMG.** Building `arm64` and `x64` DMGs together fails
in `hdiutil detach` — both want the same volume name. This is a deliberate distribution
choice (one file, works on every Mac) despite the size; per-arch DMGs would be ~half each.

**`build/afterPack.js` must only touch the Windows/Linux `locales/` directory.** On macOS
the framework's `Resources` directory holds required files such as `resources.pak`;
deleting from there yields an app that still launches but logs
`Failed to load resources.pak` and loses functionality. macOS locale trimming belongs to
`build.electronLanguages`. The hook also matches only locale-shaped filenames, so a new
Chromium resource dropped into `locales/` is left alone.

## How to verify a change

There is no test framework, so verification means writing a throwaway harness in the
scratchpad and running it. **A clean main-process log does not mean the app works** —
renderer exceptions surface only through the `console-message` forwarder in `createWindow()`,
and a syntax error there silently kills every event listener. Always assert that no console
errors were captured.

Pick the cheapest layer that actually exercises the change:

**1. Pure logic — plain `node`.** Anything not needing Electron: `creds.parse`, `awsConfig.list`,
`connectivity.create` (inject `settleMs: 300` to keep it fast), `ports` helpers.
```sh
node -e 'const c=require("./src/creds"); c.set(c.parse("...")); console.log(c.summary())'
```

**2. Modules needing `app` — `npx electron script.js`.** `settings` (needs `app.getPath`),
`trayIcon`/`tray` (need a ready app), `health`.

**3. IPC + renderer — hidden window.** `require` the real `ipc.js`, call
`ipc.register(broadcast)`, `loadFile` the renderer, then `executeJavaScript` to click real
elements and read back the DOM. This is the only layer that catches wiring failures.

**4. The real app — `require('./src/main.js')` from a harness**, so `powerMonitor`, the tray,
and the broadcast loop are all genuinely running.

### Stubbing the AWS CLI

Most tunnel behaviour can be driven without AWS by overriding two boundaries and pointing
them at a shell script:
```js
creds.has = () => true; creds.env = () => process.env;
creds.summary = () => ({ expired: false });
// Required since launch() stamps meta.principal: a harness that omits this gets null
// principals, and samePrincipal(null, …) === null silently disables the whole
// renewal-vs-switch path rather than failing loudly.
creds.principal = () => ({ account: '1', who: 'assumed-role/R', region: 'eu-west-1' });
binaries.hasSessionManagerPlugin = () => true;
binaries.awsOrThrow = () => '/path/to/fake-aws.sh';
```
The stub should print `Starting session with SessionId: …` and `Port <n> opened …` to be
treated as open. Vary it to reproduce specific states: a child that binds the port and
ignores SIGTERM reproduces the **orphan**; a process that stays alive without binding
reproduces the **zombie**; exiting non-zero after a delay reproduces a **drop**.

### Things that cannot be patched

`electron.Notification` and `electron.Tray` are non-writable, non-configurable exports, and
`notify.js`'s internal `show` is unreachable from outside. Wrap the functions `main.js` calls
on the `notify` module object instead. Similarly, a harness that `require`s `main.js` will
silently do nothing if another instance holds the single-instance lock — kill the dev app
first, and check `app.hasSingleInstanceLock()` when a probe reports suspiciously little.

Clean up any stray listeners/processes a harness leaves behind (`lsof -nP -iTCP:<port>`), or
the next run inherits them and reports nonsense.

The builds are unsigned (`build.mac.identity: null`), so first launch is blocked by
Gatekeeper and SmartScreen. See README for the bypass and the signing env vars.

## Credential handling when debugging

Pasted STS credentials are live secrets. If you write a block to a file to reproduce a
failure, delete it afterwards and confirm nothing landed in the source tree; then tell the
user to rotate, since the values persist in the transcript.
