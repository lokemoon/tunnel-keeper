// IPv4 and IPv6 loopback are separate address spaces: a process on [::1]:5985 does
// not block a bind to 127.0.0.1:5985. Both must be checked, because clients resolving
// "localhost" may pick either — on macOS and Windows ::1 is usually tried first — so a
// tunnel sharing a port with a foreign IPv6 listener silently routes to the wrong one.

use crate::error::{Error, Result};
use crate::{binaries, owned, procs};
use serde::Serialize;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};

const LOOPBACKS: [IpAddr; 2] = [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)];

#[derive(Clone, Debug, Serialize)]
pub struct Holder {
    pub name: Option<String>,
    pub pid: u32,
    pub families: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub port: u16,
    pub holders: Vec<Holder>,
    pub foreign: Vec<Holder>,
    // The one to name in a button: a foreign holder if there is one, since that is
    // what the user has to decide about.
    pub holder: Option<Holder>,
    pub label: String,
    // Only ours if *every* listener is ours. One foreign IPv6 listener alongside our
    // own IPv4 one still needs the user's say-so.
    pub ours: bool,
    pub can_terminate: bool,
    pub message: String,
}

// A bind to the loopback also fails when something holds the wildcard address
// (0.0.0.0 / ::), so probing these two covers wildcard binds as well.
fn bindable(port: u16, host: IpAddr) -> bool {
    match TcpListener::bind(SocketAddr::new(host, port)) {
        Ok(_) => true,
        // No IPv6 stack at all, or the address is unavailable: that is not a conflict.
        Err(e) => {
            e.kind() == ErrorKind::AddrNotAvailable || is_af_not_supported(&e)
        }
    }
}

fn is_af_not_supported(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EAFNOSUPPORT)
    }
    #[cfg(windows)]
    {
        e.raw_os_error() == Some(10047) // WSAEAFNOSUPPORT
    }
}

// True if anything is listening on this port on either stack.
pub fn in_use(port: u16) -> bool {
    LOOPBACKS.iter().any(|h| !bindable(port, *h))
}

// Ask the OS for an unused ephemeral port, then confirm it is free on the other stack
// too. There is an unavoidable race between this and the CLI binding it; the caller
// reports the bind failure if it loses.
pub fn find_free_port() -> Result<u16> {
    for _ in 0..20 {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .map_err(|e| Error::msg(e.to_string()))?;
        if !in_use(port) {
            return Ok(port);
        }
    }
    Err(Error::msg("Could not find a local port free on both IPv4 and IPv6."))
}

struct Row {
    name: Option<String>,
    pid: u32,
    family: String,
}

// Everything listening on this port, across both address families. Returning only the
// first match is what hid the IPv6 listener behind our own IPv4 one.
pub async fn listeners(port: u16) -> Vec<Holder> {
    let rows = if cfg!(windows) { listeners_windows(port).await } else { listeners_unix(port).await };
    // Same process on both stacks is one holder as far as the user is concerned.
    let mut out: Vec<Holder> = Vec::new();
    for r in rows {
        if let Some(h) = out.iter_mut().find(|h| h.pid == r.pid) {
            h.families.push(r.family);
        } else {
            out.push(Holder { name: r.name, pid: r.pid, families: vec![r.family] });
        }
    }
    out
}

async fn listeners_unix(port: u16) -> Vec<Row> {
    // -nP: no DNS or port-name lookups. +c 0: full command name — lsof truncates to
    // 9 characters by default, which turns session-manager-plugin into "session-m".
    let out = binaries::run("lsof", &["-nP", "+c", "0", &format!("-iTCP:{port}"), "-sTCP:LISTEN"], 4000).await;
    // COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
    let head = regex::Regex::new(r"^(.+?)\s+(\d+)\s+\S+\s+\S+\s+(IPv4|IPv6)\s").unwrap();
    // Only listeners on this exact port, not a foreign address that happens to match.
    let exact = regex::Regex::new(&format!(r"[:\]]{port}\b")).unwrap();
    let mut rows = Vec::new();
    for line in out.lines().skip(1) {
        let Some(m) = head.captures(line) else { continue };
        if !exact.is_match(line) {
            continue;
        }
        rows.push(Row {
            name: Some(m[1].trim().to_string()),
            pid: m[2].parse().unwrap_or(0),
            family: m[3].to_string(),
        });
    }
    rows
}

async fn listeners_windows(port: u16) -> Vec<Row> {
    // No `-p TCP`: that filters to IPv4 on some Windows versions, which would hide an
    // IPv6 listener exactly as the lsof first-match bug did.
    let out = binaries::run("netstat", &["-ano"], 4000).await;
    let mut found: Vec<(u32, Vec<String>)> = Vec::new();
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // Proto  Local Address  Foreign Address  State  PID
        if cols.len() < 5 || !cols[0].to_uppercase().starts_with("TCP") {
            continue;
        }
        if !cols[3].to_uppercase().contains("LISTENING") {
            continue;
        }
        let local = cols[1];
        let port_part = &local[local.rfind(':').map(|i| i + 1).unwrap_or(0)..];
        if port_part != port.to_string() {
            continue;
        }
        let Ok(pid) = cols[4].parse::<u32>() else { continue };
        if pid == 0 {
            continue;
        }
        let family = if local.starts_with('[') { "IPv6" } else { "IPv4" }.to_string();
        match found.iter_mut().find(|(p, _)| *p == pid) {
            Some((_, fams)) => {
                if !fams.contains(&family) {
                    fams.push(family)
                }
            }
            None => found.push((pid, vec![family])),
        }
    }

    let name_re = regex::Regex::new(r#"(?m)^"([^"]+)""#).unwrap();
    let mut rows = Vec::new();
    for (pid, families) in found {
        let tasks =
            binaries::run("tasklist", &["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"], 4000).await;
        let name = name_re.captures(&tasks).map(|m| m[1].to_string());
        for family in families {
            rows.push(Row { name: name.clone(), pid, family });
        }
    }
    rows
}

// Names the stack as well as the process, since which one a client reaches depends on
// how it resolves "localhost" — that ambiguity is the whole problem here.
pub fn name_of(who: &Holder) -> String {
    let mut parts = vec![format!("pid {}", who.pid)];
    if !who.families.is_empty() {
        parts.push(who.families.join("+"));
    }
    let detail = parts.join(", ");
    match &who.name {
        Some(n) => format!("{n} ({detail})"),
        None => format!("process {detail}"),
    }
}

fn list_names(all: &[Holder]) -> String {
    if all.is_empty() {
        return "another program".into();
    }
    all.iter().map(name_of).collect::<Vec<_>>().join(" and ")
}

// Who holds the port, and whether all of it is ours — the facts every caller needs to
// decide between clearing it and asking first.
pub async fn conflict(port: u16) -> Conflict {
    // Stale pids must not be mistaken for our own processes.
    owned::prune();

    let all = listeners(port).await;
    let foreign: Vec<Holder> = all.iter().filter(|h| !owned::is_owned(h.pid)).cloned().collect();
    let label = list_names(&all);
    Conflict {
        port,
        holder: foreign.first().cloned().or_else(|| all.first().cloned()),
        ours: !all.is_empty() && foreign.is_empty(),
        can_terminate: !all.is_empty() && all.iter().all(|h| procs::can_signal(h.pid)),
        message: format!("Local port {port} is already in use by {label}."),
        label,
        foreign,
        holders: all,
    }
}
