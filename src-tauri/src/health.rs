// The supervisor. Its absence once caused two wrong claims: credentials shown
// "Active" for as long as the app stayed open, and tunnels shown "Open" after the
// machine slept. Both came from trusting the last known state instead of checking.
//
// It matters most around suspend/resume and network loss, because that is exactly
// when state goes stale without any event reaching the app.

use crate::error::now_ms;
use crate::tunnels::Meta;
use crate::{aws, creds, owned, ports, procs, tunnels};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::async_runtime;

const CRED_INTERVAL_MS: i64 = 2 * 60 * 1000;
const TUNNEL_INTERVAL_MS: u64 = 60 * 1000;

// Sessions can take a moment to notice a resumed network, so a check immediately on
// wake would report a false death.
const WAKE_GRACE_MS: u64 = 6000;

static CHECKING_CREDS: AtomicBool = AtomicBool::new(false);
static CHECKING_TUNNELS: AtomicBool = AtomicBool::new(false);
static LAST_CRED_CHECK: AtomicI64 = AtomicI64::new(0);
// Most roles are not granted ssm:DescribeSessions. That is not an error worth
// showing, but there is no point asking again every minute.
static DESCRIBE_SESSIONS_ALLOWED: AtomicBool = AtomicBool::new(true);

type Hook = Box<dyn Fn() + Send + Sync>;
static ON_CHANGE: Mutex<Option<std::sync::Arc<Hook>>> = Mutex::new(None);

fn notify() {
    let hook = ON_CHANGE.lock().unwrap().clone();
    if let Some(f) = hook {
        f();
    }
}

pub fn init(on_change: impl Fn() + Send + Sync + 'static) {
    *ON_CHANGE.lock().unwrap() = Some(std::sync::Arc::new(Box::new(on_change)));
    async_runtime::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_millis(TUNNEL_INTERVAL_MS)).await;
            tick("interval").await;
        }
    });
}

pub async fn tick(reason: &'static str) {
    check_credentials(reason).await;
    check_tunnels(reason).await;
}

// Releases a busy flag however the check ends.
struct Busy(&'static AtomicBool);
impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

// A cheap, authoritative call: if AWS accepts it the session is alive, and if it
// answers ExpiredToken then aws.rs marks the credentials expired and the app reacts
// through the existing on_credentials_expired path.
async fn check_credentials(reason: &str) {
    if !creds::has() || creds::expired() {
        return;
    }
    let due = now_ms() - LAST_CRED_CHECK.load(Ordering::SeqCst) >= CRED_INTERVAL_MS;
    if reason == "interval" && !due {
        return;
    }
    if CHECKING_CREDS.swap(true, Ordering::SeqCst) {
        return;
    }
    let _busy = Busy(&CHECKING_CREDS);

    match aws::get_caller_identity().await {
        Ok(identity) => {
            creds::set_identity(identity);
            creds::mark_verified();
            // Profile mode can also refresh the expiry, since the CLI may have rotated the
            // underlying credentials on its own.
            if creds::is_profile() {
                if let Some(exp) = aws::credential_expiration().await {
                    creds::set_expiration(Some(exp));
                }
            }
        }
        // A failed check is evidence in its own right. Reporting it immediately is what
        // makes the app go Offline in seconds rather than waiting out the staleness
        // window — while still never claiming the credentials are expired, which only
        // AWS can tell us.
        Err(err) => match err.kind {
            Some("unreachable") => {
                creds::mark_unreachable("AWS cannot be reached. Check your network or VPN connection.")
            }
            Some("missing-cli") => creds::mark_unreachable(&err.message),
            // 'expired' has already been handled by aws.rs; 'denied'/'other' leave the
            // reachability state alone, since AWS clearly answered.
            _ => {}
        },
    }
    LAST_CRED_CHECK.store(now_ms(), Ordering::SeqCst);
    notify();
}

// Is this tunnel still genuinely working? Three signals, cheapest first.
async fn inspect(t: &Meta) -> Option<String> {
    // 1. Our process. If it is gone, the tunnel is definitively dead.
    let pids = owned::by_tunnel(&t.id);
    if !pids.is_empty() && !pids.iter().any(|p| procs::alive(*p)) {
        return Some("The tunnel process is no longer running.".into());
    }

    // 2. The local port. Nothing listening means nothing can connect.
    if !ports::in_use(t.local_port) {
        return Some(format!("Nothing is listening on local port {} any more.", t.local_port));
    }

    // 3. Whether the port is still ours. Something else on it means ours is gone.
    let c = ports::conflict(t.local_port).await;
    if c.holder.is_some() && !c.ours {
        return Some(format!("Local port {} is now held by {}.", t.local_port, c.label));
    }
    None
}

// The definitive check, when the role is allowed to make it: AWS's own view of
// whether the session is still active. This is the only way to catch a plugin that
// is still running happily over a session AWS has already closed — which is what a
// laptop sleeping does.
async fn server_side_dead(open: &[Meta]) -> Vec<String> {
    if !DESCRIBE_SESSIONS_ALLOWED.load(Ordering::SeqCst) {
        return vec![];
    }
    // Sessions opened as another profile are not visible to the loaded identity, so
    // asking would declare every one of them dead.
    let with_ids: Vec<&Meta> =
        open.iter().filter(|t| t.session_id.is_some() && t.session_profile.is_none()).collect();
    if with_ids.is_empty() {
        return vec![];
    }
    let res = match aws::cli_json(&["ssm", "describe-sessions", "--state", "Active", "--max-results", "200"]).await {
        Ok(r) => r,
        Err(err) => {
            let m = err.message.to_lowercase();
            if m.contains("not authorized") || m.contains("accessdenied") {
                DESCRIBE_SESSIONS_ALLOWED.store(false, Ordering::SeqCst);
            }
            return vec![];
        }
    };
    let active: HashSet<String> = res
        .get("Sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("SessionId").and_then(Value::as_str).map(String::from))
        .collect();
    with_ids
        .into_iter()
        .filter(|t| !active.contains(t.session_id.as_deref().unwrap_or("")))
        .map(|t| t.id.clone())
        .collect()
}

// States when the problem was noticed, which is all we can honestly claim — not why
// the tunnel died.
fn noticed(reason: &str) -> &'static str {
    match reason {
        "resume" => " Noticed when the machine woke up.",
        "unlock" => " Noticed after unlocking.",
        "network" => " Noticed when the network came back.",
        _ => "",
    }
}

fn open_tunnels() -> Vec<Meta> {
    tunnels::list_meta().into_iter().filter(|t| t.status == "open").collect()
}

async fn check_tunnels(reason: &str) {
    let open = open_tunnels();
    if open.is_empty() || CHECKING_TUNNELS.swap(true, Ordering::SeqCst) {
        return;
    }
    let _busy = Busy(&CHECKING_TUNNELS);

    for t in &open {
        if let Some(why) = inspect(t).await {
            tunnels::report_dead(&t.id, format!("{why}{}", noticed(reason)));
        }
    }

    // Re-read: some may have just been declared dead above.
    let still_open = open_tunnels();
    for id in server_side_dead(&still_open).await {
        tunnels::report_dead(&id, "AWS reports this SSM session is no longer active.".into());
    }
    notify();
}

// Called on resume, unlock and network change: the moments when everything the app
// believes may have quietly stopped being true.
pub fn wake(reason: &'static str) {
    LAST_CRED_CHECK.store(0, Ordering::SeqCst); // force a credential check regardless of the interval
    async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(WAKE_GRACE_MS)).await;
        tick(reason).await;
    });
}
