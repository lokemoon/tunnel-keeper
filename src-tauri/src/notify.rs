// Desktop notifications. Unlike Electron's, Tauri's desktop notifications carry no
// click handler, so a toast can no longer bring the window forward — the tray does.

use serde_json::Value;
use std::sync::OnceLock;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

static APP: OnceLock<AppHandle> = OnceLock::new();

pub fn init(app: AppHandle) {
    let _ = APP.set(app);
}

pub fn show(title: &str, body: &str) {
    if let Some(app) = APP.get() {
        let _ = app.notification().builder().title(title).body(body).show();
    }
}

fn where_(t: &Value) -> String {
    format!(
        "{} on localhost:{}",
        t["instanceName"].as_str().unwrap_or(""),
        t["localPort"].as_u64().unwrap_or(0)
    )
}

fn reason(t: &Value) -> &str {
    t["reason"].as_str().unwrap_or("")
}

pub fn tunnel_dropped(t: &Value) {
    show("Tunnel disconnected", &format!("{} — {}", where_(t), reason(t)));
}

// Only sent when auto-reconnect brought it back, so the user who was told it
// dropped also learns it recovered.
pub fn tunnel_restored(t: &Value) {
    let attempts = t["attempts"].as_u64().unwrap_or(0);
    let n = if attempts > 1 { format!(" after {attempts} attempts") } else { String::new() };
    show("Tunnel reconnected", &format!("{} is open again{n}.", where_(t)));
}

pub fn tunnel_gave_up(t: &Value) {
    show("Tunnel could not reconnect", &format!("{} — {}", where_(t), reason(t)));
}

fn are(live: usize) -> &'static str {
    if live > 1 {
        "s are"
    } else {
        " is"
    }
}

pub fn connection_lost(reason: &str, live: usize) {
    let tunnels = if live > 0 { format!(" {live} tunnel{} not carrying traffic.", are(live)) } else { String::new() };
    show("AWS connection lost", &format!("{reason}{tunnels}"));
}

pub fn connection_restored(live: usize) {
    let tunnels = if live > 0 {
        format!(" {live} tunnel{} working again.", are(live))
    } else {
        " Credentials confirmed.".into()
    };
    show("AWS connection restored", &format!("Back online.{tunnels}"));
}

pub fn credentials_expired() {
    show("AWS credentials expired", "Tunnels have been stopped. Load a fresh set to continue.");
}
