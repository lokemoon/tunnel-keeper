// Announces online <-> offline transitions, with two rules that matter more than the
// announcement itself:
//
//   1. Only when the user cannot already see it. If the window is the active window,
//      the badge and the banner are the notification; a toast on top would be telling
//      them something they are looking at.
//   2. Only once the new state has held. Connectivity flaps, and a two-second blip
//      would otherwise fire "lost" then "restored" back to back.
//
// Dependencies are injected so this can be exercised without a real window, a real
// network, or real OS notifications.

use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const SETTLE_MS: u64 = 4000;

pub struct Deps {
    pub is_foreground: Box<dyn Fn() -> bool + Send + Sync>,
    pub get_state: Box<dyn Fn() -> Value + Send + Sync>,
    // (reason, live tunnel count)
    pub on_lost: Box<dyn Fn(String, usize) + Send + Sync>,
    pub on_restored: Box<dyn Fn(usize) + Send + Sync>,
    pub settle_ms: u64,
}

#[derive(Default)]
struct Inner {
    last_online: Option<bool>, // None until the first observation
    settling: Option<(bool, u64)>, // (to, token)
    token: u64,
}

#[derive(Clone)]
pub struct Connectivity {
    deps: Arc<Deps>,
    inner: Arc<Mutex<Inner>>,
}

fn online(state: &Value) -> bool {
    state["creds"]["online"].as_bool().unwrap_or(false)
}

fn loaded(state: &Value) -> bool {
    state["creds"]["loaded"].as_bool().unwrap_or(false)
}

impl Connectivity {
    pub fn new(deps: Deps) -> Self {
        Connectivity { deps: Arc::new(deps), inner: Arc::new(Mutex::new(Inner::default())) }
    }

    pub fn observe(&self, state: &Value) {
        let mut g = self.inner.lock().unwrap();
        // With no credentials loaded there is no connection to have an opinion about.
        if !loaded(state) {
            g.last_online = None;
            g.settling = None;
            return;
        }
        let now = online(state);

        // First observation after loading: adopt it silently rather than announcing it.
        let Some(last) = g.last_online else {
            g.last_online = Some(now);
            return;
        };
        if now == last {
            g.settling = None; // flipped back before settling
            return;
        }
        if matches!(g.settling, Some((to, _)) if to == now) {
            return; // already waiting on this same flip
        }

        g.token += 1;
        let token = g.token;
        g.settling = Some((now, token));
        drop(g);

        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(this.deps.settle_ms)).await;
            this.settled(now, token);
        });
    }

    fn settled(&self, now: bool, token: u64) {
        {
            let mut g = self.inner.lock().unwrap();
            if g.settling.map(|(_, t)| t) != Some(token) {
                return; // cancelled or superseded
            }
            g.settling = None;
        }
        let settled = (self.deps.get_state)();
        // Flipped back while we waited, so there is nothing worth telling anyone.
        if !loaded(&settled) || online(&settled) != now {
            return;
        }
        self.inner.lock().unwrap().last_online = Some(now);
        if (self.deps.is_foreground)() {
            return;
        }

        let live = settled["tunnels"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|t| matches!(t["status"].as_str(), Some("open") | Some("starting")))
                    .count()
            })
            .unwrap_or(0);

        if now {
            (self.deps.on_restored)(live);
        } else {
            let reason = settled["creds"]["offlineReason"].as_str().unwrap_or("AWS cannot be reached.").to_string();
            (self.deps.on_lost)(reason, live);
        }
    }
}
