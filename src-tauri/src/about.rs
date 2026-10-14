use crate::binaries;
use serde_json::{json, Value};
use tauri::AppHandle;

// The installed AWS CLI version, since most support questions start there. Never
// fails: a missing CLI is itself the useful answer.
async fn aws_cli_version() -> String {
    let Ok(bin) = binaries::aws_or_throw() else { return "not found".into() };
    let mut cmd = binaries::command(bin);
    cmd.arg("--version").kill_on_drop(true);
    let out = match tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(o)) if o.status.success() => o,
        _ => return "could not be run".into(),
    };
    // "aws-cli/2.15.0 Python/3.11.6 Darwin/23.1.0 ..." -> "2.15.0"
    let text = format!("{} {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    regex::Regex::new(r"aws-cli/(\S+)")
        .unwrap()
        .captures(&text)
        .map(|m| m[1].to_string())
        .unwrap_or_else(|| "unknown".into())
}

// Cheap and synchronous, for the version shown in the window header.
pub fn basic(app: &AppHandle) -> Value {
    let p = app.package_info();
    json!({ "name": p.name, "version": p.version.to_string() })
}

pub async fn info(app: &AppHandle) -> Value {
    let p = app.package_info();
    json!({
        "name": p.name,
        "version": p.version.to_string(),
        "tauri": tauri::VERSION,
        "webview": tauri::webview_version().unwrap_or_else(|_| "unknown".into()),
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "awsCli": aws_cli_version().await,
        "sessionManagerPlugin": if binaries::has_session_manager_plugin() { "installed" } else { "not found" },
    })
}

pub fn describe(i: &Value) -> String {
    let s = |k: &str| i[k].as_str().unwrap_or("").to_string();
    [
        format!("Version {}", s("version")),
        String::new(),
        format!("AWS CLI: {}", s("awsCli")),
        format!("session-manager-plugin: {}", s("sessionManagerPlugin")),
        format!("Tauri {} · WebView {}", s("tauri"), s("webview")),
        format!("Platform: {}", s("platform")),
    ]
    .join("\n")
}
