// Credential handling. Secrets live only in this module's memory, never on disk,
// never sent to the renderer. The renderer only ever sees the redacted summary.

use crate::error::{now_ms, Error, Result};
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use zeroize::Zeroize;

#[derive(Clone, Default)]
pub struct Parsed {
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub expiration: Option<String>,
    pub region: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Mode {
    Paste,
    Profile,
}

#[derive(Clone)]
pub struct Creds {
    mode: Mode,
    profile: Option<String>,
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    expiration: Option<String>,
    expiry_observed: bool,
    region: Option<String>,
    loaded_at: i64,
    verified_at: Option<i64>,
}

impl Drop for Creds {
    // Best-effort scrub before the memory is released.
    fn drop(&mut self) {
        self.access_key_id.zeroize();
        self.secret_access_key.zeroize();
        if let Some(t) = self.session_token.as_mut() {
            t.zeroize();
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Identity {
    pub account: Option<String>,
    pub arn: String,
    #[serde(rename = "userId")]
    pub user_id: Option<String>,
    pub username: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct Principal {
    pub account: Option<String>,
    pub who: Option<String>,
    pub region: Option<String>,
}

struct Unreachable {
    reason: String,
}

struct State {
    creds: Option<Creds>,
    identity: Option<Identity>,
    default_region: Option<String>, // remembered from the previous session
    // Set when we positively know we cannot reach AWS, so the UI does not have to wait
    // out the staleness window. Deliberately distinct from expiry: unreachable means we
    // do not know, whereas expired is something only AWS can tell us.
    unreachable: Option<Unreachable>,
}

static STATE: Mutex<State> =
    Mutex::new(State { creds: None, identity: None, default_region: None, unreachable: None });

fn st() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap()
}

fn key_field(name: &str) -> Option<&'static str> {
    Some(match name {
        "AWS_ACCESS_KEY_ID" => "accessKeyId",
        "AWS_SECRET_ACCESS_KEY" => "secretAccessKey",
        "AWS_SESSION_TOKEN" | "AWS_SECURITY_TOKEN" => "sessionToken",
        "AWS_DEFAULT_REGION" | "AWS_REGION" => "region",
        "AWS_CREDENTIAL_EXPIRATION" => "expiration",
        _ => return None,
    })
}

// Strips one matched layer of surrounding quotes, plus a trailing `;` or `,` that
// shell/PowerShell snippets often carry. Values are left untouched if the quotes
// don't match, since an unbalanced quote means the paste was truncated.
fn unquote(value: &str) -> String {
    let mut v = value.trim().trim_end_matches([';', ',']).trim().to_string();
    let first = v.chars().next();
    if matches!(first, Some('"') | Some('\'')) && v.len() > 1 && v.ends_with(first.unwrap()) {
        v = v[1..v.len() - 1].to_string();
    }
    // Escaped quotes survive a JSON-ish paste; secrets never legitimately contain \".
    static ESC: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\\(["'\\])"#).unwrap());
    ESC.replace_all(&v, "$1").into_owned()
}

// Accepts the date shapes AWS hands out, normalised to the ISO form JS's
// toISOString() produced, which the renderer parses.
pub fn normalize_time(s: &str) -> Option<String> {
    let s = s.trim();
    let dt: DateTime<Utc> = if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        d.with_timezone(&Utc)
    } else if let Ok(d) = DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%z") {
        d.with_timezone(&Utc)
    } else if let Ok(d) = DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%z") {
        d.with_timezone(&Utc)
    } else if let Ok(n) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        n.and_utc()
    } else if let Ok(n) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        n.and_utc()
    } else {
        return None;
    };
    Some(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn time_ms(iso: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(iso).ok().map(|d| d.timestamp_millis())
}

fn json_str(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| v.get(*k)).and_then(|x| match x {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    })
}

// Accepts any of the shapes people actually paste. The AWS console alone hands out
// three different assignment syntaxes depending on which tab you copy from:
//   export AWS_ACCESS_KEY_ID="..."      (macOS/Linux tab, quoted or bare)
//   set AWS_ACCESS_KEY_ID=...           (Windows cmd tab)
//   $env:AWS_ACCESS_KEY_ID="..."        (PowerShell tab)
//   [profile]\naws_access_key_id = ...  (ini block)
//   {"Credentials":{"AccessKeyId":...}} (aws sts assume-role / get-session-token output)
//   {"AccessKeyId":..., ...}            (credential_process output)
pub fn parse(text: &str) -> Result<Parsed> {
    let raw = text.trim();
    if raw.is_empty() {
        return Err(Error::msg("Nothing to parse."));
    }

    let mut out = Parsed::default();
    let mut found = 0;
    if raw.starts_with('{') {
        let json: Value =
            serde_json::from_str(raw).map_err(|_| Error::msg("Looks like JSON but could not be parsed."))?;
        let c = json.get("Credentials").or_else(|| json.get("credentials")).unwrap_or(&json);
        out.access_key_id = json_str(c, &["AccessKeyId", "accessKeyId"]);
        out.secret_access_key = json_str(c, &["SecretAccessKey", "secretAccessKey"]);
        out.session_token = json_str(c, &["SessionToken", "sessionToken"]);
        out.expiration = json_str(c, &["Expiration", "expiration"]);
        out.region = json_str(&json, &["Region", "region"]);
        // JS counted keys even when undefined, so any JSON object counts as "found".
        found = 5;
    } else {
        static LINE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"^\s*(?:export\s+|set\s+|setx\s+|\$env:|\$Env:|SETX\s+)?\$?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+?)\s*$")
                .unwrap()
        });
        for line in raw.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let Some(m) = LINE.captures(line) else { continue };
            let name = m[1].to_uppercase();
            let Some(field) = key_field(&name).or_else(|| key_field(&format!("AWS_{name}"))) else { continue };
            let value = unquote(&m[2]);
            if value.is_empty() {
                continue;
            }
            found += 1;
            let slot = match field {
                "accessKeyId" => &mut out.access_key_id,
                "secretAccessKey" => &mut out.secret_access_key,
                "sessionToken" => &mut out.session_token,
                "region" => &mut out.region,
                _ => &mut out.expiration,
            };
            *slot = Some(value);
        }
    }

    // Name exactly what is missing — "could not parse" sends people hunting for a
    // format problem when the real cause is usually a half-copied block.
    let empty = |o: &Option<String>| o.as_deref().map_or(true, str::is_empty);
    let mut missing = vec![];
    if empty(&out.access_key_id) {
        missing.push("AWS_ACCESS_KEY_ID");
    }
    if empty(&out.secret_access_key) {
        missing.push("AWS_SECRET_ACCESS_KEY");
    }
    if !missing.is_empty() {
        return Err(Error::msg(if found > 0 {
            format!("Missing {}. Copy the whole block, not just part of it.", missing.join(" and "))
        } else {
            "No credentials found in that text. Paste the full block from your AWS access portal.".into()
        }));
    }
    // ASIA keys are temporary and are useless without their session token.
    if out.access_key_id.as_deref().unwrap().to_uppercase().starts_with("ASIA") && empty(&out.session_token) {
        return Err(Error::msg(
            "AWS_SESSION_TOKEN is missing. Temporary (ASIA…) keys need it — it is the long third line.",
        ));
    }
    out.expiration = out.expiration.as_deref().and_then(normalize_time);
    Ok(out)
}

pub fn set(parsed: Parsed) {
    let mut s = st();
    s.creds = Some(Creds {
        mode: Mode::Paste,
        profile: None,
        access_key_id: parsed.access_key_id.unwrap_or_default(),
        secret_access_key: parsed.secret_access_key.unwrap_or_default(),
        session_token: parsed.session_token.filter(|t| !t.is_empty()),
        expiration: parsed.expiration,
        expiry_observed: false,
        region: parsed.region,
        loaded_at: now_ms(),
        verified_at: None,
    });
    s.identity = None;
}

// Two missed 2-minute health checks. This is only the fallback for failures the OS
// does not report — a captive portal or a dropped VPN, where the interface still
// looks up. An outright network loss is reported immediately via mark_unreachable().
pub const VERIFY_STALE_S: i64 = 4 * 60;

// How long ago AWS last confirmed these credentials work, in seconds.
fn verified_ago(s: &State) -> Option<i64> {
    let v = s.creds.as_ref()?.verified_at?;
    Some(((now_ms() - v) as f64 / 1000.0).round() as i64)
}

fn expires_in(s: &State) -> Option<i64> {
    let exp = s.creds.as_ref()?.expiration.as_ref()?;
    let t = time_ms(exp)?;
    Some(((t - now_ms()) as f64 / 1000.0).round() as i64)
}

pub fn mark_unreachable(reason: &str) {
    let mut s = st();
    s.unreachable = Some(Unreachable { reason: reason.to_string() });
}

pub fn mark_reachable() {
    st().unreachable = None;
}

// "Online" is a claim about right now: AWS answered us recently and accepted these
// credentials, and nothing since has told us the connection is gone. Computed here so
// the window and the tray cannot disagree about it.
fn is_online(s: &State) -> bool {
    if s.creds.is_none() || s.unreachable.is_some() {
        return false;
    }
    if matches!(expires_in(s), Some(secs) if secs <= 0) {
        return false;
    }
    matches!(verified_ago(s), Some(ago) if ago <= VERIFY_STALE_S)
}

// Why we are not online, so the UI can distinguish "no network" from "not checked".
fn offline_reason(s: &State) -> Option<String> {
    s.creds.as_ref()?;
    if let Some(u) = &s.unreachable {
        return Some(u.reason.clone());
    }
    if matches!(expires_in(s), Some(secs) if secs <= 0) {
        return Some("AWS has rejected these credentials.".into());
    }
    match verified_ago(s) {
        None => Some("AWS has not confirmed these credentials yet.".into()),
        Some(ago) if ago > VERIFY_STALE_S => Some(format!(
            "AWS has not answered for {} minutes, so these credentials cannot be confirmed.",
            (ago as f64 / 60.0).round() as i64
        )),
        _ => None,
    }
}

// Profile mode: the CLI resolves the credentials itself from ~/.aws, so this app
// never sees or holds a secret at all. It also means the CLI can refresh an SSO
// session on its own, which pasted credentials can never do.
pub fn set_profile(name: &str, region: Option<String>) -> Result<()> {
    if name.is_empty() {
        return Err(Error::msg("Pick a profile."));
    }
    let region = match region {
        Some(r) if !r.is_empty() => Some(valid_region(&r)?),
        _ => None,
    };
    let mut s = st();
    s.creds = Some(Creds {
        mode: Mode::Profile,
        profile: Some(name.to_string()),
        access_key_id: String::new(),
        secret_access_key: String::new(),
        session_token: None,
        expiration: None,
        expiry_observed: false,
        region,
        loaded_at: now_ms(),
        verified_at: None,
    });
    s.identity = None;
    Ok(())
}

// Lets a profile switch be undone if the new profile fails verification, so a typo
// in ~/.aws does not cost the user their working credentials. Memory only.
pub struct Snapshot(Option<Creds>, Option<Identity>);

pub fn snapshot() -> Snapshot {
    let s = st();
    Snapshot(s.creds.clone(), s.identity.clone())
}

impl Snapshot {
    pub fn has_creds(&self) -> bool {
        self.0.is_some()
    }
}

pub fn restore(snap: Snapshot) {
    let mut s = st();
    s.creds = snap.0;
    s.identity = snap.1;
}

pub fn is_profile() -> bool {
    matches!(st().creds.as_ref(), Some(c) if c.mode == Mode::Profile)
}

pub fn profile_name() -> Option<String> {
    st().creds.as_ref().and_then(|c| c.profile.clone())
}

// Only meaningful in profile mode, where the CLI can be asked when the resolved
// credentials expire.
pub fn set_expiration(iso: Option<String>) {
    let Some(iso) = iso else { return };
    let mut s = st();
    if let (Some(c), Some(t)) = (s.creds.as_mut(), normalize_time(&iso)) {
        c.expiration = Some(t);
    }
}

// There is no API that reveals a token's expiry, so when AWS rejects credentials
// as expired that rejection is the only authoritative signal we ever get. Record
// it so the UI and tray can stop pretending the session is still good.
pub fn mark_expired() {
    let mut s = st();
    if let Some(c) = s.creds.as_mut() {
        let t = DateTime::from_timestamp_millis(now_ms() - 1000).unwrap_or_default();
        c.expiration = Some(t.to_rfc3339_opts(SecondsFormat::Millis, true));
        c.expiry_observed = true;
    }
}

pub fn set_identity(id: Identity) {
    st().identity = Some(id);
}

// Who these credentials are, reduced to the parts that must still match for a tunnel
// opened earlier to keep meaning anything: the account, the role or user, and the
// region. A renewal of the same access keeps reaching the same instance; a different
// account, role or region does not.
//
// The session name is deliberately dropped. get-caller-identity returns an
// assumed-role ARN as arn:aws:sts::123:assumed-role/Role/<session>, and that last
// segment is fresh on every renewal — comparing raw ARNs would call every legitimate
// refresh a different identity, which is precisely the case this exists to permit.
fn principal_of(id: &Identity, reg: String) -> Option<Principal> {
    if id.arn.is_empty() {
        return None;
    }
    static ASSUMED: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^arn:[^:]*:sts::(\d+):assumed-role/([^/]+)/").unwrap());
    static IAM: Lazy<Regex> = Lazy::new(|| Regex::new(r"^arn:[^:]*:iam::\d+:").unwrap());
    let assumed = ASSUMED.captures(&id.arn);
    Some(Principal {
        account: id.account.clone().or_else(|| assumed.as_ref().map(|m| m[1].to_string())),
        who: Some(match &assumed {
            Some(m) => format!("assumed-role/{}", &m[2]),
            None => IAM.replace(&id.arn, "").into_owned(),
        }),
        region: Some(reg),
    })
}

pub fn principal() -> Option<Principal> {
    let s = st();
    s.creds.as_ref()?;
    let id = s.identity.clone()?;
    let reg = region_of(&s);
    drop(s);
    principal_of(&id, reg)
}

// Some(true/false) or None, where None means at least one side is unknown. Callers
// must treat None as "do not act": neither resuming a tunnel nor declaring it
// orphaned is safe on a guess about which credentials it belongs to.
pub fn same_principal(a: Option<&Principal>, b: Option<&Principal>) -> Option<bool> {
    let (a, b) = (a?, b?);
    Some(a.account == b.account && a.who == b.who && a.region == b.region)
}

pub fn describe_principal(p: Option<&Principal>) -> String {
    let Some(p) = p else { return "an unknown identity".into() };
    let where_ = p.region.as_ref().map(|r| format!(" in {r}")).unwrap_or_default();
    format!(
        "{} (account {}){}",
        p.who.as_deref().unwrap_or("unknown"),
        p.account.as_deref().unwrap_or("unknown"),
        where_
    )
}

// The last time AWS actually accepted these credentials. Without this the UI can
// only report what it was told at load time, which is how it came to claim "Active"
// after 21 hours.
pub fn mark_verified() {
    let mut s = st();
    if let Some(c) = s.creds.as_mut() {
        c.verified_at = Some(now_ms());
    }
    s.unreachable = None;
}

pub fn valid_region(next: &str) -> Result<String> {
    let r = next.trim();
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-z]{2}(-[a-z]+)+-\d$").unwrap());
    if !RE.is_match(r) {
        return Err(Error::msg(format!("Not a valid region: {r}")));
    }
    Ok(r.to_string())
}

// Pasted blocks often omit the region, so the UI has to be able to correct it
// without making the user re-paste their credentials.
pub fn set_region(next: &str) -> Result<String> {
    let r = valid_region(next)?;
    let mut s = st();
    let c = s.creds.as_mut().ok_or_else(|| Error::msg("No credentials loaded."))?;
    c.region = Some(r.clone());
    Ok(r)
}

// The region remembered from the previous session. Applied when the paste does
// not carry one, and settable before any credentials exist.
pub fn set_default_region(next: &str) -> Result<String> {
    let r = valid_region(next)?;
    st().default_region = Some(r.clone());
    Ok(r)
}

pub fn clear() {
    let mut s = st();
    s.creds = None; // Drop scrubs the secrets
    s.identity = None;
    s.unreachable = None;
}

pub fn has() -> bool {
    st().creds.is_some()
}

// Precedence: the paste, then the region remembered from last session, then the
// surrounding environment, then a final fallback.
fn region_of(s: &State) -> String {
    s.creds
        .as_ref()
        .and_then(|c| c.region.clone())
        .or_else(|| s.default_region.clone())
        .or_else(|| std::env::var("AWS_REGION").ok().filter(|v| !v.is_empty()))
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "eu-west-1".into())
}

pub fn region() -> String {
    region_of(&st())
}

pub type Env = HashMap<String, String>;

// Environment for spawned aws/session-manager-plugin processes.
pub fn env() -> Result<Env> {
    let s = st();
    let c = s.creds.as_ref().ok_or_else(|| Error::msg("No credentials loaded."))?;
    let mut e: Env = std::env::vars().collect();

    // A profile must be *absent*, not empty: botocore reads AWS_PROFILE='' as a
    // profile literally named "", then fails with "The config profile () could not
    // be found". Removing the keys is what actually forces env-var credentials.
    // Any inherited session token would otherwise outlive a key/secret-only paste
    // and get mismatched against it. Inherited keys take precedence over a profile
    // in botocore, so they have to go for profile mode to mean anything.
    for k in [
        "AWS_PROFILE",
        "AWS_DEFAULT_PROFILE",
        "AWS_SESSION_TOKEN",
        "AWS_SECURITY_TOKEN",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
    ] {
        e.remove(k);
    }

    if c.mode == Mode::Profile {
        e.insert("AWS_PROFILE".into(), c.profile.clone().unwrap_or_default());
    } else {
        e.insert("AWS_ACCESS_KEY_ID".into(), c.access_key_id.clone());
        e.insert("AWS_SECRET_ACCESS_KEY".into(), c.secret_access_key.clone());
        if let Some(t) = &c.session_token {
            e.insert("AWS_SESSION_TOKEN".into(), t.clone());
        }
    }
    let r = region_of(&s);
    e.insert("AWS_DEFAULT_REGION".into(), r.clone());
    e.insert("AWS_REGION".into(), r);
    e.insert("AWS_EC2_METADATA_DISABLED".into(), "true".into());
    // So the CLI can locate session-manager-plugin when we were launched from the
    // desktop rather than a shell.
    e.insert("PATH".into(), crate::binaries::augmented_path());
    Ok(e)
}

// A tunnel that runs as a named profile instead of the loaded credentials — e.g. a
// role that is allowed ssm:StartSession on the jump host when the loaded identity is
// not. Same scrubbing as env(); only the profile and its own region differ.
pub fn env_for_profile(name: &str, profile_region: Option<&str>) -> Result<Env> {
    let mut e = env()?;
    for k in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_SECURITY_TOKEN"] {
        e.remove(k);
    }
    e.insert("AWS_PROFILE".into(), name.to_string());
    if let Some(r) = profile_region {
        e.insert("AWS_DEFAULT_REGION".into(), r.to_string());
        e.insert("AWS_REGION".into(), r.to_string());
    }
    Ok(e)
}

pub fn expired() -> bool {
    matches!(expires_in(&st()), Some(secs) if secs <= 0)
}

// The only credential-shaped thing the renderer is allowed to see.
pub fn summary() -> Value {
    let s = st();
    // Even with nothing loaded, report the region so the UI can show the one
    // remembered from last session.
    let Some(c) = s.creds.as_ref() else {
        return json!({ "loaded": false, "online": false, "region": region_of(&s) });
    };
    let secs = expires_in(&s);
    let profile_mode = c.mode == Mode::Profile;
    let tail: String = {
        let chars: Vec<char> = c.access_key_id.chars().collect();
        chars[chars.len().saturating_sub(4)..].iter().collect()
    };
    let id = s.identity.as_ref();
    json!({
        "loaded": true,
        "mode": if profile_mode { "profile" } else { "paste" },
        "profile": if profile_mode { c.profile.clone() } else { None },
        // In profile mode there is no key here to redact — the CLI holds it.
        "accessKeyIdRedacted": if profile_mode { None } else { Some(format!("****************{tail}")) },
        "hasSessionToken": c.session_token.is_some(),
        "region": region_of(&s),
        "regionSupplied": c.region.is_some(),
        "expiration": c.expiration,
        "expiresInSeconds": secs,
        "expired": matches!(secs, Some(x) if x <= 0),
        // How we know the expiry, so the UI can be honest instead of guessing:
        // 'supplied' came from the paste or was reported by the CLI for a profile,
        // 'observed' means AWS rejected it as expired, 'unknown' means it genuinely
        // cannot be determined.
        "expirySource": if c.expiry_observed { "observed" } else if c.expiration.is_some() { "supplied" } else { "unknown" },
        "loadedAt": c.loaded_at,
        "verifiedAt": c.verified_at,
        "verifiedAgoSeconds": verified_ago(&s),
        // Whether AWS has confirmed these credentials recently enough to be trusted now.
        "online": is_online(&s),
        "offlineReason": offline_reason(&s),
        "staleAfterSeconds": VERIFY_STALE_S,
        "account": id.and_then(|i| i.account.clone()),
        "arn": id.map(|i| i.arn.clone()),
        "username": id.map(|i| i.username.clone()),
    })
}
