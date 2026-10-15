// Tunnel records and lifecycle: start, reconnect, retry, sweep, port claims.
//
// Locking rule: TUNNELS is a std Mutex and is never held across an `.await`, and
// never while calling notify()/emit() — both re-enter list() through the broadcast.

use crate::creds::{self, Principal};
use crate::error::{now_ms, Error, Result};
use crate::ports::{self, Conflict};
use crate::{aws_config, binaries, owned, procs};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::async_runtime::{self, JoinHandle};
use tokio::io::AsyncReadExt;

// Bounded on purpose: an endless retry loop against AWS is noise, and after three
// failures the cause is something the user has to fix.
const MAX_ATTEMPTS: u32 = 3;
const BACKOFF_MS: [u64; 3] = [2000, 5000, 10000];

// A session that opens and immediately dies is flapping, not recovered. Attempts
// only reset after it has held for this long, so a flapping tunnel still exhausts
// its retries instead of reconnecting forever.
const STABLE_MS: u64 = 10000;

// Escalate promptly: a slow teardown means a quick reconnect finds its own port
// still occupied. 1.5s is ample for a cooperative exit.
const SWEEP_DELAY_MS: u64 = 1500;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub id: String,
    pub instance_id: String,
    pub instance_name: String,
    pub label: String,
    pub remote_port: u16,
    pub remote_host: Option<String>,
    pub session_profile: Option<String>,
    pub local_port: u16,
    pub status: String,
    pub auto_reconnect: bool,
    pub attempts: u32,
    pub retry_at: Option<i64>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<String>,
    pub session_id: Option<String>,
    pub reason: Option<String>,
    pub dropped: bool,
    pub stopping: bool,
    pub asked_foreign_kill: bool,
    pub foreign_kill_approved: bool,
    pub principal: Option<Principal>,
    pub stale: bool,
    pub suspended: bool,
    pub auto_before: Option<bool>,
}

// The running process of one launch. `running` flips when the waiter reaps it.
#[derive(Clone)]
struct Proc {
    pid: u32,
    gen: u64,
    running: Arc<AtomicBool>,
}

struct Tunnel {
    meta: Meta,
    log: VecDeque<String>,
    // Absolute count of lines ever pushed; with log_mark it bounds "this launch".
    pushed: usize,
    log_mark: usize,
    proc: Option<Proc>,
    // Bumped by every launch. Output and exit from an older launch are ignored, so a
    // late exit from a reaped zombie cannot overwrite the state of its replacement.
    gen: u64,
    timer: Option<JoinHandle<()>>,
    stable_timer: Option<JoinHandle<()>>,
}

static TUNNELS: Mutex<Vec<Tunnel>> = Mutex::new(Vec::new());
static SEQ: AtomicU64 = AtomicU64::new(0);

type ChangeHook = Box<dyn Fn() + Send + Sync>;
type EventHook = Box<dyn Fn(&str, Value) + Send + Sync>;
static ON_CHANGE: Mutex<Option<Arc<ChangeHook>>> = Mutex::new(None);
static ON_EVENT: Mutex<Option<Arc<EventHook>>> = Mutex::new(None);

pub fn on_change(f: impl Fn() + Send + Sync + 'static) {
    *ON_CHANGE.lock().unwrap() = Some(Arc::new(Box::new(f)));
}

// kind: 'dropped' | 'restored' | 'gaveup' | 'confirm-kill' | 'credentials-changed'
pub fn on_event(f: impl Fn(&str, Value) + Send + Sync + 'static) {
    *ON_EVENT.lock().unwrap() = Some(Arc::new(Box::new(f)));
}

fn notify() {
    let hook = ON_CHANGE.lock().unwrap().clone();
    if let Some(f) = hook {
        f();
    }
}

fn emit(kind: &str, payload: Value) {
    let hook = ON_EVENT.lock().unwrap().clone();
    if let Some(f) = hook {
        f(kind, payload);
    }
}

fn meta_json(m: &Meta) -> Value {
    serde_json::to_value(m).unwrap_or(Value::Null)
}

fn lock() -> std::sync::MutexGuard<'static, Vec<Tunnel>> {
    TUNNELS.lock().unwrap()
}

fn find<'a>(v: &'a mut [Tunnel], id: &str) -> Option<&'a mut Tunnel> {
    v.iter_mut().find(|t| t.meta.id == id)
}

fn with<T>(id: &str, f: impl FnOnce(&mut Tunnel) -> T) -> Result<T> {
    let mut g = lock();
    let t = find(&mut g, id).ok_or_else(|| Error::msg("That tunnel no longer exists."))?;
    Ok(f(t))
}

// The plugin's chatter ends with boilerplate like "Exiting session with sessionId";
// the line before that is usually the actual cause.
static NOISE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(Exiting session|Starting session|Waiting for connections|Connection accepted|Port \d+ opened|SessionId:)")
        .unwrap()
});

// Only lines from the current launch count — see log_mark in launch().
fn last_meaningful_line(t: &Tunnel) -> Option<String> {
    let first_abs = t.pushed - t.log.len();
    for (i, line) in t.log.iter().enumerate().rev() {
        if first_abs + i < t.log_mark {
            break;
        }
        let line = line.trim();
        if !line.is_empty() && !NOISE.is_match(line) {
            return Some(line.chars().take(200).collect());
        }
    }
    None
}

// Retrying an authorisation failure just fails again three times; only the user can
// fix it, so say so once instead.
static UNRECOVERABLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)ExpiredToken|expired|AccessDenied|not authorized|UnauthorizedOperation|InvalidClientTokenId").unwrap()
});

fn push(t: &mut Tunnel, line: &str) {
    let text = line.trim_end();
    if text.is_empty() {
        return;
    }
    t.log.push_back(text.to_string());
    t.pushed += 1;
    if t.log.len() > 200 {
        t.log.pop_front();
    }
}

fn is_live(m: &Meta) -> bool {
    matches!(m.status.as_str(), "open" | "starting" | "reconnecting")
}

// A tunnel keeps its local port for as long as its card exists, whatever its status.
// A stopped or dropped tunnel is still holding a reservation the user can act on —
// Reconnect reuses that exact port — so handing the same port to a second tunnel would
// give two cards the same address and make Reconnect ambiguous. The reservation ends
// only when the card is dismissed.
fn claimant(port: u16) -> Option<Meta> {
    lock().iter().find(|t| t.meta.local_port == port).map(|t| t.meta.clone())
}

fn status_word(s: &str) -> &str {
    match s {
        "error" => "disconnected",
        other => other,
    }
}

fn claim_message(port: u16, m: &Meta) -> String {
    format!(
        "Local port {port} is already assigned to {} in Tunnels ({}). {}",
        m.instance_name,
        status_word(&m.status),
        if is_live(m) {
            "Stop that tunnel first, or use a different local port."
        } else {
            "Reconnect that tunnel to reuse it, or dismiss it to free the port."
        }
    )
}

fn clear_timer(t: &mut Tunnel) {
    if let Some(h) = t.timer.take() {
        h.abort();
    }
    if let Some(h) = t.stable_timer.take() {
        h.abort();
    }
    t.meta.retry_at = None;
}

fn guard_credentials(m: Option<&Meta>) -> Result<()> {
    if !creds::has() {
        return Err(Error::msg("Load credentials first."));
    }
    if creds::expired() {
        return Err(Error::msg("Credentials have expired. Load a fresh set, then reconnect."));
    }
    // A tunnel marked stale belongs to credentials that are no longer loaded. Retrying
    // it under a different account, role or region would open a session to a different
    // place behind the same local port, which is worse than not reconnecting at all.
    if let Some(m) = m {
        if m.session_profile.is_some() {
            return Ok(()); // runs as its own profile, not the loaded identity
        }
        if m.stale && creds::same_principal(m.principal.as_ref(), creds::principal().as_ref()) != Some(true) {
            return Err(Error::msg(stale_message(m.principal.as_ref())));
        }
    }
    Ok(())
}

fn stale_message(was: Option<&Principal>) -> String {
    format!(
        "These credentials are for {}, but this tunnel was opened with {}. Dismiss it and start a new one.",
        creds::describe_principal(creds::principal().as_ref()),
        creds::describe_principal(was)
    )
}

#[derive(Clone, Serialize)]
pub struct Cleared {
    label: String,
    pid: u32,
    ours: bool,
    count: usize,
}

// Frees the local port so a same-port reconnect can proceed.
//   ours          -> cleared without asking; it is our own orphaned plugin
//   someone       -> refused until the caller passes allow_foreign_kill
//   not ours, and unkillable -> refused with the reason
// Returns None if the port was already free, or a description of what it stopped,
// so the caller can tell the user what happened rather than doing it silently.
async fn clear_port(port: u16, allow_foreign_kill: bool) -> Result<Option<Cleared>> {
    if !ports::in_use(port) {
        return Ok(None);
    }

    let c = ports::conflict(port).await;
    if c.holders.is_empty() {
        return Err(Error::msg(format!("{} It could not be identified, so it was left alone.", c.message)));
    }

    // Never signal ourselves, whatever the registry or the user says.
    if c.holders.iter().any(|h| procs::is_self(h.pid)) {
        return Err(Error::msg(format!("{} That is Tunnel Keeper itself, so it will not be stopped.", c.message)));
    }

    if !c.ours && !allow_foreign_kill {
        return Err(Error::needs_confirmation(c));
    }

    if !c.can_terminate {
        return Err(Error::msg(format!("{} It belongs to another user, so this app cannot stop it.", c.message)));
    }

    // Every listener must go, not just the first: a port shared between an IPv4 and an
    // IPv6 listener is only actually free once both are gone.
    for h in &c.holders {
        let res = procs::terminate(h.pid, 2000).await;
        if !res.ok {
            return Err(Error::msg(format!(
                "Could not stop {}: {}",
                ports::name_of(h),
                res.error.unwrap_or_default()
            )));
        }
        owned::forget(h.pid);
    }

    // The port can linger for a moment after the processes go.
    for _ in 0..10 {
        if !ports::in_use(port) {
            return Ok(Some(Cleared {
                label: c.label.clone(),
                pid: c.holders[0].pid,
                ours: c.ours,
                count: c.holders.len(),
            }));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Err(Error::msg(format!("{} exited but local port {port} is still not free.", c.label)))
}

// The CLI's session-manager-plugin child is what binds the port, and it can outlive
// its parent. Recording it is what lets a later reconnect recognise the orphan as
// ours and clear it without bothering the user. Best-effort: failing it only means
// we prompt later.
async fn track_children(id: &str, gen: u64, pid: u32) {
    for child in procs::descendants(pid).await {
        owned::record(child, id, gen, "session-manager-plugin");
    }
}

// Re-read at every launch, so a profile removed from ~/.aws fails with a clear reason.
fn profile_env(name: &str) -> Result<creds::Env> {
    let p = aws_config::get(name).ok_or_else(|| Error::msg(format!("Profile \"{name}\" is not in ~/.aws/config.")))?;
    let region = p.region.unwrap_or_else(creds::region);
    creds::env_for_profile(name, Some(&region))
}

static OPENED: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)port \[?\d+\]? opened|Waiting for connections").unwrap());
static SESSION_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)SessionId:\s*([\w@.\-]+)").unwrap());

// Launches (or relaunches) the CLI for an existing record. The record's identity,
// local port and auto-reconnect toggle all survive a reconnect.
fn launch(id: &str) -> Result<()> {
    if !binaries::has_session_manager_plugin() {
        return Err(Error::msg(
            "session-manager-plugin is not installed. AWS SSM tunnels require it — install it from AWS, then reopen this app.",
        ));
    }
    let m = with(id, |t| t.meta.clone())?;

    // With a remote host the instance is only a hop (a jump host), and the session
    // forwards on to host:port from inside the VPC — e.g. a private EKS endpoint.
    // JSON parameters, since the shorthand syntax would split a value on commas.
    let mut params = json!({ "portNumber": [m.remote_port.to_string()], "localPortNumber": [m.local_port.to_string()] });
    if let Some(h) = &m.remote_host {
        params["host"] = json!([h]);
    }
    let params = params.to_string();
    let doc = if m.remote_host.is_some() {
        "AWS-StartPortForwardingSessionToRemoteHost"
    } else {
        "AWS-StartPortForwardingSession"
    };
    let env = match &m.session_profile {
        Some(p) => profile_env(p)?,
        None => creds::env()?,
    };

    let mut cmd = binaries::command(binaries::aws_or_throw()?);
    cmd.args(["ssm", "start-session", "--target", &m.instance_id, "--document-name", doc, "--parameters", &params])
        .env_clear()
        .envs(&env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Its own process group, so stopping the tunnel can take the whole group down
    // and not leave session-manager-plugin orphaned on the port. Windows has no
    // process groups here; taskkill /T handles the tree instead.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| Error::msg(e.to_string()))?;
    let pid = child.id().unwrap_or(0);
    let running = Arc::new(AtomicBool::new(true));

    // Which credentials this session was opened with, so a later load can tell a
    // renewal of the same access from a switch to a different account, role or region.
    // A tunnel on its own profile is not tied to the loaded credentials at all, so it
    // has no principal to compare: None makes review_credentials leave it alone.
    let principal = if m.session_profile.is_some() { None } else { creds::principal() };

    let gen = with(id, |t| {
        t.gen += 1;
        t.proc = Some(Proc { pid, gen: t.gen, running: running.clone() });
        t.meta.status = "starting".into();
        t.meta.stopping = false;
        t.meta.dropped = false;
        t.meta.asked_foreign_kill = false;
        t.meta.started_at = Some(now_ms());

        // Everything below belongs to the session about to start, not to the record that
        // outlives it. session_id especially: it is only captured while empty, so keeping
        // the previous session's id here makes health check a live session against a
        // dead id, declare a perfectly healthy tunnel dead every minute, and reconnect
        // forever — which is why only records that had already reconnected once flapped.
        t.meta.session_id = None;
        t.meta.reason = None;
        // The log also survives reconnects, so only lines produced by this launch may be
        // used to explain this launch's death. Otherwise an ExpiredToken line from before
        // a credential renewal is read back as the new failure and retrying is abandoned.
        t.log_mark = t.pushed;
        t.meta.principal = principal;
        t.meta.stale = false;
        t.meta.suspended = false;
        t.gen
    })?;

    owned::record(pid, id, gen, "aws");
    // The plugin appears shortly after the CLI starts.
    {
        let id = id.to_string();
        async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            track_children(&id, gen, pid).await;
            tokio::time::sleep(Duration::from_millis(1700)).await;
            track_children(&id, gen, pid).await;
        });
    }

    if let Some(out) = child.stdout.take() {
        watch(id.to_string(), gen, out);
    }
    if let Some(err) = child.stderr.take() {
        watch(id.to_string(), gen, err);
    }

    let id_owned = id.to_string();
    async_runtime::spawn(async move {
        let status = child.wait().await;
        running.store(false, Ordering::SeqCst);
        let (code, signal) = match &status {
            Ok(s) => (s.code(), exit_signal(s)),
            Err(_) => (None, None),
        };
        if let Err(e) = &status {
            let _ = with(&id_owned, |t| push(t, &e.to_string()));
        }
        on_exit(&id_owned, gen, code, signal);
    });
    Ok(())
}

#[cfg(unix)]
fn exit_signal(s: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    s.signal().map(|n| match n {
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGINT => "SIGINT".into(),
        libc::SIGHUP => "SIGHUP".into(),
        other => format!("SIG{other}"),
    })
}

#[cfg(not(unix))]
fn exit_signal(_s: &std::process::ExitStatus) -> Option<String> {
    None
}

fn watch(id: String, gen: u64, mut stream: impl tokio::io::AsyncRead + Unpin + Send + 'static) {
    async_runtime::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
            on_output(&id, gen, &chunk);
        }
    });
}

fn on_output(id: &str, gen: u64, chunk: &str) {
    let found = with(id, |t| {
        if t.gen != gen {
            return None;
        }
        for line in chunk.split('\n') {
            push(t, line);
        }
        // Captured so a health check can ask AWS whether this session is still alive
        // — the only way to catch a plugin that is still running over a dead session.
        if t.meta.session_id.is_none() {
            if let Some(m) = SESSION_ID.captures(chunk) {
                t.meta.session_id = Some(m[1].to_string());
            }
        }
        // The plugin prints this once the local listener is actually accepting.
        let mut opened_via = None;
        if t.meta.status == "starting" && OPENED.is_match(chunk) {
            t.meta.status = "open".into();
            opened_via = t.meta.remote_host.clone();
            // Only declare recovery once it has actually held, so a flapping session
            // cannot reset the retry budget on every cycle.
            if t.meta.attempts > 0 && t.stable_timer.is_none() {
                let id = id.to_string();
                t.stable_timer = Some(async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(STABLE_MS)).await;
                    let restored = with(&id, |t| {
                        t.stable_timer = None;
                        if t.meta.status != "open" {
                            return None;
                        }
                        let snap = meta_json(&t.meta);
                        t.meta.attempts = 0;
                        Some(snap)
                    });
                    if let Ok(Some(snap)) = restored {
                        emit("restored", snap);
                        notify();
                    }
                }));
            }
        }
        Some(opened_via)
    });
    // Written outside the lock: it touches the disk.
    if let Ok(Some(Some(host))) = &found {
        crate::settings::remember_remote_host(host);
    }
    if matches!(found, Ok(Some(_))) {
        notify();
    }
}

enum AfterExit {
    Nothing,
    Retry,
    Dropped(Value),
}

fn on_exit(id: &str, gen: u64, code: Option<i32>, signal: Option<String>) {
    let after = with(id, |t| {
        // Not the current launch, or already declared dead by a health check (which
        // detached it): this exit says nothing about the record's present state.
        if t.proc.as_ref().map(|p| p.gen) != Some(gen) {
            return AfterExit::Nothing;
        }
        let manual = t.meta.stopping;
        let was_up = t.meta.status == "open" || t.meta.status == "starting";
        t.meta.exit_code = code;
        t.meta.exit_signal = signal;
        t.meta.ended_at = Some(now_ms());
        t.proc = None;
        if let Some(h) = t.stable_timer.take() {
            h.abort();
        }

        if manual {
            t.meta.status = "stopped".into();
            return AfterExit::Nothing;
        }

        // An unasked-for exit is what the user needs telling about: the local port is
        // dead and whatever was using it is about to fail. A clean exit code still
        // counts, because the CLI exits 0 when the remote end closes the session.
        if !was_up {
            t.meta.status = if code == Some(0) { "stopped" } else { "error" }.into();
            return AfterExit::Nothing;
        }

        t.meta.dropped = true;
        t.meta.reason = Some(last_meaningful_line(t).unwrap_or_else(|| "The SSM session ended unexpectedly.".into()));

        if t.meta.auto_reconnect {
            AfterExit::Retry
        } else {
            t.meta.status = "error".into();
            AfterExit::Dropped(meta_json(&t.meta))
        }
    });
    match after {
        Ok(AfterExit::Retry) => schedule_retry(id),
        Ok(AfterExit::Dropped(m)) => emit("dropped", m),
        _ => {}
    }
    notify();
}

fn fail_retry(id: &str, why: String) {
    let snap = with(id, |t| {
        t.meta.status = "error".into();
        t.meta.reason = Some(why);
        clear_timer(t);
        meta_json(&t.meta)
    });
    if let Ok(m) = snap {
        emit("gaveup", m);
    }
    notify();
}

fn schedule_retry(id: &str) {
    enum Plan {
        Fail(String),
        Wait(u64),
    }
    let plan = with(id, |t| {
        // CLI output rarely ends in punctuation, so don't run two sentences together.
        let because = t.meta.reason.clone().unwrap_or_default();
        let because = because.trim_end_matches(|c: char| c == '.' || c.is_whitespace()).to_string();

        if UNRECOVERABLE.is_match(&because) {
            return Plan::Fail(format!("{because}. Reconnecting will not help until that is fixed."));
        }
        if t.meta.attempts >= MAX_ATTEMPTS {
            return Plan::Fail(format!("{because}. Gave up after {MAX_ATTEMPTS} reconnect attempts."));
        }
        let delay = BACKOFF_MS[(t.meta.attempts as usize).min(BACKOFF_MS.len() - 1)];
        t.meta.attempts += 1;
        t.meta.status = "reconnecting".into();
        t.meta.retry_at = Some(now_ms() + delay as i64);
        Plan::Wait(delay)
    });
    let delay = match plan {
        Ok(Plan::Fail(why)) => return fail_retry(id, why),
        Ok(Plan::Wait(d)) => d,
        Err(_) => return,
    };
    notify();

    let id_owned = id.to_string();
    let handle = async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let id = id_owned;
        let Ok((meta, approved)) = with(&id, |t| {
            t.timer = None;
            t.meta.retry_at = None;
            (t.meta.clone(), t.meta.foreign_kill_approved)
        }) else {
            return;
        };
        let attempt = async {
            guard_credentials(Some(&meta))?;
            // Our own orphan is cleared silently; anything else has to be asked about,
            // even on an automatic retry.
            clear_port(meta.local_port, approved).await?;
            launch(&id)
        };
        if let Err(err) = attempt.await {
            if let Some(conflict) = &err.conflict {
                // Ask once per drop, not once per attempt.
                let ask = with(&id, |t| {
                    if t.meta.asked_foreign_kill {
                        return None;
                    }
                    t.meta.asked_foreign_kill = true;
                    t.meta.status = "reconnecting".into();
                    let mut v = meta_json(&t.meta);
                    v["conflict"] = serde_json::to_value(conflict.as_ref()).unwrap_or(Value::Null);
                    Some(v)
                });
                if let Ok(Some(v)) = ask {
                    notify();
                    emit("confirm-kill", v);
                    return;
                }
            }
            // Port taken or credentials gone: both need the user, so stop retrying.
            return fail_retry(&id, err.message);
        }
        notify();
    });
    let _ = with(id, |t| t.timer = Some(handle));
}

// A DNS name, IPv4, or bare IPv6 address. Shape only: whether the jump host can
// resolve and reach it is learned from the session, as it would be on the CLI.
static HOST_DNS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?(\.[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?)*\.?$").unwrap()
});
static HOST_IP: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9A-Fa-f:.]+$").unwrap());

pub fn normalize_host(value: Option<&str>) -> Result<Option<String>> {
    let original = value.unwrap_or("").trim().to_string();
    if original.is_empty() {
        return Ok(None);
    }
    // People paste the EKS endpoint as shown in the console, scheme and all.
    static SCHEME: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^[a-z]+://").unwrap());
    let mut host = SCHEME.replace(&original, "").into_owned();
    if let Some(i) = host.find('/') {
        host.truncate(i);
    }
    if host.starts_with('[') && host.ends_with(']') && host.len() >= 2 {
        host = host[1..host.len() - 1].to_string();
    }
    let dns_ok = !host.is_empty() && host.len() <= 253 && HOST_DNS.is_match(&host);
    if !dns_ok && !HOST_IP.is_match(&host) {
        return Err(Error::msg(format!("Remote host \"{original}\" is not a hostname or IP address.")));
    }
    Ok(Some(host))
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StartOpts {
    pub instance_id: Option<String>,
    pub instance_name: Option<String>,
    pub remote_port: Option<Value>,
    pub remote_host: Option<String>,
    pub session_profile: Option<String>,
    pub local_port: Option<Value>,
    pub label: Option<String>,
    pub allow_foreign_kill: bool,
}

// Numbers arrive from the renderer as numbers or strings; empty means "not given".
fn port_value(v: &Option<Value>) -> Option<std::result::Result<u16, ()>> {
    let n: f64 = match v {
        None | Some(Value::Null) => return None,
        Some(Value::Number(n)) => n.as_f64()?,
        Some(Value::String(s)) if s.trim().is_empty() => return None,
        Some(Value::String(s)) => s.trim().parse().unwrap_or(f64::NAN),
        Some(Value::Bool(false)) => return None,
        _ => f64::NAN,
    };
    if n == 0.0 {
        return None; // JS treated 0 as "auto-allocate"
    }
    Some(if n.fract() == 0.0 && (1.0..=65535.0).contains(&n) { Ok(n as u16) } else { Err(()) })
}

pub async fn start(o: StartOpts) -> Result<Value> {
    guard_credentials(None)?;
    let instance_id = o.instance_id.filter(|s| !s.is_empty()).ok_or_else(|| Error::msg("Pick an instance."))?;
    let host = normalize_host(o.remote_host.as_deref())?;
    let profile = o.session_profile.filter(|s| !s.is_empty());
    if let Some(p) = &profile {
        profile_env(p)?; // validates before any port is touched
    }

    let remote = match port_value(&o.remote_port) {
        Some(Ok(p)) => p,
        _ => return Err(Error::msg("Remote port must be 1-65535.")),
    };

    let mut cleared = None;
    let local = match port_value(&o.local_port) {
        Some(Err(())) => return Err(Error::msg("Local port must be 1-65535.")),
        Some(Ok(local)) => {
            // A port still listed in Tunnels belongs to that card until it is dismissed.
            // Checked before touching the OS, so no process is killed for a port that was
            // never going to be granted.
            if let Some(held) = claimant(local) {
                return Err(Error::msg(claim_message(local, &held)));
            }
            // Same rules as a reconnect: our own leftovers are cleared automatically, and
            // anything else is asked about first.
            cleared = clear_port(local, o.allow_foreign_kill).await?;
            local
        }
        None => {
            // Auto-allocation must also respect existing cards, or it could hand out a port
            // that a stopped tunnel is still holding for its Reconnect.
            let mut found = None;
            for _ in 0..20 {
                let candidate = ports::find_free_port()?;
                if claimant(candidate).is_none() {
                    found = Some(candidate);
                    break;
                }
            }
            found.ok_or_else(|| {
                Error::msg("Could not find a free local port. Dismiss some stopped tunnels and try again.")
            })?
        }
    };

    let id = format!("t{}", SEQ.fetch_add(1, Ordering::SeqCst) + 1);
    let instance_name = o.instance_name.filter(|s| !s.is_empty()).unwrap_or_else(|| instance_id.clone());
    let label = o.label.filter(|s| !s.is_empty()).unwrap_or_else(|| {
        format!("{}:{remote}", host.clone().unwrap_or_else(|| instance_name.clone()))
    });
    let meta = Meta {
        id: id.clone(),
        instance_id,
        instance_name,
        label,
        remote_port: remote,
        remote_host: host,
        session_profile: profile,
        local_port: local,
        status: "starting".into(),
        started_at: Some(now_ms()),
        ..Default::default()
    };
    lock().push(Tunnel {
        meta,
        log: VecDeque::new(),
        pushed: 0,
        log_mark: 0,
        proc: None,
        gen: 0,
        timer: None,
        stable_timer: None,
    });

    if let Err(e) = launch(&id) {
        lock().retain(|t| t.meta.id != id);
        return Err(e);
    }

    notify();
    with_cleared(&id, cleared)
}

fn with_cleared(id: &str, cleared: Option<Cleared>) -> Result<Value> {
    let mut v = with(id, |t| meta_json(&t.meta))?;
    v["cleared"] = serde_json::to_value(cleared).unwrap_or(Value::Null);
    Ok(v)
}

// Manual reconnect: same instance, same remote port, same local port. Never
// silently moves to a different port.
//
// If our own orphaned plugin is squatting on the port it is cleared automatically.
// If something else holds it, this fails with a conflict and the caller must ask
// the user before calling again with allow_foreign_kill.
pub async fn reconnect(id: &str, allow_foreign_kill: bool) -> Result<Value> {
    let meta = with(id, |t| {
        if is_live(&t.meta) && t.meta.status != "reconnecting" {
            return Err(Error::msg("That tunnel is already running."));
        }
        clear_timer(t);
        Ok(t.meta.clone())
    })??;
    guard_credentials(Some(&meta))?;
    let cleared = clear_port(meta.local_port, allow_foreign_kill).await?;

    with(id, |t| {
        t.meta.attempts = 0;
        t.meta.reason = None;
    })?;
    launch(id)?;
    notify();
    with_cleared(id, cleared)
}

// Declares a tunnel dead from outside its own exit handler.
//
// A tunnel can stop working while its process keeps running: after the machine
// sleeps or the network drops, the SSM session dies server-side but the local
// plugin may sit there holding the port. Nothing exits, so the exit handler never
// fires and the UI would keep claiming "Open" indefinitely. Health checks call this
// so such a zombie is treated exactly like any other unexpected drop, including
// auto-reconnect.
pub fn report_dead(id: &str, reason: String) {
    let r = with(id, |t| {
        if !is_live(&t.meta) {
            return None;
        }
        t.meta.dropped = true;
        t.meta.reason = Some(reason);
        t.meta.ended_at = Some(now_ms());
        let proc = t.proc.take();
        let auto = t.meta.auto_reconnect;
        if !auto {
            t.meta.status = "error".into();
        }
        Some((proc, t.meta.local_port, auto, meta_json(&t.meta)))
    });
    let Ok(Some((proc, port, auto, snap))) = r else { return };

    // Clear the corpse out of the way so a reconnect finds its own port free.
    let id_owned = id.to_string();
    async_runtime::spawn(async move { sweep(&id_owned, proc, port).await });

    if auto {
        schedule_retry(id);
    } else {
        emit("dropped", snap);
    }
    notify();
}

// The user's answer to a 'confirm-kill' prompt raised by an automatic retry.
pub async fn resolve_foreign_kill(id: &str, approved: bool) -> Result<Value> {
    if !approved {
        let v = with(id, |t| {
            t.meta.status = "error".into();
            t.meta.reason = Some(format!(
                "{} Reconnect cancelled — local port {} is still in use.",
                t.meta.reason.clone().unwrap_or_else(|| "Tunnel dropped.".into()),
                t.meta.local_port
            ));
            clear_timer(t);
            meta_json(&t.meta)
        })?;
        notify();
        return Ok(v);
    }
    with(id, |t| t.meta.foreign_kill_approved = true)?;
    match reconnect(id, true).await {
        Ok(v) => {
            let _ = with(id, |t| t.meta.foreign_kill_approved = false);
            Ok(v)
        }
        Err(err) => {
            let _ = with(id, |t| {
                t.meta.foreign_kill_approved = false;
                t.meta.status = "error".into();
                t.meta.reason = Some(err.message.clone());
            });
            notify();
            Err(err)
        }
    }
}

pub fn set_auto_reconnect(id: &str, enabled: bool) -> Result<bool> {
    let v = with(id, |t| {
        t.meta.auto_reconnect = enabled;
        // Turning it off mid-countdown should cancel the pending attempt.
        if !enabled && t.meta.status == "reconnecting" {
            clear_timer(t);
            t.meta.status = "error".into();
        }
        t.meta.auto_reconnect
    })?;
    notify();
    Ok(v)
}

fn signal_stop(proc: &Proc) {
    // SIGTERM the whole group, not just the CLI: killing only the parent leaves
    // session-manager-plugin alive holding the local port, which is exactly what
    // makes a later same-port reconnect fail.
    #[cfg(unix)]
    {
        procs::kill_group(proc.pid, false);
        procs::kill_pid(proc.pid, false);
    }
    // Windows: /T takes the plugin child with the CLI.
    #[cfg(windows)]
    {
        let pid = proc.pid.to_string();
        async_runtime::spawn(async move {
            binaries::run("taskkill", &["/PID", &pid, "/T", "/F"], 5000).await;
        });
    }
}

pub fn stop(id: &str) {
    let r = with(id, |t| {
        clear_timer(t);
        t.meta.stopping = true;
        t.meta.auto_reconnect = false; // an explicit stop means stop
        t.meta.status = if t.proc.is_some() { "stopping" } else { "stopped" }.into();
        (t.proc.clone(), t.meta.local_port)
    });
    notify();
    let Ok((Some(proc), port)) = r else { return };

    signal_stop(&proc);
    let id = id.to_string();
    async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(SWEEP_DELAY_MS)).await;
        sweep(&id, Some(proc), port).await;
    });
}

// Makes certain nothing of ours is left running or holding the port. Works from the
// pids we recorded, so it catches an orphan whether or not it still has the port.
async fn sweep(id: &str, proc: Option<Proc>, port: u16) {
    if let Some(p) = &proc {
        if p.running.load(Ordering::SeqCst) {
            #[cfg(unix)]
            {
                procs::kill_group(p.pid, true);
                procs::kill_pid(p.pid, true);
            }
            #[cfg(windows)]
            {
                procs::terminate(p.pid, 0).await;
            }
        }
    }

    // A prompt reconnect may already have launched a fresh session on this record.
    // Only the swept launch's processes are fair game then — the new one's pids and
    // the port it now holds are not leftovers.
    let swept_gen = proc.as_ref().map(|p| p.gen);
    let relaunched = lock()
        .iter()
        .find(|t| t.meta.id == id)
        .and_then(|t| t.proc.as_ref().map(|p| Some(p.gen) != swept_gen))
        .unwrap_or(false);

    let pids = if relaunched {
        swept_gen.map(|g| owned::by_launch(id, g)).unwrap_or_default()
    } else {
        owned::by_tunnel(id)
    };
    for pid in pids {
        if procs::alive(pid) {
            procs::terminate(pid, 800).await;
        }
        owned::forget(pid);
    }

    // Anything of ours still on the port goes too, so the next reconnect finds it free.
    if !relaunched && ports::in_use(port) {
        let c: Conflict = ports::conflict(port).await;
        // Only our own leftovers, and all of them — a foreign listener sharing the port is
        // not ours to clean up.
        if c.ours && c.can_terminate {
            for h in &c.holders {
                procs::terminate(h.pid, 800).await;
                owned::forget(h.pid);
            }
        }
    }
    notify();
}

pub fn remove(id: &str) {
    let live = with(id, |t| is_live(&t.meta)).unwrap_or(false);
    if live {
        stop(id);
    }
    // Deliberately does NOT forget this tunnel's pids: a leftover process can outlive
    // the card, and forgetting it here would make the app prompt about its own
    // orphan. sweep() forgets each pid as it confirms it dead.
    {
        let mut g = lock();
        if let Some(t) = find(&mut g, id) {
            clear_timer(t);
        }
        g.retain(|t| t.meta.id != id);
    }
    notify();
}

pub fn stop_all() {
    let ids: Vec<String> = lock().iter().map(|t| t.meta.id.clone()).collect();
    for id in ids {
        stop(&id);
    }
}

// Credentials expired, so every SSM session died with them. The tunnels are stopped
// like any other stop — but stamped, because if the user comes back with a renewal of
// the same access these exact tunnels can be brought back, and only these.
pub fn suspend_for_expiry() {
    // Its own profile did not expire with the loaded credentials.
    let targets: Vec<(String, bool)> = lock()
        .iter()
        .filter(|t| is_live(&t.meta) && t.meta.session_profile.is_none())
        .map(|t| (t.meta.id.clone(), t.meta.auto_reconnect))
        .collect();
    for (id, auto) in targets {
        stop(&id); // clears the retry timer and turns auto-reconnect off
        let _ = with(&id, |t| {
            t.meta.suspended = true;
            t.meta.auto_before = Some(auto);
            t.meta.reason = Some("Credentials expired, so the SSM session ended.".into());
        });
    }
    notify();
}

// Called once a newly loaded set of credentials has been verified.
//
// An existing card is only worth anything if the new credentials are the same
// identity its session was opened with. A renewal still reaches the same instance
// through the same local port, so tunnels suspended by an expiry are resumed. A
// different account, role or region does not, and reconnecting there would put a
// different destination behind an unchanged local port — so those are marked and
// left alone for the user to decide about. Nothing is torn down here.
pub async fn review_credentials() {
    let now = creds::principal();
    let mut stale = Vec::new();
    let mut resumable = Vec::new();

    {
        let mut g = lock();
        for t in g.iter_mut() {
            match creds::same_principal(t.meta.principal.as_ref(), now.as_ref()) {
                Some(false) => {
                    clear_timer(t);
                    t.meta.auto_reconnect = false;
                    t.meta.suspended = false;
                    t.meta.stale = true;
                    t.meta.reason = Some(stale_message(t.meta.principal.as_ref()));
                    // A live tunnel is left running and its status untouched: its session was
                    // opened under credentials that AWS has not rejected, so claiming it is dead
                    // would be a guess. The mark and the prompt are enough.
                    if !is_live(&t.meta) {
                        t.meta.status = "error".into();
                    }
                    stale.push(meta_json(&t.meta));
                }
                Some(true) if t.meta.suspended => resumable.push(t.meta.id.clone()),
                // None: one side's identity is unknown, so neither resume nor mark.
                _ => {}
            }
        }
    }

    for id in resumable {
        let _ = with(&id, |t| t.meta.suspended = false);
        match reconnect(&id, false).await {
            Ok(_) => {
                let _ = with(&id, |t| {
                    if t.meta.auto_before == Some(true) {
                        t.meta.auto_reconnect = true;
                    }
                });
            }
            Err(err) => {
                // A port that cannot be freed is the usual cause, and it is the user's to
                // resolve — the card keeps its Reconnect button.
                let _ = with(&id, |t| {
                    t.meta.status = "error".into();
                    t.meta.reason = Some(if err.needs_confirmation_flag() {
                        format!("{} Reconnect to decide what to do about it.", err.message)
                    } else {
                        err.message.clone()
                    });
                });
            }
        }
        let _ = with(&id, |t| t.meta.auto_before = None);
    }

    notify();
    if !stale.is_empty() {
        emit("credentials-changed", json!({ "stale": stale, "principal": now }));
    }
}

// The user's answer to the prompt raised above: dismiss every card that belongs to
// credentials that are no longer loaded. Goes through remove(), so each one is
// stopped and its port released exactly as a manual dismissal would.
pub fn remove_stale() -> usize {
    let ids: Vec<String> = lock().iter().filter(|t| t.meta.stale).map(|t| t.meta.id.clone()).collect();
    for id in &ids {
        remove(id);
    }
    ids.len()
}

pub fn list_meta() -> Vec<Meta> {
    lock().iter().map(|t| t.meta.clone()).collect()
}

pub fn list() -> Value {
    Value::Array(
        lock()
            .iter()
            .map(|t| {
                let mut v = meta_json(&t.meta);
                let skip = t.log.len().saturating_sub(12);
                v["log"] = json!(t.log.iter().skip(skip).collect::<Vec<_>>());
                v
            })
            .collect(),
    )
}
