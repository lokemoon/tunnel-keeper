// Every channel the renderer can call, through one Tauri command. Each returns the
// uniform {ok, data|error} envelope and never a Tauri-level error, so the renderer
// has exactly one error path to render. bridge.js adds the second guarantee: the
// call resolves even if this layer is missing or fails.

use crate::error::{Error, Result};
use crate::{about, aws, aws_config, creds, settings, tunnels};
use serde_json::{json, Value};
use tauri::AppHandle;

fn arg_str(args: &[Value], i: usize) -> String {
    match args.get(i) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn arg_bool(args: &[Value], i: usize) -> bool {
    match args.get(i) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) | None => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| Error::msg(e.to_string()))
}

// A foreign process on the port is not an error the renderer should just print —
// it is a question. Report it as data so the card can offer the choice.
fn started(res: Result<Value>) -> Result<Value> {
    match res {
        Ok(t) => Ok(json!({ "started": true, "tunnel": t })),
        Err(e) if e.conflict.is_some() => {
            Ok(json!({ "started": false, "needsConfirmation": true, "conflict": e.conflict }))
        }
        Err(e) => Err(e),
    }
}

async fn verify_identity() -> Result<()> {
    creds::set_identity(aws::get_caller_identity().await?);
    creds::mark_verified();
    Ok(())
}

async fn dispatch(app: &AppHandle, channel: &str, args: &[Value]) -> Result<Value> {
    match channel {
        "state" => Ok(crate::current_state()),

        // The pasted blob crosses IPC exactly once and is discarded here; only the
        // redacted summary ever goes back the other way.
        "creds:load" => {
            let parsed = creds::parse(&arg_str(args, 0))?;
            let had_region = parsed.region.is_some();
            creds::set(parsed);
            // A region in the paste wins; otherwise reuse the last one the user chose, so
            // nobody has to re-pick their region on every launch.
            if !had_region {
                if let Some(remembered) = settings::region() {
                    if creds::set_region(&remembered).is_err() {
                        settings::set_region(None); // stored value is no longer valid
                    }
                }
            }
            if let Err(e) = verify_identity().await {
                // Never leave half-loaded credentials behind after a failed verification.
                creds::clear();
                return Err(e);
            }
            // Existing tunnels belong to whatever credentials opened them: a renewal of the
            // same identity resumes them, a different one leaves them marked for the user.
            tunnels::review_credentials().await;
            crate::broadcast();
            Ok(creds::summary())
        }

        "app:info" => Ok(about::basic(app)),
        "app:about" => {
            crate::show_about().await;
            Ok(Value::Null)
        }

        "profiles:list" => Ok(json!({
            "profiles": aws_config::list(),
            "configPath": aws_config::config_path().to_string_lossy(),
            "lastUsed": settings::profile(),
        })),

        // Profile mode: the CLI resolves the credentials, so nothing secret enters this
        // process. Verification is the same get-caller-identity as the paste flow.
        "creds:profile" => {
            let name = arg_str(args, 0);
            let profile = aws_config::get(&name)
                .ok_or_else(|| Error::msg(format!("Profile \"{name}\" is not in ~/.aws/config.")))?;

            // Precedence for the region: what the profile itself says, then the one
            // remembered from last session.
            let previous = creds::snapshot();
            creds::set_profile(&name, profile.region.or_else(settings::region))?;

            if let Err(e) = verify_identity().await {
                // A failed switch keeps what was working; a failed first load leaves nothing.
                if previous.has_creds() {
                    creds::restore(previous);
                } else {
                    creds::clear();
                }
                return Err(e);
            }
            // Best-effort, and a real win for SSO profiles: a genuine expiry countdown.
            creds::set_expiration(aws::credential_expiration().await);
            tunnels::review_credentials().await;

            settings::set_profile(Some(name));
            settings::set_region(Some(creds::region()));
            crate::broadcast();
            Ok(creds::summary())
        }

        "creds:clear" => {
            tunnels::stop_all();
            creds::clear();
            crate::broadcast();
            Ok(creds::summary())
        }

        "creds:region" => {
            let next = creds::set_region(&arg_str(args, 0))?;
            settings::set_region(Some(next.clone()));
            crate::broadcast();
            Ok(json!(next))
        }

        "hosts:clear" => {
            settings::clear_remote_hosts();
            crate::broadcast();
            Ok(json!([]))
        }

        "instances:list" => to_value(aws::list_instances().await?),
        "port:free" => Ok(json!(crate::ports::find_free_port()?)),

        // Starting has the same port-conflict question as reconnecting, so it answers the
        // same way: as data the UI can turn into a prompt, not an error string.
        "tunnel:start" => {
            let opts: tunnels::StartOpts =
                serde_json::from_value(args.first().cloned().unwrap_or(Value::Null)).unwrap_or_default();
            started(tunnels::start(opts).await)
        }
        "tunnel:reconnect" => started(tunnels::reconnect(&arg_str(args, 0), arg_bool(args, 1)).await),
        "tunnel:auto" => Ok(json!(tunnels::set_auto_reconnect(&arg_str(args, 0), arg_bool(args, 1))?)),
        "tunnel:stop" => {
            tunnels::stop(&arg_str(args, 0));
            Ok(Value::Null)
        }
        "tunnel:remove" => {
            tunnels::remove(&arg_str(args, 0));
            Ok(Value::Null)
        }

        // The webview has no console forwarder like Electron's, and a renderer
        // exception otherwise surfaces nowhere: buttons silently stop responding.
        "log:renderer" => {
            eprintln!("[renderer] {}", arg_str(args, 0));
            Ok(Value::Null)
        }

        other => Err(Error::msg(format!("Unknown channel {other}."))),
    }
}

#[tauri::command]
pub async fn call(app: AppHandle, channel: String, args: Option<Vec<Value>>) -> Value {
    let args = args.unwrap_or_default();
    match dispatch(&app, &channel, &args).await {
        Ok(data) => json!({ "ok": true, "data": data }),
        Err(e) => json!({ "ok": false, "error": e.message }),
    }
}
