// Small JSON file in the OS-standard per-user app data directory — the same file
// the Electron build used, so a remembered region survives the upgrade:
//   macOS   ~/Library/Application Support/Tunnel Keeper/settings.json
//   Windows %APPDATA%\Tunnel Keeper\settings.json
// Preferences only — credentials are never persisted anywhere.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    pub region: Option<String>,
    pub profile: Option<String>,
}

static CACHE: Mutex<Option<Settings>> = Mutex::new(None);
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn init(data_dir: PathBuf) {
    *DIR.lock().unwrap() = Some(data_dir.join("Tunnel Keeper"));
}

fn file() -> Option<PathBuf> {
    DIR.lock().unwrap().as_ref().map(|d| d.join("settings.json"))
}

pub fn load() -> Settings {
    let mut cache = CACHE.lock().unwrap();
    if let Some(s) = cache.as_ref() {
        return s.clone();
    }
    // Ignore anything unexpected on disk rather than trusting it into the app.
    let parsed: serde_json::Value = file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let s = Settings {
        region: parsed.get("region").and_then(|v| v.as_str()).map(String::from),
        profile: parsed.get("profile").and_then(|v| v.as_str()).map(String::from),
    };
    *cache = Some(s.clone());
    s
}

fn save(next: Settings) {
    *CACHE.lock().unwrap() = Some(next.clone());
    let Some(f) = file() else { return };
    let res = f
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|_| std::fs::write(&f, serde_json::to_string_pretty(&next).unwrap_or_default()));
    // A read-only profile should not stop the app from working this session.
    if let Err(e) = res {
        eprintln!("[settings] could not save: {e}");
    }
}

pub fn region() -> Option<String> {
    load().region
}

pub fn profile() -> Option<String> {
    load().profile
}

pub fn set_region(v: Option<String>) {
    save(Settings { region: v, ..load() });
}

pub fn set_profile(v: Option<String>) {
    save(Settings { profile: v, ..load() });
}
