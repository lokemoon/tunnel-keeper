// `cargo test` — pure logic first, then the tunnel lifecycle driven end to end
// against tests/fake-aws, a stand-in CLI that binds real ports on both loopbacks.
// The lifecycle runs as one serial test: the core is built on process-wide state.

use crate::{aws, aws_config, connectivity, creds, owned, ports, procs, tunnels};
use serde_json::{json, Value};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- pure logic

#[test]
fn parse_accepts_every_console_shape() {
    let shells = [
        "export AWS_ACCESS_KEY_ID=\"ASIAAAAA1111\"\nexport AWS_SECRET_ACCESS_KEY=\"sec\"\nexport AWS_SESSION_TOKEN=\"tok\"",
        "set AWS_ACCESS_KEY_ID=ASIAAAAA1111\r\nset AWS_SECRET_ACCESS_KEY=sec\r\nset AWS_SESSION_TOKEN=tok",
        "$env:AWS_ACCESS_KEY_ID=\"ASIAAAAA1111\";\n$env:AWS_SECRET_ACCESS_KEY=\"sec\";\n$env:AWS_SESSION_TOKEN=\"tok\";",
        "[123_Dev]\naws_access_key_id = ASIAAAAA1111\naws_secret_access_key = sec\naws_session_token = tok",
        r#"{"Credentials":{"AccessKeyId":"ASIAAAAA1111","SecretAccessKey":"sec","SessionToken":"tok","Expiration":"2030-01-02T03:04:05+00:00"}}"#,
    ];
    for text in shells {
        let p = creds::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(p.access_key_id.as_deref(), Some("ASIAAAAA1111"), "{text}");
        assert_eq!(p.secret_access_key.as_deref(), Some("sec"), "{text}");
        assert_eq!(p.session_token.as_deref(), Some("tok"), "{text}");
    }
    let p = creds::parse(shells[4]).unwrap();
    assert_eq!(p.expiration.as_deref(), Some("2030-01-02T03:04:05.000Z"));
}

#[test]
fn parse_names_what_is_missing() {
    let e = creds::parse("export AWS_ACCESS_KEY_ID=AKIA1").err().unwrap();
    assert!(e.message.contains("Missing AWS_SECRET_ACCESS_KEY"), "{}", e.message);
    let e = creds::parse("hello world").err().unwrap();
    assert!(e.message.starts_with("No credentials found"), "{}", e.message);
    let e = creds::parse("AWS_ACCESS_KEY_ID=ASIA1\nAWS_SECRET_ACCESS_KEY=s").err().unwrap();
    assert!(e.message.contains("AWS_SESSION_TOKEN is missing"), "{}", e.message);
    let e = creds::parse("{nope").err().unwrap();
    assert_eq!(e.message, "Looks like JSON but could not be parsed.");
    assert!(creds::parse("   ").is_err());
}

#[test]
fn principal_ignores_session_name() {
    let a = Some(creds::Principal { account: Some("1".into()), who: Some("assumed-role/R".into()), region: Some("eu-west-1".into()) });
    let b = a.clone();
    assert_eq!(creds::same_principal(a.as_ref(), b.as_ref()), Some(true));
    assert_eq!(creds::same_principal(a.as_ref(), None), None);
    let mut c = a.clone().unwrap();
    c.region = Some("us-east-1".into());
    assert_eq!(creds::same_principal(a.as_ref(), Some(&c)), Some(false));
}

#[test]
fn host_normalisation() {
    let n = |s: &str| tunnels::normalize_host(Some(s));
    assert_eq!(n("").unwrap(), None);
    assert_eq!(n("https://ABC.gr7.eu-west-1.eks.amazonaws.com/").unwrap().unwrap(), "ABC.gr7.eu-west-1.eks.amazonaws.com");
    assert_eq!(n("10.0.1.5").unwrap().unwrap(), "10.0.1.5");
    assert_eq!(n("[fd00::1]").unwrap().unwrap(), "fd00::1");
    assert!(n("bad host!").is_err());
    assert!(n("-leading.example").is_err());
}

#[test]
fn regions_are_validated() {
    assert!(creds::valid_region("eu-west-1").is_ok());
    assert!(creds::valid_region("us-gov-west-1").is_ok());
    assert!(creds::valid_region("Ireland").is_err());
}

#[test]
fn explain_maps_cli_errors() {
    assert!(aws::explain("An error occurred (ExpiredToken) when calling").contains("expired"));
    assert!(aws::explain("Could not connect to the endpoint URL").contains("Cannot reach AWS"));
    assert!(aws::explain("The config profile () could not be found").contains("stale AWS_PROFILE"));
    assert_eq!(aws::explain(""), "The AWS CLI failed with no output.");
}

#[test]
fn ini_profiles_merge_and_hide_secrets() {
    let ini = aws_config::parse_ini("[default]\nregion = eu-west-1 # c\n[profile sso]\nsso_session = x\n[sso-session x]\nsso_region=eu-west-1\n");
    assert_eq!(ini.len(), 3);
    assert_eq!(ini[0].1.get("region").map(String::as_str), Some("eu-west-1"));
}

// ---------------------------------------------------------------- connectivity

#[test]
fn connectivity_settles_and_respects_foreground() {
    let rt = tauri::async_runtime::handle();
    let state = Arc::new(Mutex::new(json!({"creds": {"loaded": true, "online": true}, "tunnels": []})));
    let fg = Arc::new(Mutex::new(false));
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let (s, f, c1, c2) = (state.clone(), fg.clone(), calls.clone(), calls.clone());
    let conn = connectivity::Connectivity::new(connectivity::Deps {
        is_foreground: Box::new(move || *f.lock().unwrap()),
        get_state: Box::new(move || s.lock().unwrap().clone()),
        on_lost: Box::new(move |r, _| c1.lock().unwrap().push(format!("lost:{r}"))),
        on_restored: Box::new(move |_| c2.lock().unwrap().push("restored".into())),
        settle_ms: 300,
    });
    let set = |online: bool| {
        *state.lock().unwrap() = json!({"creds": {"loaded": true, "online": online, "offlineReason": "gone"}, "tunnels": []});
        conn.observe(&state.lock().unwrap().clone());
    };
    rt.block_on(async {
        set(true); // first observation is adopted silently
        set(false);
        set(true); // a blip shorter than the settle window
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(calls.lock().unwrap().is_empty(), "blip announced: {:?}", calls.lock().unwrap());

        set(false);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(*calls.lock().unwrap(), vec!["lost:gone".to_string()]);

        *fg.lock().unwrap() = true; // the window is the notification
        set(true);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls.lock().unwrap().len(), 1);
    });
}

// ---------------------------------------------------------------- ports

fn spawn_listener(port: u16, v6_only: bool) -> Child {
    let code = format!(
        "import socket,time\nfams=[(socket.AF_INET6,'::1')]{}\nk=[]\nfor f,h in fams:\n s=socket.socket(f);s.bind((h,{port}));s.listen(1);k.append(s)\nprint('up',flush=True)\ntime.sleep(60)",
        if v6_only { "" } else { "+[(socket.AF_INET,'127.0.0.1')]" }
    );
    let child = Command::new("python3").args(["-c", &code]).stdout(std::process::Stdio::null()).spawn().unwrap();
    wait_until(Duration::from_secs(5), || ports::in_use(port));
    child
}

// Detached (double-forked), so it is genuinely someone else's process: a child of
// the test would linger as an unreaped zombie after being killed and read as alive.
fn spawn_foreign_listener(port: u16) -> u32 {
    let code = format!(
        "import socket,time\ns=socket.socket(socket.AF_INET6);s.bind(('::1',{port}));s.listen(1)\ntime.sleep(60)"
    );
    let script = format!("nohup python3 -c \"{code}\" >/dev/null 2>&1 &");
    Command::new("sh").args(["-c", &script]).status().unwrap();
    wait_until(Duration::from_secs(5), || ports::in_use(port));
    tauri::async_runtime::block_on(ports::listeners(port))[0].pid
}

fn free_port() -> u16 {
    ports::find_free_port().unwrap()
}

#[test]
fn ipv6_only_listener_counts_as_in_use() {
    let port = free_port();
    assert!(!ports::in_use(port));
    let mut child = spawn_listener(port, true);
    assert!(ports::in_use(port), "a [::1] listener must block the port");
    let c = tauri::async_runtime::block_on(ports::conflict(port));
    assert_eq!(c.holders.len(), 1);
    assert_eq!(c.holders[0].families, vec!["IPv6".to_string()]);
    assert!(!c.ours);
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------- tunnel lifecycle

fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    f()
}

fn meta(id: &str) -> tunnels::Meta {
    tunnels::list_meta().into_iter().find(|t| t.id == id).expect("tunnel exists")
}

fn status(id: &str) -> String {
    meta(id).status
}

fn await_status(id: &str, want: &str, secs: u64) {
    let ok = wait_until(Duration::from_secs(secs), || status(id) == want);
    assert!(ok, "{id}: wanted {want}, have {} ({:?})", status(id), meta(id).reason);
}

fn start(port: Option<u16>, allow: bool) -> crate::error::Result<Value> {
    let opts: tunnels::StartOpts = serde_json::from_value(json!({
        "instanceId": "i-0123", "instanceName": "fake", "remotePort": 5432,
        "localPort": port, "allowForeignKill": allow,
    }))
    .unwrap();
    tauri::async_runtime::block_on(tunnels::start(opts))
}

fn reconnect(id: &str, allow: bool) -> crate::error::Result<Value> {
    tauri::async_runtime::block_on(tunnels::reconnect(id, allow))
}

fn set_mode(mode: &str) {
    std::env::set_var("FAKE_MODE", mode);
}

#[test]
fn tunnel_lifecycle_against_fake_cli() {
    let fake = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-aws");
    std::env::set_var("PATH", format!("{}:{}", fake.display(), std::env::var("PATH").unwrap_or_default()));
    set_mode("ok");

    let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let ev = events.clone();
    tunnels::on_event(move |k, v| ev.lock().unwrap().push((k.to_string(), v)));
    let saw = |kind: &str| events.lock().unwrap().iter().filter(|(k, _)| k == kind).count();

    creds::set(creds::parse("AWS_ACCESS_KEY_ID=AKIATEST\nAWS_SECRET_ACCESS_KEY=secret\nAWS_REGION=eu-west-1").unwrap());
    creds::set_identity(tauri::async_runtime::block_on(aws::get_caller_identity()).expect("stub identity"));
    creds::mark_verified();
    assert_eq!(creds::principal().unwrap().who.as_deref(), Some("assumed-role/Dev"));

    // Open, then stop: the port must be released and nothing of ours left behind.
    let t = start(None, false).expect("start");
    let id = t["id"].as_str().unwrap().to_string();
    let port = t["localPort"].as_u64().unwrap() as u16;
    await_status(&id, "open", 10);
    assert!(meta(&id).session_id.unwrap().starts_with("fake-"));
    assert!(ports::in_use(port));
    tunnels::stop(&id);
    await_status(&id, "stopped", 5);
    assert!(wait_until(Duration::from_secs(4), || !ports::in_use(port)), "port still held after stop");

    // A stopped card still owns its port.
    let e = start(Some(port), false).unwrap_err();
    assert!(e.message.contains("already assigned"), "{}", e.message);

    // Same-port reconnect, then stop + immediate reconnect: the first stop's delayed
    // sweep must not take down the new session.
    reconnect(&id, false).expect("reconnect");
    await_status(&id, "open", 10);
    assert_eq!(meta(&id).local_port, port);
    tunnels::stop(&id);
    await_status(&id, "stopped", 5);
    reconnect(&id, false).expect("prompt reconnect");
    await_status(&id, "open", 10);
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(status(&id), "open", "the old sweep killed the new session");
    assert!(ports::in_use(port));
    tunnels::stop(&id);
    await_status(&id, "stopped", 5);
    tunnels::remove(&id);

    // Our own orphan holding the port is cleared without asking.
    set_mode("orphan");
    let port2 = free_port();
    let t = start(Some(port2), false).expect("start orphan");
    let id2 = t["id"].as_str().unwrap().to_string();
    await_status(&id2, "open", 10);
    std::thread::sleep(Duration::from_millis(3000)); // let track_children record the plugin
    let cli_pid = owned::by_tunnel(&id2).into_iter().min().unwrap();
    // The CLI dies on its own; the detached child keeps the port.
    unsafe { libc::kill(cli_pid as i32, libc::SIGKILL) };
    await_status(&id2, "error", 5);
    assert!(meta(&id2).dropped);
    assert!(ports::in_use(port2), "orphan should still hold the port");
    set_mode("ok");
    let r = reconnect(&id2, false).expect("reconnect over own orphan");
    assert_eq!(r["cleared"]["ours"], json!(true));
    await_status(&id2, "open", 10);
    tunnels::stop(&id2);
    await_status(&id2, "stopped", 5);
    tunnels::remove(&id2);

    // A foreign IPv6-only listener is a question, not an automatic kill.
    let port3 = free_port();
    let foreign = spawn_foreign_listener(port3);
    let e = start(Some(port3), false).unwrap_err();
    let c = e.conflict.as_ref().expect("needs confirmation");
    assert!(!c.ours && c.holders[0].families == vec!["IPv6".to_string()]);
    assert!(procs::alive(foreign), "foreign listener was killed without asking");
    let t = start(Some(port3), true).expect("start after approval");
    let id3 = t["id"].as_str().unwrap().to_string();
    assert_eq!(t["cleared"]["ours"], json!(false));
    assert!(!procs::alive(foreign));
    await_status(&id3, "open", 10);

    // A zombie declared dead with auto-reconnect: it reconnects, and the late exit
    // of the killed process does not overwrite the retry.
    tunnels::set_auto_reconnect(&id3, true).unwrap();
    tunnels::report_dead(&id3, "AWS reports this SSM session is no longer active.".into());
    assert_eq!(status(&id3), "reconnecting");
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(status(&id3), "reconnecting", "late exit overwrote the retry");
    await_status(&id3, "open", 10);

    // Expiry suspends; a renewal of the same identity resumes with auto-reconnect.
    tunnels::suspend_for_expiry();
    await_status(&id3, "stopped", 5);
    assert!(meta(&id3).suspended);
    std::thread::sleep(Duration::from_millis(2000));
    tauri::async_runtime::block_on(tunnels::review_credentials());
    await_status(&id3, "open", 10);
    assert!(meta(&id3).auto_reconnect, "auto-reconnect not restored");

    // A different identity marks the card stale and asks; it does not tear it down.
    creds::set_identity(creds::Identity {
        account: Some("999".into()),
        arn: "arn:aws:sts::999:assumed-role/Other/s".into(),
        user_id: None,
        username: "s".into(),
    });
    tauri::async_runtime::block_on(tunnels::review_credentials());
    assert!(meta(&id3).stale);
    assert_eq!(status(&id3), "open", "a live stale tunnel keeps its status");
    assert_eq!(saw("credentials-changed"), 1);
    assert_eq!(tunnels::remove_stale(), 1);
    assert!(wait_until(Duration::from_secs(4), || !ports::in_use(port3)));

    // A drop without auto-reconnect is reported with the CLI's reason.
    creds::set_identity(tauri::async_runtime::block_on(aws::get_caller_identity()).unwrap());
    set_mode("drop");
    let t = start(None, false).expect("start drop");
    let id4 = t["id"].as_str().unwrap().to_string();
    await_status(&id4, "error", 10);
    let m = meta(&id4);
    assert!(m.dropped && m.reason.as_deref().unwrap_or("").contains("TargetNotConnected"), "{:?}", m.reason);
    assert_eq!(saw("dropped"), 2); // the orphan step dropped once too

    // A flapping session with auto-reconnect gives up after MAX_ATTEMPTS.
    tunnels::set_auto_reconnect(&id4, true).unwrap();
    reconnect(&id4, false).expect("reconnect flapper");
    let gave_up = wait_until(Duration::from_secs(40), || saw("gaveup") == 1);
    assert!(gave_up, "never gave up: {} {:?}", status(&id4), meta(&id4).reason);
    let m = meta(&id4);
    assert_eq!(m.status, "error");
    assert!(m.reason.unwrap().contains("Gave up after 3"));
    assert_eq!(saw("restored"), 0, "a flapping session must not announce recovery");
    tunnels::remove(&id4);

    tunnels::stop_all();
    std::thread::sleep(Duration::from_millis(2000));
    creds::clear();
}
