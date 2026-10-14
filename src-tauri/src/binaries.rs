// A GUI app launched from Finder or the Start Menu does not inherit the shell's
// PATH — macOS gives it roughly /usr/bin:/bin:/usr/sbin:/sbin. Neither `aws` nor
// `session-manager-plugin` lives there, so without this every packaged install
// would fail with "AWS CLI not found". The plugin is located by the CLI through
// PATH, so extending PATH covers it too.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[cfg(target_os = "macos")]
const EXTRA_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "/usr/local/aws-cli",
    "/opt/aws-cli/bin",
    "/usr/local/sessionmanagerplugin/bin",
];
#[cfg(windows)]
const EXTRA_DIRS: &[&str] = &[
    "C:\\Program Files\\Amazon\\AWSCLIV2",
    "C:\\Program Files (x86)\\Amazon\\AWSCLIV2",
    "C:\\Program Files\\Amazon\\SessionManagerPlugin\\bin",
    "C:\\Program Files (x86)\\Amazon\\SessionManagerPlugin\\bin",
];
#[cfg(all(unix, not(target_os = "macos")))]
const EXTRA_DIRS: &[&str] = &["/usr/local/bin", "/usr/bin", "/snap/bin"];

const EXE: &str = if cfg!(windows) { ".exe" } else { "" };
const SEP: &str = if cfg!(windows) { ";" } else { ":" };

fn home_dirs() -> Vec<String> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
    match home {
        Ok(h) if !h.is_empty() => vec![
            Path::new(&h).join(".local").join("bin").to_string_lossy().into_owned(),
            Path::new(&h).join("bin").to_string_lossy().into_owned(),
        ],
        _ => vec![],
    }
}

fn search_dirs() -> Vec<String> {
    let from_path = std::env::var("PATH").unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    // Keep the user's own PATH first so a deliberately chosen install wins.
    let all = from_path
        .split(SEP)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .chain(EXTRA_DIRS.iter().map(|s| s.to_string()))
        .chain(home_dirs());
    for d in all {
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

// PATH for spawned processes, widened so the CLI can find its plugin.
pub fn augmented_path() -> String {
    search_dirs().join(SEP)
}

static CACHED: Mutex<Option<PathBuf>> = Mutex::new(None);

// Absolute path to the aws executable, so we never depend on PATH resolution in
// the spawn itself. Returns None when it genuinely is not installed.
pub fn find_aws() -> Option<PathBuf> {
    let mut cached = CACHED.lock().unwrap();
    if let Some(p) = cached.as_ref() {
        if p.exists() {
            return Some(p.clone());
        }
    }
    for dir in search_dirs() {
        let candidate = Path::new(&dir).join(format!("aws{EXE}"));
        if candidate.is_file() {
            *cached = Some(candidate.clone());
            return Some(candidate);
        }
    }
    None
}

pub fn aws_or_throw() -> Result<PathBuf> {
    find_aws().ok_or_else(|| {
        Error::kind(
            format!(
                "AWS CLI not found. Install AWS CLI v2, then reopen this app. {}",
                if cfg!(target_os = "macos") {
                    "On macOS: brew install awscli"
                } else {
                    "On Windows: install the AWSCLIV2 MSI from AWS."
                }
            ),
            "missing-cli",
        )
    })
}

pub fn has_session_manager_plugin() -> bool {
    search_dirs()
        .iter()
        .any(|d| Path::new(d).join(format!("session-manager-plugin{EXE}")).is_file())
}

// Every subprocess gets the widened PATH, and on Windows no console window: a GUI
// app spawning console programs otherwise flashes a terminal for each one.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.env("PATH", augmented_path());
    cmd.stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

// Runs a helper (lsof, pgrep, netstat, ...) and returns stdout, or "" on any
// failure. Never throws: callers treat "no output" as "nothing found".
pub async fn run(program: &str, args: &[&str], timeout_ms: u64) -> String {
    let mut cmd = command(program);
    cmd.args(args).kill_on_drop(true);
    let fut = cmd.output();
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), fut).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    }
}
