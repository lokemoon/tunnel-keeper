// Tray state and menu. Menu item ids encode the action and tunnel ("stop:t3");
// lib.rs routes them.

use crate::tray_icon;
use serde_json::Value;
use std::sync::Mutex;
use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Wry};

static TRAY: Mutex<Option<TrayIcon>> = Mutex::new(None);
static LAST_KEY: Mutex<String> = Mutex::new(String::new());

pub fn set(tray: TrayIcon) {
    *TRAY.lock().unwrap() = Some(tray);
}

pub fn destroy() {
    TRAY.lock().unwrap().take();
}

fn status(t: &Value) -> &str {
    t["status"].as_str().unwrap_or("")
}

fn plural(n: usize) -> &'static str {
    if n > 1 {
        "s"
    } else {
        ""
    }
}

pub struct Overall {
    pub status: &'static str,
    pub text: String,
    pub count: usize,
}

// Overall state, worst-first: one broken tunnel matters more than three healthy ones.
pub fn overall(creds: &Value, list: &[Value]) -> Overall {
    let o = |status, text: String, count| Overall { status, text, count };
    if !creds["loaded"].as_bool().unwrap_or(false) {
        return o("idle", "Not signed in".into(), 0);
    }
    if creds["expired"].as_bool().unwrap_or(false) {
        return o("error", "Credentials expired".into(), 0);
    }

    // Offline outranks everything below it: with no confirmed connection to AWS the
    // tunnel states we hold are not trustworthy, so we do not advertise them.
    if !creds["online"].as_bool().unwrap_or(false) {
        let live = list.iter().filter(|t| matches!(status(t), "open" | "starting")).count();
        let why = creds["offlineReason"].as_str().unwrap_or("AWS not reachable");
        return o(
            "warn",
            if live > 0 {
                format!("Offline — {live} tunnel{} not working · {why}", plural(live))
            } else {
                format!("Offline — {why}")
            },
            0,
        );
    }

    let count = |s: &str| list.iter().filter(|t| status(t) == s).count();
    let open = count("open");
    let starting = count("starting");
    let retrying = count("reconnecting");
    let dropped = list.iter().filter(|t| t["dropped"].as_bool() == Some(true) && status(t) == "error").count();

    if dropped > 0 {
        return o("error", format!("{dropped} tunnel{} dropped", plural(dropped)), open);
    }
    // Retrying is recoverable, so it reads as in-progress rather than failed.
    if retrying > 0 {
        return o("warn", format!("{retrying} reconnecting"), open);
    }
    if starting > 0 {
        return o("warn", format!("{starting} starting"), open);
    }
    if open > 0 {
        return o("active", format!("{open} tunnel{} open", plural(open)), open);
    }
    if matches!(creds["expiresInSeconds"].as_i64(), Some(s) if s < 300) {
        return o("warn", "Credentials expiring soon".into(), 0);
    }
    o("ready", "Ready — no tunnels".into(), 0)
}

fn dot(s: &str) -> &'static str {
    match s {
        "open" => "●",
        "starting" => "○",
        "reconnecting" => "◌",
        "error" => "✕",
        _ => "·",
    }
}

fn build_menu(app: &AppHandle, creds: &Value, list: &[Value]) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    let label = |text: String| MenuItem::new(app, text, false, None::<&str>);
    let sep = || PredefinedMenuItem::separator(app);
    let online = creds["online"].as_bool().unwrap_or(false);

    if creds["loaded"].as_bool().unwrap_or(false) {
        menu.append(&label(format!(
            "{} · {}",
            creds["username"].as_str().unwrap_or("unknown"),
            creds["account"].as_str().unwrap_or("—")
        ))?)?;
        let region = creds["region"].as_str().unwrap_or("");
        menu.append(&label(if online { format!("Region {region}") } else { format!("Region {region} · offline") })?)?;
    } else {
        menu.append(&label("No credentials loaded".into())?)?;
    }
    menu.append(&sep()?)?;

    if !list.is_empty() {
        for t in list {
            let id = t["id"].as_str().unwrap_or("");
            let st = status(t);
            let port = t["localPort"].as_u64().unwrap_or(0);
            let live = matches!(st, "open" | "starting" | "reconnecting");
            // Never label a tunnel as running while we cannot confirm AWS is reachable.
            let offline = !online && matches!(st, "open" | "starting");

            let copy = MenuItem::with_id(app, format!("copy:{id}"), format!("Copy localhost:{port}"), true, None::<&str>)?;
            let auto = CheckMenuItem::with_id(
                app,
                format!("auto:{id}"),
                "Auto-reconnect if it drops",
                true,
                t["autoReconnect"].as_bool().unwrap_or(false),
                None::<&str>,
            )?;
            let s1 = sep()?;
            let mut items: Vec<Box<dyn IsMenuItem<Wry>>> = vec![Box::new(copy), Box::new(auto), Box::new(s1)];
            if live {
                items.push(Box::new(MenuItem::with_id(app, format!("stop:{id}"), "Stop this tunnel", true, None::<&str>)?));
            } else {
                // Same port, so nothing the user configured needs changing.
                items.push(Box::new(MenuItem::with_id(
                    app,
                    format!("reconnect:{id}"),
                    format!("Reconnect on localhost:{port}"),
                    true,
                    None::<&str>,
                )?));
                items.push(Box::new(MenuItem::with_id(app, format!("remove:{id}"), "Dismiss", true, None::<&str>)?));
            }
            let refs: Vec<&dyn IsMenuItem<Wry>> = items.iter().map(|b| b.as_ref()).collect();
            let text = format!(
                "{} {} — localhost:{port} → {}:{}{}",
                if offline { "⚠" } else { dot(st) },
                t["instanceName"].as_str().unwrap_or(""),
                t["remoteHost"].as_str().unwrap_or(""),
                t["remotePort"].as_u64().unwrap_or(0),
                if offline { " (offline)" } else { "" }
            );
            menu.append(&Submenu::with_items(app, text, true, &refs)?)?;
        }
        menu.append(&sep()?)?;
        if list.iter().any(|t| matches!(status(t), "open" | "starting" | "reconnecting")) {
            menu.append(&MenuItem::with_id(app, "stopAll", "Stop all tunnels", true, None::<&str>)?)?;
        }
    } else {
        menu.append(&label("No tunnels".into())?)?;
    }

    menu.append(&sep()?)?;
    menu.append(&MenuItem::with_id(app, "show", "Open Tunnel Keeper", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "about", "About Tunnel Keeper", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?)?;
    Ok(menu)
}

pub fn update(app: &AppHandle, state: &Value) {
    let tray = TRAY.lock().unwrap().clone();
    let Some(tray) = tray else { return };
    let creds = &state["creds"];
    let list: Vec<Value> = state["tunnels"].as_array().cloned().unwrap_or_default();
    let o = overall(creds, &list);

    // Rebuilding the menu on every 1s tick would close it while in use.
    let key = serde_json::json!([
        o.status,
        o.text,
        o.count,
        creds["online"],
        list.iter().map(|t| serde_json::json!([t["id"], t["status"], t["localPort"], t["autoReconnect"]])).collect::<Vec<_>>()
    ])
    .to_string();
    {
        let mut last = LAST_KEY.lock().unwrap();
        if *last == key {
            return;
        }
        *last = key;
    }

    let _ = tray.set_icon(Some(tray_icon::icon(o.status)));
    let _ = tray.set_tooltip(Some(format!("Tunnel Keeper — {}", o.text)));
    // macOS can show text beside the icon; keep it to the open-tunnel count.
    #[cfg(target_os = "macos")]
    let _ = tray.set_title(Some(if o.count > 0 { o.count.to_string() } else { String::new() }));
    if let Ok(menu) = build_menu(app, creds, &list) {
        let _ = tray.set_menu(Some(menu));
    }
}

// Forces the next update() to rebuild, e.g. after a checkbox toggled itself.
pub fn invalidate() {
    LAST_KEY.lock().unwrap().clear();
}
