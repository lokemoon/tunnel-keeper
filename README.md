# Tunnel Keeper

Small Electron utility for opening AWS SSM port-forwarding tunnels without touching the terminal.

## Install

Grab the installer for your platform from `dist/`:

| Platform | File | Notes |
| --- | --- | --- |
| macOS | `Tunnel Keeper-<version>-macOS-arm64.dmg` | Apple Silicon (M1 and later) |
| macOS | `Tunnel Keeper-<version>-macOS-x64.dmg` | Intel Macs |
| Windows | `Tunnel Keeper-<version>-Windows-x64-Setup.exe` | Per-user install, choose your own directory |
| Windows | `Tunnel Keeper-<version>-Windows-x64-Portable.exe` | No install, just run it |
| Windows | `Tunnel Keeper-<version>-Windows-arm64-Setup.exe` / `-Portable.exe` | The same, for Windows on ARM |

### Prerequisites on the target machine

The app drives the real AWS CLI, so both of these must be installed:

- **AWS CLI v2** — `brew install awscli`, or the AWSCLIV2 MSI on Windows
- **session-manager-plugin** — [install guide](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html)

The app locates them itself, including in Homebrew and `Program Files` locations that a
desktop-launched app does not normally have on its `PATH`. If either is missing you get a
message naming which one and how to install it, not a cryptic failure.

### The builds are unsigned

There is no Apple Developer ID or Windows code-signing certificate wired in, so:

- **macOS** — Gatekeeper blocks first launch. Right-click the app → **Open** → **Open**,
  or `xattr -dr com.apple.quarantine "/Applications/Tunnel Keeper.app"`.
- **Windows** — SmartScreen shows "unrecognised app". Click **More info** → **Run anyway**.

To sign properly, set `CSC_LINK`/`CSC_KEY_PASSWORD` (macOS) or `WIN_CSC_LINK`/
`WIN_CSC_KEY_PASSWORD` (Windows) and remove `"identity": null` from `build.mac`.

## Build from source

```sh
npm install
npm start          # run in development
npm run dist:mac   # macOS .dmg per chip (arm64, x64)
npm run dist:win   # Windows .exe installer + portable
npm run dist:all   # both
```

Windows installers build fine on macOS (no Wine needed). macOS `.dmg` builds must run on
a Mac.

### Size

Installers are built per OS and per chip, so each download carries only one copy of
Chromium: ~89 MB (macOS Apple Silicon), ~93 MB (macOS Intel), ~73 MB (Windows x64), ~76 MB
(Windows ARM). Almost all of that is Chromium — the app's own code is under 100 KB.
`build/afterPack.js` strips the ~55 unused Electron locale files and `compression: "maximum"`
is enabled. Beyond that, meaningful reduction would mean not shipping Chromium at all.

## Use

There are two routes, side by side. Pick a profile if `~/.aws` already has what you need;
paste credentials otherwise.

### Route A — a profile from `~/.aws`

The dropdown lists every profile the CLI would recognise, merged from `~/.aws/config` and
`~/.aws/credentials`, each annotated with its region and how it gets credentials (SSO,
assumed role, credential process, static keys). Profiles that only set a region are marked
`no credentials configured` and the button is disabled, so you cannot be led into a failure.

In this mode **the app never holds a secret at all** — it sets `AWS_PROFILE` and the CLI
resolves the credentials. Two consequences worth knowing:

- **SSO sessions refresh themselves.** The CLI renews them; pasted credentials can never do
  that.
- **You get a real expiry countdown.** `aws configure export-credentials` reports the
  expiry for a resolved profile, so the clock is genuine rather than "not stated by AWS".
  Only the expiry is read from that output; the keys in it are discarded.

The chosen profile is remembered for next launch, and the profile's own region is used unless
you override it.

### Route B — paste temporary STS credentials

Unchanged. Any of these work — including all three tabs the AWS access-portal hands out:
   - macOS/Linux: `export AWS_ACCESS_KEY_ID="…"` (quoted, single-quoted, bare, or `;`-terminated)
   - Windows cmd: `set AWS_ACCESS_KEY_ID=…`
   - PowerShell: `$env:AWS_ACCESS_KEY_ID="…"`
   - bare `AWS_ACCESS_KEY_ID=…` lines, or an ini block (`aws_access_key_id = …`)
   - `aws sts assume-role` / `get-session-token` JSON output
   - `credential_process` JSON output

The app verifies them with `sts get-caller-identity` and shows account, username, ARN,
region, and an expiry countdown. The pasted text is cleared from the input the moment it is
consumed; only a redacted key (`****JSON`) is ever displayed again.

### Then, whichever route you used

1. **Pick an instance.** The list is `ssm describe-instance-information` (online only)
   joined with EC2 metadata, so every target offered is actually reachable. Windows hosts
   pre-select RDP, Linux hosts pre-select SSH.

2. **Pick a service.** MSSQL (1433), RDP (3389), SSH (22), or a custom remote port.
   Leave **local port** blank to auto-allocate a free one.

   **A local port belongs to its tunnel card until you dismiss it.** While a card is listed
   — open, stopped, or disconnected — no new tunnel can take that port, because Reconnect
   reuses that exact port and two cards sharing an address would make it ambiguous. Typing a
   claimed port shows why and disables **Start**:

   - open: `In use by db-server below — stop it, or pick another port.`
   - stopped or dropped: `Reserved by db-server below — reconnect or dismiss it to free this port.`

   Dismiss the card and the port is immediately available again. Auto-allocation skips
   claimed ports too.

   **Forwarding through a jump host.** Tag an instance `SSMJumpHost=true` and it is marked
   `jump host` in the list, enabling **Forward to a remote host**. Tick it to forward
   `localhost:<local>` through that instance to `<remote host>:<remote port>` — e.g. a
   private EKS API endpoint on 443 (the console URL can be pasted as-is). This is
   `AWS-StartPortForwardingSessionToRemoteHost`. Tick **Use a different profile** to run that
   one tunnel as another `~/.aws/config` profile (e.g. a role allowed to start sessions on the
   jump host); otherwise it follows the loaded credentials. The app does not assume roles itself.

3. **Start.** Each tunnel shows `localhost:<port> → <instance>:<remote>`, its live status,
   session output, and a copy-address button. Stop them individually, or quit the app —
   everything is torn down on exit.

## Tray, notifications, and remembered settings

- **The region is remembered** between launches in `settings.json` under the OS per-user app
  data directory (`~/Library/Application Support/Tunnel Keeper` on macOS,
  `%APPDATA%\Tunnel Keeper` on Windows). A region in the pasted block still wins; the
  remembered one only fills the gap. Only preferences are stored — never credentials.
- **A tray icon shows connection status at a glance**, colour-coded: grey not signed in,
  blue ready, amber starting, expiring, or offline, green tunnels open, red dropped or expired.
  Its menu lists live tunnels with copy-address and stop, plus Stop all. On macOS the count
  of open tunnels appears beside the icon.
- **Closing the window does not quit.** Tunnels keep running in the tray; quit explicitly
  from the tray menu. Only one instance runs at a time.
- **You get a desktop notification if a tunnel drops on its own**, naming the instance, the
  local port, and the reason from the session output. Stopping a tunnel yourself is silent.
  Credentials expiring also stops tunnels and notifies.
- **Going offline and coming back are announced too** — `AWS connection lost` with the reason
  and how many tunnels stopped working, then `AWS connection restored`. Two rules keep these
  quiet rather than annoying:
  - **Only when you cannot already see it.** If the window is the active window, nothing is
    sent: the badge and banner are already telling you, and a toast on top would be
    duplicating the app you are looking at. Hidden to tray, minimised, or behind another app
    all count as background.
  - **Only once the state has held for 4 seconds.** A brief blip flips the badge but sends no
    notification at all, so a flaky connection cannot produce pairs of lost/restored toasts.

  The state on load is adopted silently, so opening the app never announces anything.

## Reconnecting

A dropped tunnel keeps its card, with **Reconnect** on it and in the tray menu.

**Reconnect always reuses the same local port**, so whatever you configured against
`localhost:<port>` — a saved SSMS connection, an RDP profile, an `ssh -p` command — keeps
working. It never silently moves to a different port.

### When the port is already taken

This applies when **starting** a tunnel on a port you typed, and when **reconnecting** —
both use the same rules and the same wording.

The AWS CLI spawns `session-manager-plugin`, and *that* is the process which binds your
local port. It can outlive its parent, so a stopped tunnel could leave an orphan squatting
on the port — which is what made a same-port reconnect fail with
`already in use by session-m (pid 43810)`.

Two fixes. Tunnels now start in their own process group and are torn down as a group
(`taskkill /T` on Windows), so the plugin cannot be orphaned in the first place. And every
process the app spawns is recorded in a registry, which gives a precise ownership test:

Both IPv4 and IPv6 are checked. The two loopbacks are separate address spaces, so something
on `[::1]:5985` does not block `127.0.0.1:5985` — and because `localhost` usually resolves to
`::1` first, a tunnel sharing a port with another IPv6 listener would silently send your client
to the wrong process. Every listener is reported with its stack, e.g.
`Local port 5985 is already in use by Python (pid 34318, IPv6).`

| Port held by | What happens |
| --- | --- |
| a process **this app started** (recorded pid) | stopped automatically, reconnects, no prompting |
| **anything else** | **asks first**, naming the process and pid, then stops it only if you approve |
| a process owned by **another user** | refuses, and says it cannot stop it — free the port yourself |
| Tunnel Keeper itself | always refused; the app will not signal itself |

When something is stopped to free the port, the status line says so — for example
`Stopped session-manager-plugin (pid 43810) (a leftover from an earlier tunnel) to free the
port.` — rather than doing it invisibly.

Ownership is deliberately **pid-exact**. A `session-manager-plugin` you started in your own
terminal has the same name and may sit on the same port, but it is not ours, so it still
prompts. The prompt appears inline on the tunnel card, and as a native dialog when the
request came from the tray or from an automatic retry — auto-reconnect never kills a process
it did not start without asking, and asks once per drop rather than once per attempt.

Process names are read with `lsof +c 0` on macOS and `netstat -ano` + `tasklist` on Windows,
so you see `session-manager-plugin` rather than `lsof`'s 9-character `session-m`.

**Auto-reconnect** is a per-tunnel toggle, off by default, on the card and in the tray
submenu. Reopening a session to a production host unattended should be a deliberate choice.
When on:

- up to **3 attempts**, backing off 2s → 5s → 10s, with a live countdown on the card
- **authorisation failures are never retried** (expired token, access denied) — only you can
  fix those, so it says so once instead of failing three times
- a session must stay up **10 seconds** to count as recovered, so a flapping tunnel
  exhausts its retries instead of reconnecting forever
- notifications follow the outcome, not the noise: with auto on you are told when it
  **reconnected** or when it **gave up**, not about each individual drop
- stopping a tunnel yourself clears the toggle, and turning it off mid-countdown cancels the
  pending attempt

## Staying honest over long sessions

The app leaves tunnels and credentials running for as long as you like, so it re-checks
rather than trusting what it was last told. A laptop that sleeps, locks, or changes network
will silently invalidate both.

**Credentials are re-verified** with `sts get-caller-identity` every 2 minutes, and
immediately on resume, unlock, and when the network comes back. If AWS answers
`ExpiredToken`, tunnels are stopped and you get a notification — you do not have to click
something to find out.

**How quickly Offline appears**, depending on how the connection breaks:

| What happened | Shows Offline after |
| --- | --- |
| Wi-Fi off, cable pulled — the OS reports it | **~3 seconds** (network state is polled every 3s) |
| DNS or routing broken, VPN down — a check fails outright | **~1 second** after that check, and checks run every 2 min |
| AWS silently unreachable, interface still "up" (captive portal) | **2–4 minutes**, via the staleness fallback |

A failed check is treated as evidence in itself, so the app does not wait out the staleness
window. It is never treated as *expiry* though — only AWS saying `ExpiredToken` does that, and
an `AccessDenied` reply still counts as Online, because AWS clearly answered. Coming back
online requires a check to actually **succeed**; the network interface returning is not enough
on its own.

The badge says **Online** only when a remote check has actually succeeded recently — it is a
statement about right now, not about what the app was told at load time:

| Badge | Meaning |
| --- | --- |
| **Online** | AWS answered and accepted the credentials within the last 11 minutes |
| **Expiring** | online, with under 5 minutes left on a known expiry |
| **Offline** | AWS has not confirmed them recently, or not at all — unreachable network, VPN down, or the credentials are already dead |
| **Expired** | AWS rejected them outright |

**When credentials are not Online, every tunnel shows Offline.** A tunnel's SSM session runs
over the same connection to AWS, so it cannot be carrying traffic while that connection is
unconfirmed — reporting "Open" would be a false claim. The tunnels list explains why, the tray
tooltip reads `Offline — 1 tunnel not working`, and each tray entry is marked `(offline)`.
Nothing is torn down: the moment a check succeeds again, tunnels go back to showing their real
state, and any that genuinely died are caught by the health check.

**Tunnels are health-checked every minute**, and on the same wake events. A tunnel can stop
working while its process keeps running — after a sleep the SSM session dies server-side but
the local plugin may sit there holding the port, so nothing exits and nothing would otherwise
notice. Three signals are used, cheapest first:

1. is our process still alive
2. is anything still listening on the local port
3. is the port still held by *our* process rather than something else

plus, when the role is permitted `ssm:DescribeSessions`, AWS's own view of whether the
session is still active — the only way to catch a healthy plugin over a dead session. If that
permission is missing the check disables itself quietly.

A tunnel found dead is treated exactly like any other unexpected drop: leftover processes are
cleaned up, you are notified with what was detected, and auto-reconnect takes over if enabled.

## About the expiry countdown

**No AWS API reports a credential's expiry** — `sts get-caller-identity` returns only
`UserId`, `Account`, and `Arn`, and the session token is an opaque encrypted blob. So the
app shows one of three honest states rather than a fabricated clock:

- **a live countdown**, when the paste itself stated an expiry
- **`not stated by AWS · loaded 12m ago`**, when it cannot be known
- **`expired (AWS rejected it)`**, once a real call comes back `ExpiredToken` — the only
  authoritative signal that exists

To get a real countdown, paste output that includes the expiry:

```sh
aws configure export-credentials --format env   # emits AWS_CREDENTIAL_EXPIRATION
aws sts assume-role ... --output json            # Credentials.Expiration
```

## Credential handling

- Secrets live only in the main process's memory, in `src/creds.js`.
- Nothing is written to disk, to a profile, or to a log.
- Secrets never cross IPC to the renderer; the renderer receives only the redacted summary.
- They are passed to `aws` subprocesses via the environment, never as argv.
- "Clear credentials" scrubs the values and stops every active tunnel.
- Expired credentials block new tunnels.

## Layout

| File | Role |
| --- | --- |
| `src/main.js` | window, 1s state broadcast, renderer error forwarding |
| `src/ipc.js` | IPC handlers, all returning `{ok, data|error}` |
| `src/creds.js` | parsing, in-memory storage, redaction, expiry |
| `src/aws.js` | AWS CLI wrapper, caller identity, instance list |
| `src/tunnels.js` | tunnel lifecycle, free-port allocation |
| `src/binaries.js` | locates `aws` / `session-manager-plugin` outside the GUI `PATH` |
| `src/preload.js` | context-isolated bridge; every call resolves, never rejects |
| `src/renderer/` | UI |
