// Which OS processes this app started.
//
// The AWS CLI spawns session-manager-plugin as a child, and that child is what
// actually binds the local port. Killing the CLI does not always take the plugin
// with it, so a stopped tunnel can leave an orphan still holding the port — which
// is why a reconnect on the same port fails.
//
// Every pid we spawn, and every descendant we discover, is recorded here. That is
// the ownership test: a port held by a pid in this registry is our own leftover and
// can be cleared without asking. Anything else belongs to the user or another tool,
// and requires explicit confirmation before we kill it.
//
// Deliberately pid-exact. A session-manager-plugin the user started in their own
// terminal is not ours, even though it has the same name and may sit on the same
// port, so it still prompts.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct Info {
    pub tunnel_id: String,
    // Which launch of that tunnel, so a sweep of an old session cannot take down
    // the processes of a prompt reconnect on the same record.
    pub gen: u64,
    #[allow(dead_code)]
    pub role: &'static str,
}

static REGISTRY: Mutex<Option<HashMap<u32, Info>>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut HashMap<u32, Info>) -> T) -> T {
    let mut g = REGISTRY.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

pub fn record(pid: u32, tunnel_id: &str, gen: u64, role: &'static str) {
    if pid == 0 {
        return;
    }
    with(|r| {
        r.insert(pid, Info { tunnel_id: tunnel_id.to_string(), gen, role });
    });
}

pub fn forget(pid: u32) {
    with(|r| {
        r.remove(&pid);
    });
}

// Drops pids that have exited. Without this the registry would grow forever, and a
// dead pid could eventually be reused by an unrelated process which we would then
// wrongly treat as our own.
pub fn prune() {
    let pids: Vec<u32> = with(|r| r.keys().copied().collect());
    let dead: Vec<u32> = pids.into_iter().filter(|p| !crate::procs::alive(*p)).collect();
    with(|r| {
        for p in dead {
            r.remove(&p);
        }
    });
}

pub fn is_owned(pid: u32) -> bool {
    with(|r| r.contains_key(&pid))
}

pub fn by_tunnel(tunnel_id: &str) -> Vec<u32> {
    with(|r| {
        r.iter()
            .filter(|(_, v)| v.tunnel_id == tunnel_id)
            .map(|(pid, _)| *pid)
            .collect()
    })
}

pub fn by_launch(tunnel_id: &str, gen: u64) -> Vec<u32> {
    with(|r| {
        r.iter()
            .filter(|(_, v)| v.tunnel_id == tunnel_id && v.gen == gen)
            .map(|(pid, _)| *pid)
            .collect()
    })
}
