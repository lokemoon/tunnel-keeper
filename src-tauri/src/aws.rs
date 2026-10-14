// Run the AWS CLI with the in-memory credentials. Never logs argv values that
// could carry a secret — nothing secret is ever passed as an argument anyway.

use crate::error::{Error, Result};
use crate::{binaries, creds};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;

fn re(p: &str) -> Regex {
    Regex::new(&format!("(?i){p}")).unwrap()
}

// The CLI's own wording ("InvalidClientTokenId", "SignatureDoesNotMatch") does not
// tell anyone what to actually do, so map the ones we expect onto a fix.
pub fn explain(raw: &str) -> String {
    let s = raw;
    // The same AWS error needs different advice depending on where the credentials
    // came from: you re-copy a paste, but you fix a profile in ~/.aws.
    let profile = creds::profile_name();

    static EXPIRED_RE: Lazy<Regex> = Lazy::new(|| re("ExpiredToken|token included in the request is expired"));
    static INVALID: Lazy<Regex> =
        Lazy::new(|| re("InvalidClientTokenId|security token included in the request is invalid"));
    static SIG: Lazy<Regex> = Lazy::new(|| re("SignatureDoesNotMatch"));
    static SSO: Lazy<Regex> = Lazy::new(|| {
        re(r"SSO session .* (expired|invalid)|Error loading SSO Token|sso-session|Token has expired and refresh failed")
    });
    static NOCREDS: Lazy<Regex> = Lazy::new(|| re("Unable to locate credentials"));
    static EMPTY_PROFILE: Lazy<Regex> = Lazy::new(|| re(r"config profile \(\s*\) could not be found"));
    static NOPROFILE: Lazy<Regex> = Lazy::new(|| re("ProfileNotFound|config profile .* could not be found"));
    static ASSUME: Lazy<Regex> = Lazy::new(|| re("not authorized to perform: sts:AssumeRole"));
    static ASSUME_RES: Lazy<Regex> = Lazy::new(|| re(r"on resource:?\s*(\S+)"));
    static MFA: Lazy<Regex> = Lazy::new(|| re("MultiFactorAuthentication|MFA|serial-number"));
    static MFA2: Lazy<Regex> = Lazy::new(|| re("required|invalid|failed"));
    static DURATION: Lazy<Regex> = Lazy::new(|| re("DurationSeconds exceeds the MaxSessionDuration"));
    static DENIED: Lazy<Regex> = Lazy::new(|| re("AccessDenied|not authorized|UnauthorizedOperation"));
    static NET: Lazy<Regex> = Lazy::new(|| re("Could not connect|EndpointConnectionError|NetworkError|getaddrinfo"));

    if EXPIRED_RE.is_match(s) {
        return match &profile {
            Some(p) => format!(
                "The credentials for profile \"{p}\" have expired. Refresh them (for SSO: aws sso login --profile {p})."
            ),
            None => "These credentials have already expired. Copy a fresh set from your access portal.".into(),
        };
    }
    if INVALID.is_match(s) {
        return match &profile {
            Some(p) => format!("AWS rejected the credentials for profile \"{p}\" as invalid. Check that profile in ~/.aws."),
            None => "AWS rejected these credentials as invalid. The session token is usually truncated — re-copy the whole block.".into(),
        };
    }
    if SIG.is_match(s) {
        return match &profile {
            Some(p) => format!(
                "AWS rejected the secret access key for profile \"{p}\". Check that profile in ~/.aws/credentials."
            ),
            None => "AWS rejected the secret access key. It looks incomplete — re-copy the whole block.".into(),
        };
    }
    // SSO sessions are refreshed by a login, not by anything this app can do.
    if SSO.is_match(s) {
        let suffix = profile.as_ref().map(|p| format!(" --profile {p}")).unwrap_or_default();
        return format!("That profile's SSO session has expired. Run: aws sso login{suffix}");
    }
    if NOCREDS.is_match(s) {
        return match &profile {
            Some(p) => format!(
                "Profile \"{p}\" has no usable credentials. Add keys to ~/.aws/credentials, \
                 or if it is an SSO profile run: aws sso login --profile {p}"
            ),
            None => "AWS saw no credentials at all. Re-copy the block and try again.".into(),
        };
    }
    // "profile ()" means an empty AWS_PROFILE reached the CLI — a bug on our side,
    // not a missing profile.
    if EMPTY_PROFILE.is_match(s) {
        return "A stale AWS_PROFILE is interfering. Restart the app and load your credentials again.".into();
    }
    if NOPROFILE.is_match(s) {
        return match &profile {
            Some(p) => format!("Profile \"{p}\" is no longer in ~/.aws/config. Pick another profile."),
            None => "That AWS profile could not be found in ~/.aws/config.".into(),
        };
    }
    // A profile with role_arn assumes the role inside the CLI, so its failures still
    // arrive here and must say what is wrong rather than read as a generic denial.
    if ASSUME.is_match(s) {
        let role = ASSUME_RES.captures(s).map(|m| m[1].to_string());
        return format!(
            "These credentials are not allowed to assume {}. \
             Check the role's trust policy lists the profile's source identity, or pick a different profile.",
            role.as_deref().unwrap_or("that role")
        );
    }
    if MFA.is_match(s) && MFA2.is_match(s) {
        return "That profile's role requires MFA, which this app cannot prompt for. \
                Run any aws command with the profile in a terminal first to cache the session, then load it again."
            .into();
    }
    if DURATION.is_match(s) {
        return "Requested a longer session than the role allows. Lower it, or raise the role's maximum session duration.".into();
    }
    if DENIED.is_match(s) {
        return format!("Your role is not permitted to do this: {}", s.split('\n').next_back().unwrap_or(""));
    }
    if NET.is_match(s) {
        return "Cannot reach AWS. Check your network or VPN connection.".into();
    }
    let lines: Vec<&str> = s.split('\n').collect();
    let tail = lines[lines.len().saturating_sub(2)..].join(" ").trim().to_string();
    if tail.is_empty() {
        "The AWS CLI failed with no output.".into()
    } else {
        tail
    }
}

static EXPIRED: Lazy<Regex> = Lazy::new(|| re("ExpiredToken|token included in the request is expired"));

// Cannot-reach-AWS conditions. These say nothing about whether the credentials are
// still valid, only that we currently have no way to find out.
static UNREACHABLE: Lazy<Regex> = Lazy::new(|| {
    re("Could not connect|EndpointConnectionError|ConnectTimeout|ReadTimeout|NetworkError|getaddrinfo|Temporary failure in name resolution|SSLError|ProxyError|Connection was closed|Network is unreachable|Name or service not known")
});

// Tags the error so callers can act on the kind rather than re-matching strings.
fn classify(raw: &str) -> &'static str {
    if EXPIRED.is_match(raw) {
        "expired"
    } else if UNREACHABLE.is_match(raw) {
        "unreachable"
    } else if re("AccessDenied|not authorized|UnauthorizedOperation").is_match(raw) {
        "denied"
    } else {
        "other"
    }
}

type Hook = Box<dyn Fn() + Send + Sync>;
static ON_EXPIRED: Mutex<Option<Hook>> = Mutex::new(None);

// Lets the app react the moment AWS tells us the session is dead, from whichever
// call happened to notice.
pub fn on_credentials_expired(f: impl Fn() + Send + Sync + 'static) {
    *ON_EXPIRED.lock().unwrap() = Some(Box::new(f));
}

fn fire_expired() {
    if let Some(f) = ON_EXPIRED.lock().unwrap().as_ref() {
        f();
    }
}

pub async fn cli(args: &[&str], timeout_ms: u64) -> Result<String> {
    let bin = binaries::aws_or_throw()?;
    let env = creds::env()?;
    let mut cmd = binaries::command(&bin);
    cmd.args(args).env_clear().envs(&env).kill_on_drop(true);

    let out = match tokio::time::timeout(Duration::from_millis(timeout_ms), cmd.output()).await {
        // Our own timeout elapsed.
        Err(_) => {
            return Err(Error::kind(
                "AWS timed out. Check your network or VPN connection and try again.",
                "unreachable",
            ))
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::kind(
                "AWS CLI not found. Install the AWS CLI v2 and make sure `aws` is on your PATH.",
                "missing-cli",
            ))
        }
        Ok(Err(e)) => return Err(Error::kind(explain(&e.to_string()), "other")),
        Ok(Ok(o)) => o,
    };

    if !out.status.success() {
        let raw = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let kind = classify(&raw);
        if kind == "expired" {
            creds::mark_expired();
            fire_expired();
        }
        return Err(Error::kind(explain(&raw), kind));
    }
    // A successful call is proof AWS is reachable right now.
    creds::mark_reachable();
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn cli_json(args: &[&str]) -> Result<Value> {
    let mut a = args.to_vec();
    a.extend(["--output", "json"]);
    let out = cli(&a, 30000).await?;
    if out.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&out).map_err(|e| Error::msg(format!("Unexpected AWS CLI output: {e}")))
}

// Profile mode can do something paste mode never can: ask the CLI when the
// credentials it resolved actually expire. Only the Expiration is kept — the keys in
// this output are deliberately discarded.
pub async fn credential_expiration() -> Option<String> {
    if !creds::is_profile() {
        return None;
    }
    // Static-key profiles have no expiry, and older CLIs lack the command.
    let out = cli(&["configure", "export-credentials", "--format", "process"], 20000).await.ok()?;
    let parsed: Value = serde_json::from_str(&out).ok()?;
    parsed.get("Expiration").and_then(|v| v.as_str()).map(String::from)
}

pub async fn get_caller_identity() -> Result<creds::Identity> {
    let r = cli_json(&["sts", "get-caller-identity"]).await?;
    let arn = r.get("Arn").and_then(Value::as_str).unwrap_or("").to_string();
    let user_id = r.get("UserId").and_then(Value::as_str).map(String::from);
    // arn:aws:sts::123:assumed-role/Role/session  ->  session
    // arn:aws:iam::123:user/name                  ->  name
    let last = arn.rsplit('/').next().unwrap_or("").to_string();
    let username = if !last.is_empty() { last } else { user_id.clone().unwrap_or_else(|| "unknown".into()) };
    Ok(creds::Identity {
        account: r.get("Account").and_then(Value::as_str).map(String::from),
        arn,
        user_id,
        username,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    id: String,
    name: String,
    private_ip: String,
    #[serde(rename = "type")]
    kind: String,
    windows: bool,
    platform: String,
    jump_host: bool,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

// Instances that SSM can actually reach, joined with their EC2 metadata so the
// list only ever offers targets a tunnel can succeed against.
pub async fn list_instances() -> Result<Vec<Instance>> {
    let ssm = cli_json(&[
        "ssm", "describe-instance-information",
        "--filters", "Key=PingStatus,Values=Online",
        "--max-results", "50",
    ])
    .await?;
    let mut online: Vec<(String, Value)> = Vec::new();
    for i in ssm.get("InstanceInformationList").and_then(Value::as_array).into_iter().flatten() {
        let id = s(i, "InstanceId");
        if id.starts_with("i-") {
            online.push((id, i.clone()));
        }
    }
    if online.is_empty() {
        return Ok(vec![]);
    }

    let mut args = vec!["ec2", "describe-instances", "--instance-ids"];
    args.extend(online.iter().map(|(id, _)| id.as_str()));
    let ec2 = cli_json(&args).await?;
    let mut rows = Vec::new();
    for res in ec2.get("Reservations").and_then(Value::as_array).into_iter().flatten() {
        for inst in res.get("Instances").and_then(Value::as_array).into_iter().flatten() {
            let id = s(inst, "InstanceId");
            let info = online.iter().find(|(i, _)| *i == id).map(|(_, v)| v.clone()).unwrap_or(Value::Null);
            let tags: Vec<Value> = inst.get("Tags").and_then(Value::as_array).cloned().unwrap_or_default();
            let tag = |k: &str| tags.iter().find(|t| s(t, "Key") == k).map(|t| s(t, "Value"));
            let mut platform = s(inst, "Platform");
            if platform.is_empty() {
                platform = s(&info, "PlatformType");
            }
            let platform_name = Some(s(&info, "PlatformName"))
                .filter(|v| !v.is_empty())
                .or_else(|| Some(s(inst, "Platform")).filter(|v| !v.is_empty()))
                .unwrap_or_else(|| "Linux".into());
            rows.push(Instance {
                name: tag("Name").filter(|v| !v.is_empty()).unwrap_or_else(|| id.clone()),
                id,
                private_ip: s(inst, "PrivateIpAddress"),
                kind: s(inst, "InstanceType"),
                windows: platform.to_lowercase().contains("windows"),
                platform: platform_name,
                // Opt-in: only instances tagged SSMJumpHost=true offer forwarding on to a
                // remote host, so the option does not appear for boxes nobody meant as a hop.
                jump_host: tag("SSMJumpHost").map(|v| v.trim().eq_ignore_ascii_case("true")).unwrap_or(false),
            });
        }
    }
    rows.sort_by_key(|r| r.name.to_lowercase());
    Ok(rows)
}
