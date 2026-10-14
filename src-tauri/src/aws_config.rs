// Reads the shared AWS config so profiles can be listed without shelling out.
// Only non-secret fields are ever taken from these files — the app reads profile
// names, regions and how each profile gets its credentials, and never keeps
// aws_access_key_id or aws_secret_access_key (only whether they are present).

use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

pub fn config_path() -> PathBuf {
    std::env::var_os("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".aws").join("config"))
}

pub fn credentials_path() -> PathBuf {
    std::env::var_os("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".aws").join("credentials"))
}

type Section = HashMap<String, String>;

// Minimal ini reader: sections, `key = value`, `#`/`;` comments. Nested
// sub-sections (used by some SSO setups) are flattened, which is fine here since
// we only look at top-level keys. Order of first appearance is preserved.
pub fn parse_ini(text: &str) -> Vec<(String, Section)> {
    let mut out: Vec<(String, Section)> = Vec::new();
    let mut section: Option<usize> = None;
    let kv = regex::Regex::new(r"^([A-Za-z0-9_.-]+)\s*=\s*(.*)$").unwrap();
    for raw in text.lines() {
        let line = match raw.find(['#', ';']) {
            Some(i) => &raw[..i],
            None => raw,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && line.len() > 2 {
            let name = line[1..line.len() - 1].trim().to_string();
            section = Some(match out.iter().position(|(n, _)| *n == name) {
                Some(i) => i,
                None => {
                    out.push((name, Section::new()));
                    out.len() - 1
                }
            });
            continue;
        }
        let Some(i) = section else { continue };
        if let Some(m) = kv.captures(line) {
            out[i].1.insert(m[1].to_lowercase(), m[2].trim().to_string());
        }
    }
    out
}

fn read_ini(path: PathBuf) -> Vec<(String, Section)> {
    std::fs::read_to_string(path).map(|t| parse_ini(&t)).unwrap_or_default()
}

// How this profile obtains credentials — used to explain what to do when it fails.
fn source_of(cfg: &Section, has_static_keys: bool) -> &'static str {
    if cfg.contains_key("sso_session") || cfg.contains_key("sso_start_url") {
        "sso"
    } else if cfg.contains_key("credential_process") {
        "process"
    } else if cfg.contains_key("role_arn") {
        "role"
    } else if has_static_keys || cfg.contains_key("aws_access_key_id") {
        "keys"
    } else {
        "unknown"
    }
}

fn label(source: &str) -> &'static str {
    match source {
        "sso" => "SSO",
        "process" => "credential process",
        "role" => "assumed role",
        "keys" => "static keys",
        _ => "no credentials configured",
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub name: String,
    pub region: Option<String>,
    pub source: &'static str,
    pub source_label: &'static str,
    pub usable: bool,
}

// Every profile the CLI would recognise, merged across both files.
pub fn list() -> Vec<Profile> {
    let config = read_ini(config_path());
    let credentials = read_ini(credentials_path());
    let mut profiles: Vec<(String, Section, bool)> = Vec::new();

    let mut put = |name: String, cfg: Section, has_static: bool| {
        // Secrets are never carried past this point: only presence matters.
        let cfg: Section = cfg
            .into_iter()
            .filter(|(k, _)| k != "aws_secret_access_key" && k != "aws_session_token")
            .collect();
        match profiles.iter_mut().find(|(n, _, _)| *n == name) {
            Some((_, existing, hs)) => {
                existing.extend(cfg);
                *hs = *hs || has_static;
            }
            None => profiles.push((name, cfg, has_static)),
        }
    };

    let prefixed = regex::Regex::new(r"(?i)^profile\s+(.+)$").unwrap();
    let skip = regex::Regex::new(r"(?i)^(sso-session|services)\s").unwrap();
    for (section, cfg) in config {
        // `[default]` has no prefix; everything else is `[profile name]`.
        // `[sso-session x]` and `[services x]` are not profiles.
        if skip.is_match(&section) {
            continue;
        }
        if section == "default" {
            put("default".into(), cfg, false);
        } else if let Some(m) = prefixed.captures(&section) {
            put(m[1].trim().to_string(), cfg, false);
        }
    }

    for (section, cfg) in credentials {
        let has = cfg.contains_key("aws_access_key_id") && cfg.contains_key("aws_secret_access_key");
        put(section, cfg, has);
    }

    let mut out: Vec<Profile> = profiles
        .into_iter()
        .map(|(name, cfg, has_static)| {
            let source = source_of(&cfg, has_static);
            Profile {
                name,
                region: cfg.get("region").cloned().filter(|r| !r.is_empty()),
                source,
                source_label: label(source),
                usable: source != "unknown",
            }
        })
        .collect();
    out.sort_by(|a, b| match (a.name == "default", b.name == "default") {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    out
}

pub fn get(name: &str) -> Option<Profile> {
    list().into_iter().find(|p| p.name == name)
}
