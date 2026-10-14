// Window and tray lifetime, the 1s broadcast, sleep/network watching, and the
// confirm dialogs. The equivalent of the Electron build's main.js.

mod about;
mod aws;
mod aws_config;
mod binaries;
mod connectivity;
mod creds;
mod error;
mod health;
mod ipc;
mod notify;
mod owned;
mod ports;
mod procs;
mod settings;
mod tray;
mod tray_icon;
mod tunnels;

#[cfg(test)]
mod tests;

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{async_runtime, AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult};

static APP: OnceLock<AppHandle> = OnceLock::new();
static QUITTING: AtomicBool = AtomicBool::new(false);
static CONNECTION: OnceLock<connectivity::Connectivity> = OnceLock::new();

fn app() -> &'static AppHandle {
    APP.get().expect("app not initialised")
}

pub(crate) fn current_state() -> Value {
    json!({ "creds": creds::summary(), "tunnels": tunnels::list() })
}

pub(crate) fn broadcast() {
    let Some(app) = APP.get() else { return };
    let state = current_state();
    let _ = app.emit_to("main", "state", &state);
    tray::update(app, &state);
    if let Some(c) = CONNECTION.get() {
        c.observe(&state);
    }
}

fn show_window() {
    let app = app();
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

// The window is in front of the user, so the badge and banner already tell them
// everything a notification would. Anything less — hidden to tray, minimised, behind
// another app — counts as background.
fn window_in_foreground() -> bool {
    let Some(w) = APP.get().and_then(|a| a.get_webview_window("main")) else { return false };
    w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(true) && w.is_focused().unwrap_or(false)
}

// Native dialogs block, so they run off the async workers. True only when the
// user picked `yes`; closing the dialog counts as the other button.
async fn ask(kind: MessageDialogKind, title: String, message: String, yes: String, no: String) -> bool {
    let app = app().clone();
    async_runtime::spawn_blocking(move || {
        let res = app
            .dialog()
            .message(message)
            .title(title)
            .kind(kind)
            .buttons(MessageDialogButtons::OkCancelCustom(yes.clone(), no))
            .blocking_show_with_result();
        res == MessageDialogResult::Custom(yes) || res == MessageDialogResult::Ok
    })
    .await
    .unwrap_or(false)
}

async fn tell(kind: MessageDialogKind, title: String, message: String) {
    let app = app().clone();
    let _ = async_runtime::spawn_blocking(move || {
        app.dialog()
            .message(message)
            .title(title)
            .kind(kind)
            .buttons(MessageDialogButtons::OkCustom("OK".into()))
            .blocking_show();
    })
    .await;
}

pub(crate) async fn show_about() {
    let i = about::info(app()).await;
    let name = i["name"].as_str().unwrap_or("Tunnel Keeper").to_string();
    let version = i["version"].as_str().unwrap_or("").to_string();
    let detail = about::describe(&i);
    let copy = ask(
        MessageDialogKind::Info,
        format!("About {name}"),
        format!("{name} {version}\n\n{detail}"),
        "Copy details".into(),
        "OK".into(),
    )
    .await;
    if copy {
        let _ = app().clipboard().write_text(format!("{name} {version}\n{detail}"));
    }
}

// The newly loaded credentials are a different identity from the one the listed
// tunnels were opened with, so none of them can be reconnected to the same place.
// Asked rather than done: the cards are the user's record of what they had set up,
// and their local ports may be written into client configs.
async fn confirm_reset_tunnels(payload: &Value) -> bool {
    let n = payload["stale"].as_array().map(|a| a.len()).unwrap_or(0);
    let principal: Option<creds::Principal> = serde_json::from_value(payload["principal"].clone()).ok();
    let one = n == 1;
    let message = format!(
        "{} opened with different credentials.\n\nThese credentials are for {}. {} {} opened as a different \
         identity and cannot reconnect to the same instance, so reconnecting is disabled until dismissed. \
         Clear them now?",
        if one { "One tunnel was".to_string() } else { format!("{n} tunnels were") },
        creds::describe_principal(principal.as_ref()),
        if one { "That tunnel" } else { "Those tunnels" },
        if one { "was" } else { "were" },
    );
    ask(
        MessageDialogKind::Info,
        "Different AWS identity".into(),
        message,
        if one { "Dismiss it" } else { "Dismiss them" }.into(),
        "Keep for now".into(),
    )
    .await
}

// Confirmation for killing a process the app did not start. Used by the tray and by
// automatic retries, neither of which has a card to render into.
async fn confirm_kill(conflict: &Value) -> bool {
    let message = conflict["message"].as_str().unwrap_or("").to_string();
    if !conflict["canTerminate"].as_bool().unwrap_or(false) {
        tell(
            MessageDialogKind::Error,
            "Cannot reconnect".into(),
            format!(
                "{message}\n\nIt was not started by Tunnel Keeper and belongs to another user, so this app \
                 cannot stop it. Free the port yourself, then reconnect."
            ),
        )
        .await;
        return false;
    }
    let name = conflict["holder"]["name"].as_str().map(String::from);
    ask(
        MessageDialogKind::Warning,
        "Port in use by another program".into(),
        format!(
            "{message}\n\n{} was not started by Tunnel Keeper. Stopping it will end whatever is using it. \
             Reconnecting needs this exact port.",
            name.clone().unwrap_or_else(|| "That process".into())
        ),
        format!("Stop {} and reconnect", name.unwrap_or_else(|| "it".into())),
        "Cancel".into(),
    )
    .await
}

async fn ask_then_reconnect(id: String, conflict: Value) {
    if !confirm_kill(&conflict).await {
        notify::show("Reconnect cancelled", conflict["message"].as_str().unwrap_or(""));
        return;
    }
    if let Err(e) = tunnels::reconnect(&id, true).await {
        notify::show("Could not reconnect", &e.message);
    }
}

// From the tray there is no status line to write to, so a failure has to speak for
// itself as a notification, and a question has to become a dialog.
fn on_menu(id: &str) {
    let (action, tid) = id.split_once(':').unwrap_or((id, ""));
    let tid = tid.to_string();
    match action {
        "show" => show_window(),
        "about" => {
            async_runtime::spawn(show_about());
        }
        "quit" => quit(),
        "stopAll" => tunnels::stop_all(),
        "stop" => tunnels::stop(&tid),
        "remove" => tunnels::remove(&tid),
        "copy" => {
            let port = tunnels::list_meta().into_iter().find(|t| t.id == tid).map(|t| t.local_port);
            if let Some(port) = port {
                let _ = app().clipboard().write_text(format!("localhost:{port}"));
            }
        }
        "auto" => {
            let current = tunnels::list_meta().into_iter().find(|t| t.id == tid).map(|t| t.auto_reconnect);
            if let Some(current) = current {
                let _ = tunnels::set_auto_reconnect(&tid, !current);
            }
            tray::invalidate();
            broadcast();
        }
        "reconnect" => {
            async_runtime::spawn(async move {
                if let Err(e) = tunnels::reconnect(&tid, false).await {
                    match e.conflict {
                        Some(c) => ask_then_reconnect(tid, serde_json::to_value(*c).unwrap_or_default()).await,
                        None => notify::show("Could not reconnect", &e.message),
                    }
                }
            });
        }
        _ => {}
    }
}

fn on_tunnel_event(kind: &str, t: Value) {
    match kind {
        // With auto-reconnect on, a drop is not worth interrupting for — the outcome is.
        "dropped" => notify::tunnel_dropped(&t),
        "restored" => notify::tunnel_restored(&t),
        "gaveup" => notify::tunnel_gave_up(&t),
        "credentials-changed" => {
            async_runtime::spawn(async move {
                if confirm_reset_tunnels(&t).await {
                    tunnels::remove_stale();
                }
                broadcast();
            });
        }
        // An automatic retry found a foreign process on the port. Auto-reconnect still
        // does not get to kill someone else's process without being told to.
        "confirm-kill" => {
            async_runtime::spawn(async move {
                let approved = confirm_kill(&t["conflict"]).await;
                let id = t["id"].as_str().unwrap_or("").to_string();
                // resolve_foreign_kill has already recorded any failure on the tunnel.
                let _ = tunnels::resolve_foreign_kill(&id, approved).await;
            });
        }
        _ => {}
    }
}

// Interface-level connectivity, as Electron's net.isOnline() reported it: any
// non-loopback interface holding a routable address.
fn has_network() -> bool {
    if_addrs::get_if_addrs()
        .map(|ifs| {
            ifs.iter().any(|i| {
                !i.is_loopback()
                    && match i.ip() {
                        std::net::IpAddr::V4(a) => !a.is_link_local(),
                        std::net::IpAddr::V6(a) => (a.segments()[0] & 0xffc0) != 0xfe80,
                    }
            })
        })
        .unwrap_or(true)
}

// Sleep and network loss are exactly when the app's picture of the world goes stale
// without any event telling it so. Tauri has no powerMonitor, so a resume is detected
// from the wall clock instead: timers do not run while the machine sleeps, so a tick
// that arrives far later than scheduled means the machine was suspended.
fn watch_system_events() {
    const TICK: Duration = Duration::from_secs(3);
    const SLEPT: Duration = Duration::from_secs(20);

    // Watched in both directions. Losing the network is the fastest possible signal
    // that nothing is working, so it is reported at once instead of waiting for a
    // check to time out or for the staleness window to elapse.
    let mut had_network = has_network();
    if !had_network {
        creds::mark_unreachable("This machine has no network connection.");
    }
    async_runtime::spawn(async move {
        let mut last = SystemTime::now();
        loop {
            tokio::time::sleep(TICK).await;
            let now = SystemTime::now();
            let gap = now.duration_since(last).unwrap_or_default();
            last = now;
            if gap > TICK + SLEPT {
                health::wake("resume");
            }

            let net = has_network();
            if net == had_network {
                continue;
            }
            had_network = net;
            if net {
                // Do not claim Online again on the strength of the interface alone — a check
                // has to succeed first. wake() runs one shortly.
                health::wake("network");
            } else {
                creds::mark_unreachable("This machine has no network connection.");
                broadcast();
            }
        }
    });
}

fn quit() {
    QUITTING.store(true, Ordering::SeqCst);
    tunnels::stop_all();
    creds::clear();
    tray::destroy();
    app().exit(0);
}

pub fn run() {
    tauri::Builder::default()
        // One instance only: a second launch should surface the existing tray/window
        // rather than start a rival set of tunnels. Must be the first plugin.
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| show_window()))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![ipc::call])
        .setup(|app| {
            let handle = app.handle().clone();
            let _ = APP.set(handle.clone());
            notify::init(handle.clone());
            if let Ok(dir) = app.path().data_dir() {
                settings::init(dir);
            }
            // Seed the region from the last session before anything queries AWS.
            if let Some(r) = settings::region() {
                let _ = creds::set_default_region(&r);
            }

            // Online/offline transitions are announced from here, but only when the window
            // is not the active window — see connectivity.rs for the two suppression rules.
            let _ = CONNECTION.set(connectivity::Connectivity::new(connectivity::Deps {
                is_foreground: Box::new(window_in_foreground),
                get_state: Box::new(current_state),
                on_lost: Box::new(|reason, live| notify::connection_lost(&reason, live)),
                on_restored: Box::new(notify::connection_restored),
                settle_ms: connectivity::SETTLE_MS,
            }));

            let tray = TrayIconBuilder::with_id("main")
                .icon(tray_icon::icon("idle"))
                .tooltip("Tunnel Keeper")
                // macOS opens the menu on click by default; Windows shows the window and
                // keeps the menu on right-click.
                .show_menu_on_left_click(cfg!(target_os = "macos"))
                .on_menu_event(|_app, event| on_menu(event.id().as_ref()))
                .on_tray_icon_event(|_tray, event| match event {
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                        if cfg!(windows) =>
                    {
                        show_window()
                    }
                    TrayIconEvent::DoubleClick { .. } => show_window(),
                    _ => {}
                })
                .build(app)?;
            tray::set(tray);

            tunnels::on_change(broadcast);
            tunnels::on_event(on_tunnel_event);
            aws::on_credentials_expired(|| {
                // Every tunnel is dead once the session is, so do not leave them looking open.
                // Suspended rather than merely stopped, so a renewal of the same access can bring
                // back exactly the tunnels the expiry took down.
                tunnels::suspend_for_expiry();
                notify::credentials_expired();
                broadcast();
            });

            // Push the credential expiry countdown without the UI having to poll for it.
            async_runtime::spawn(async {
                loop {
                    broadcast();
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });

            health::init(broadcast);
            watch_system_events();

            // Development only: TK_EVAL=<file> runs a script in the real webview once it
            // has loaded — the way to click through the UI from a harness, since the
            // webview cannot be driven from outside. Compiled out of release builds.
            #[cfg(debug_assertions)]
            if let Ok(path) = std::env::var("TK_EVAL") {
                let h = handle.clone();
                async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if let (Ok(js), Some(w)) = (std::fs::read_to_string(path), h.get_webview_window("main")) {
                        let _ = w.eval(&js);
                    }
                });
            }
            Ok(())
        })
        // Closing the window leaves tunnels running in the tray rather than killing
        // them; quitting is explicit, from the tray.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if !QUITTING.load(Ordering::SeqCst) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Tunnel Keeper")
        .run(|_app, event| match event {
            // The window is only ever hidden, never closed, so the tray keeps the app alive
            // without any help here — an exit request is always a deliberate quit (Cmd+Q,
            // logout), and it gets the same teardown as the tray's Quit.
            RunEvent::ExitRequested { .. } => {
                if !QUITTING.swap(true, Ordering::SeqCst) {
                    tunnels::stop_all();
                    creds::clear();
                }
            }
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => show_window(),
            _ => {}
        });
}
