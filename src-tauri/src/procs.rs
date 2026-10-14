// Process discovery and termination. POSIX signals directly via libc; on Windows,
// OpenProcess for liveness and `taskkill /T` for trees, as before.

use crate::binaries;
use std::time::Duration;

fn own_pids() -> (u32, u32) {
    let me = std::process::id();
    #[cfg(unix)]
    let parent = unsafe { libc::getppid() } as u32;
    #[cfg(not(unix))]
    let parent = 0u32;
    (me, parent)
}

// The app must never signal itself, whoever asks.
pub fn is_self(pid: u32) -> bool {
    let (me, parent) = own_pids();
    pid == me || (parent != 0 && pid == parent)
}

// Direct children of a pid. Used to record the session-manager-plugin that the AWS
// CLI spawns, so we can recognise and clean up our own leftovers later.
pub async fn children(pid: u32) -> Vec<u32> {
    let out = if cfg!(windows) {
        let filter = format!(
            "Get-CimInstance Win32_Process -Filter \"ParentProcessId={pid}\" | Select-Object -ExpandProperty ProcessId"
        );
        binaries::run("powershell", &["-NoProfile", "-NonInteractive", "-Command", &filter], 5000).await
    } else {
        binaries::run("pgrep", &["-P", &pid.to_string()], 5000).await
    };
    out.split_whitespace().filter_map(|s| s.parse::<u32>().ok()).filter(|n| *n > 0).collect()
}

// Every descendant, not just direct children, so a whole tunnel's process tree can
// be accounted for.
pub async fn descendants(pid: u32) -> Vec<u32> {
    let mut all: Vec<u32> = Vec::new();
    let mut frontier = vec![pid];
    for _depth in 0..=4 {
        let mut next = Vec::new();
        for p in frontier {
            for kid in children(p).await {
                if !all.contains(&kid) {
                    all.push(kid);
                    next.push(kid);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    all
}

#[cfg(unix)]
fn signal(pid: i32, sig: i32) -> std::result::Result<(), i32> {
    let r = unsafe { libc::kill(pid, sig) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match signal(pid as i32, 0) {
            Ok(()) => true,
            // EPERM means it exists but belongs to someone else.
            Err(e) => e == libc::EPERM,
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                // Exists, but owned by someone we cannot query.
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }
}

// Whether this user could actually signal the process. A process owned by another
// user (or root) reports EPERM, and no amount of retrying changes that.
pub fn can_signal(pid: u32) -> bool {
    #[cfg(unix)]
    {
        !matches!(signal(pid as i32, 0), Err(e) if e == libc::EPERM)
    }
    #[cfg(windows)]
    {
        let _ = pid;
        true // decided by taskkill's exit code
    }
}

// SIGTERM (or SIGKILL) a whole process group. Best-effort, like `process.kill(-pgid)`.
#[cfg(unix)]
pub fn kill_group(pgid: u32, force: bool) {
    let _ = signal(-(pgid as i32), if force { libc::SIGKILL } else { libc::SIGTERM });
}

#[cfg(unix)]
pub fn kill_pid(pid: u32, force: bool) {
    if is_self(pid) {
        return;
    }
    let _ = signal(pid as i32, if force { libc::SIGKILL } else { libc::SIGTERM });
}

pub struct Outcome {
    pub ok: bool,
    pub error: Option<String>,
}

fn ok() -> Outcome {
    Outcome { ok: true, error: None }
}

fn fail(e: impl Into<String>) -> Outcome {
    Outcome { ok: false, error: Some(e.into()) }
}

// Ask politely, then insist. Returns {ok, error} rather than failing so callers
// can turn a failure straight into a message.
pub async fn terminate(pid: u32, grace_ms: u64) -> Outcome {
    // Refuse to shoot ourselves in the foot, no matter who asked.
    if is_self(pid) {
        return fail("That is this application itself.");
    }
    if !alive(pid) {
        return ok();
    }

    #[cfg(windows)]
    {
        let _ = grace_ms;
        // /T takes the process tree with it, /F forces it.
        let mut cmd = binaries::command("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        let out = cmd.output().await.map(|o| {
            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        if !alive(pid) {
            return ok();
        }
        let text = out.unwrap_or_default().to_lowercase();
        return fail(if text.contains("denied") || text.contains("elevat") {
            "Access denied — it is owned by another user or needs Administrator rights."
        } else {
            "The process did not exit."
        });
    }

    #[cfg(unix)]
    {
        if !can_signal(pid) {
            return fail("It belongs to another user, so this app cannot stop it.");
        }
        match signal(pid as i32, libc::SIGTERM) {
            Ok(()) => {}
            Err(e) if e == libc::ESRCH => return ok(),
            Err(e) if e == libc::EPERM => {
                return fail("It belongs to another user, so this app cannot stop it.")
            }
            Err(e) => return fail(std::io::Error::from_raw_os_error(e).to_string()),
        }

        let deadline = std::time::Instant::now() + Duration::from_millis(grace_ms);
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !alive(pid) {
                return ok();
            }
        }

        let _ = signal(pid as i32, libc::SIGKILL);
        tokio::time::sleep(Duration::from_millis(300)).await;
        if alive(pid) {
            fail("The process would not exit.")
        } else {
            ok()
        }
    }
}
