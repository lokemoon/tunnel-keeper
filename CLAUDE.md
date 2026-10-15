# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
npm start          # tauri dev — debug build with live reload of src/renderer
npm test           # cargo test: pure logic + tunnel lifecycle against tests/fake-aws
npm run dist       # release build for this machine
npm run dist:mac   # macOS .dmg per chip: arm64 and x64 (run on a Mac)
npm run dist:win   # Windows NSIS installer per chip: x64 and arm64 (run on Windows)
npm run icons      # regenerate src-tauri/icons from build/icon.svg (the source of truth)
```

Needs a Rust toolchain (rustup) plus Node for the Tauri CLI. Cross-building Windows from
macOS is not set up; CI builds each OS on its own runner.

## What this is

A Tauri 2 app (Rust core, plain-JS webview) that shells out to the real **AWS CLI v2** to open SSM
port-forwarding tunnels. It does not use the AWS SDK; every AWS interaction is an
`execFile`/`spawn` of the `aws` binary. AWS CLI v2 and `session-manager-plugin` must be
installed on the machine — they are not bundled.

## Module map

Rust core in `src-tauri/src/`, one module per former Electron main-process file, roughly
outermost to innermost:

| File | Responsibility |
| --- | --- |
| `lib.rs` | window + tray lifetime, 1s broadcast, sleep/network watching, confirm dialogs (was `main.js`) |
| `ipc.rs` | the single `call(channel, args)` command, each channel returning `{ok, data\|error}` |
| `health.rs` | the supervisor — re-verifies credentials, inspects tunnels for zombies |
| `connectivity.rs` | online↔offline transition notifications (dependency-injected, tested) |
| `creds.rs` | credential state, redaction, expiry, `online` computation, subprocess `env()` |
| `aws.rs` | AWS CLI wrapper, error classification (`Error.kind`) and message mapping |
| `aws_config.rs` | parses `~/.aws/config` + `credentials` into a profile list (non-secret fields only) |
| `tunnels.rs` | tunnel records and lifecycle: start, reconnect, retry, sweep, port claims |
| `ports.rs` | free-port allocation, in-use checks, who-holds-this-port conflict reports |
| `procs.rs` | process discovery and termination (`pgrep`/CIM, SIGTERM→SIGKILL, `taskkill /T`) |
| `owned.rs` | registry of pids this app spawned — the ownership authority |
| `binaries.rs` | locates `aws`/`session-manager-plugin` outside the GUI `PATH`; `command()` for all spawns |
| `tray.rs` / `tray_icon.rs` | tray state and menu / RGBA icons generated at runtime |
| `notify.rs` | desktop notifications |
| `settings.rs` | persisted preferences (region, last profile) — never credentials |
| `sysevents.rs` | native network/sleep/unlock notifications, polling fallback, AWS reachability probe |
| `error.rs` | the one `Error` type: message, `kind`, optional port `conflict` |
| `tests.rs` | `cargo test` suite |

Renderer is `src/renderer/{index.html,bridge.js,renderer.js,styles.css}` only. `bridge.js`
recreates the Electron preload's `window.hub` on Tauri IPC, so `renderer.js` is unchanged.

The JS file names used in the sections below map one to one onto these modules
(`main.js` → `lib.rs`, `tunnels.js` → `tunnels.rs`, and so on).

## Architecture

Tauri 2 with no bundler and no frontend framework; `frontendDist` is `src/renderer` as-is. `src/renderer/renderer.js` is a classic
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

**Every IPC channel returns `{ok, data|error}`.** There is one Tauri command, `call`, and
`ipc.rs` dispatches on the channel name; it never returns a Tauri-level error, so a failure
is always `{ok: false, error: message}`. `bridge.js` additionally guarantees calls **resolve
rather than reject** even when the command is missing or the core fails — a rejected invoke
used to leave the UI stuck on a "working…" message forever. Preserve both properties when
adding channels: add a match arm in `ipc.rs` and a method on `hub` in `bridge.js`.

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

**Roles come from profiles, not from the app.** In-app role assumption was removed in 0.1.1:
a profile with `role_arn` already makes the CLI assume the role on every call, so the app holds
only one credential set. Do not reintroduce a second layer. `explain()` still maps
`sts:AssumeRole` denials and MFA errors, because a role profile surfaces them through the CLI;
MFA cannot be prompted for here, so the message points at caching the session in a terminal.

**The remote-host option is opt-in per instance via the tag `SSMJumpHost=true`.**
`aws.listInstances()` sets `jumpHost` from it; the renderer's "Forward to a remote host"
checkbox is disabled (and cleared) for any other instance, and `remoteHost` is sent only when it
is ticked. With a host, `launch()` uses
`AWS-StartPortForwardingSessionToRemoteHost`; `remoteHost` lives on the record so Reconnect
keeps it. `tunnels.normalizeHost()` validates shape only — reachability from the jump host is
learned from the session.

**A tunnel may run as its own profile (`meta.sessionProfile`).** Ticking "Use a different
profile" picks any `~/.aws/config` profile for that one tunnel (choosing the loaded profile
sends nothing). `launch()` spawns it with `creds.envForProfile()`, and because it is not the
loaded identity it is exempt from everything keyed to that identity: `meta.principal` is null
(so `reviewCredentials()` leaves it alone), `suspendForExpiry()` skips it, and
`health.serverSideDead()` skips it — the loaded identity cannot see another profile's
sessions and would declare every one dead.

**`settings.js` is for preferences only, never credentials.** It persists to the Electron
`userData` directory and re-validates whatever it reads back off disk.

**The tray owns the app's lifetime.** The window's `CloseRequested` is intercepted to hide,
so tunnels survive closing the window and no window is ever actually closed. Any
`RunEvent::ExitRequested` is therefore a deliberate quit (tray Quit, Cmd+Q, logout) and gets
the teardown: `stop_all()` and `creds::clear()`. `tray::update()` diffs before rebuilding the
menu, because rebuilding on the 1s tick closes the menu while the user is in it.

**Locking in the core.** Module state lives in `static Mutex`es (std, not tokio). Never hold
one across an `.await`, and never while calling `notify()`/`emit()` — the broadcast re-enters
`tunnels::list()` and deadlocks. The `with(id, |t| …)` helper in `tunnels.rs` exists for this.

**State must be verified, not assumed — `health.js` exists because it wasn't.** Two bugs came
from trusting last-known state: the badge read "Active" for 21 hours because `expired` is
false whenever expiry is unknown and nothing ever re-checked, and tunnels read "Open" after a
sleep because death was detected *only* by `proc.on('exit')`. Credentials are re-verified on
an interval and on resume/unlock/network change. `sysevents.rs` gets those from the OS
(macOS: SystemConfiguration for network paths — which is what catches a VPN going up or down
while Wi-Fi stays connected — IOKit for wake, the `com.apple.screenIsUnlocked` distributed
notification; Windows: IP Helper change callbacks, `PowerRegisterSuspendResumeNotification`, and
`WM_WTSSESSION_CHANGE` via a subclass of our own window), keeps the 3s interface poll and the
late-timer sleep check as a fallback, and debounces bursts into one `health::wake()`); tunnels are inspected for process liveness, a listening port, and continued
ownership of that port.

**Connectivity notifications live in `src/connectivity.js`, deliberately separated from
`main.js` and dependency-injected** (`isForeground`, `getState`, `onLost`, `onRestored`), because
the rules are worth testing without a window, a network, or real toasts. Two invariants: never
notify while `isForeground()` — the UI is the notification, and doubling it is the bug being
avoided — and never notify until the new state has survived `SETTLE_MS`, or a blip fires a
lost/restored pair. The first observation after credentials load is adopted silently.

Desktop notifications in Tauri have no click handler, so a toast cannot raise the window.

**Offline must be reported from evidence, not just from a timer.** Four paths set it:
the network going away (`sysevents.rs`), a health check failing with `kind == "unreachable"`
(classified in `aws.rs`), the reachability probe — two consecutive failed TCP connects to
`ssm.<region>.amazonaws.com:443`, skipped behind an HTTPS proxy — and the `VERIFY_STALE_S`
fallback. The probe may only ever report *Offline*: a successful connect never sets Online, it
only asks for the real check via `health::wake()`. Recovery requires a *successful* call — `creds.markVerified()`
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
parent.** Tunnels are therefore spawned in their own process group (`process_group(0)`) and
torn down by group, with `sweep()` killing the tunnel's recorded pids afterwards; on Windows
`taskkill /T`. Without this a stopped tunnel leaves an orphan holding the port and the next
same-port reconnect fails.

**Every launch has a generation (`gen`).** Output and exit from an older launch of the same
record are ignored, and `owned` records the gen per pid, so a sweep that fires after a prompt
reconnect kills only the old launch's processes. Without it, stop-then-Reconnect within 1.5s had
the first stop's sweep kill the new session, and a reaped zombie's late exit overwrote
`reconnecting`. Both regressions are covered in `tests.rs`.

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
| `PROBE_EVERY` / `PROBE_RETRY` | `sysevents.rs` | 20 s / 3 s | reachability probe cadence; a first failure is retried quickly |
| `PROBE_FAILS_FOR_OFFLINE` | `sysevents.rs` | 2 | one failed connect is a blip, two is evidence |
| `WAKE_DEBOUNCE_MS` | `sysevents.rs` | 3 s | OS events come in bursts; each wake forces a credential check |
| `STABLE_MS` | `tunnels.js` | 10 s | a reconnect must hold this long to count as recovered |
| `MAX_ATTEMPTS` / `BACKOFF_MS` | `tunnels.js` | 3 / 2s,5s,10s | bounded auto-reconnect |
| sweep delay | `tunnels.js` | 1.5 s | teardown escalation; slower leaves a reconnect finding its own port held |

## Constraints that have already caused bugs

**Never declare a top-level `const hub` in the renderer.** `bridge.js` installs `hub` as a
non-configurable global, as `contextBridge` did; a colliding lexical declaration throws
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

**Installers are one build per OS and chip.** `--target` picks the chip; `dist:mac` and
`dist:win` chain one build per chip. The DMG step styles its window through Finder
AppleScript, which fails without automation permission; with `CI=true` it skips that.

## How to verify a change

**`npm test` first.** `src-tauri/src/tests.rs` covers parsing, connectivity, the IPv6 port
checks, and the tunnel lifecycle end to end against `tests/fake-aws/` — a shell `aws` whose
`FAKE_MODE` (`ok`, `drop`, `orphan`) picks the behaviour, with `listen.py` binding real ports on
both loopbacks. Lifecycle steps run as one serial test because the core is process-wide state.
A "foreign" process in a test must be double-forked: a child of the test lingers as an unreaped
zombie after being killed and reads as alive.

**UI and IPC — the real app with `TK_EVAL`.** The webview cannot be driven from outside, so
debug builds (only) run the script in `TK_EVAL=<file>` inside the window 3s after launch:
```sh
PATH="$PWD/src-tauri/tests/fake-aws:$PATH" TK_EVAL=/path/ui.js npx tauri dev
```
Click real elements there and report back with
`__TAURI__.core.invoke('call', {channel: 'log:renderer', args: [msg]})`, which prints
`[renderer] …` on stderr. Renderer errors and unhandled rejections reach the same log via
`bridge.js`; **a clean log with no `[renderer]` error lines is part of passing**, since a
renderer exception silently kills every listener.

**Settings are real.** The app reads and writes the user's actual
`~/Library/Application Support/Tunnel Keeper/settings.json` (deliberately the Electron
build's file). A harness that switches profile or region overwrites it — back it up first.

Clean up any stray listeners/processes a harness leaves behind (`lsof -nP -iTCP:<port>`), or
the next run inherits them and reports nonsense. The single-instance plugin makes a second
launch just focus the first — quit the installed app before `tauri dev`.

The builds have no developer certificate. macOS is ad-hoc signed (`bundle.macOS.signingIdentity:
"-"`), which is load-bearing: with `null` Tauri leaves the bundle unsigned and a downloaded copy
is reported as **"damaged"** with no way past it, rather than "unidentified developer". See README
for the first-launch bypass on both platforms.

## Credential handling when debugging

Pasted STS credentials are live secrets. If you write a block to a file to reproduce a
failure, delete it afterwards and confirm nothing landed in the source tree; then tell the
user to rotate, since the values persist in the transcript.
