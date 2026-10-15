// Sleep, unlock and network changes are exactly when the app's picture of the world
// goes stale without anything in it changing. This module turns them into
// health::wake() calls, from three sources, most precise first:
//
//   1. Native OS notifications — macOS SystemConfiguration/IOKit/distributed
//      notifications, Windows IP Helper/power/session callbacks. Immediate, and they
//      see path changes the interface list cannot: a VPN coming up or dropping while
//      Wi-Fi stays connected.
//   2. Polling as the fallback: the interface list every 3s, and a timer tick that
//      arrives far later than scheduled meaning the machine slept.
//   3. An AWS reachability probe: a TCP connect to the region's SSM endpoint. It can
//      only ever report *Offline* faster — "Online" still requires a successful AWS
//      call (see creds::is_online), so a probe success just asks for that check.

use crate::{creds, health};
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use tauri::async_runtime;
use tokio::sync::Notify;

const POLL: Duration = Duration::from_secs(3);
const SLEPT: Duration = Duration::from_secs(20);
const PROBE_EVERY: Duration = Duration::from_secs(20);
// A single failed connect is a blip; the second, a few seconds later, is evidence.
const PROBE_RETRY: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_FAILS_FOR_OFFLINE: u32 = 2;
// OS notifications arrive in bursts (one per interface, per address family), and each
// wake forces a credential check, so a burst is collapsed into one.
const WAKE_DEBOUNCE_MS: i64 = 3000;

const NO_NETWORK: &str = "This machine has no network connection.";

static PROBE_NOW: OnceLock<Notify> = OnceLock::new();
static PROBE_FAILS: AtomicU32 = AtomicU32::new(0);
static LAST_WAKE: AtomicI64 = AtomicI64::new(0);

type Broadcast = fn();
static BROADCAST: OnceLock<Broadcast> = OnceLock::new();

fn broadcast() {
    if let Some(f) = BROADCAST.get() {
        f();
    }
}

fn probe_now() -> &'static Notify {
    PROBE_NOW.get_or_init(Notify::new)
}

#[derive(Clone, Copy, Debug)]
pub enum Event {
    Network,
    Resume,
    Unlock,
}

// The one entry point for every source, native or polled.
pub fn signal(event: Event) {
    #[cfg(debug_assertions)]
    eprintln!("[sysevents] {event:?}");
    match event {
        Event::Network => {
            // Losing the network is the fastest possible signal that nothing is working,
            // so it is reported at once instead of waiting for a check to time out.
            if !has_network() {
                creds::mark_unreachable(NO_NETWORK);
                broadcast();
                return;
            }
            // Some path changed while an interface is still up — a VPN, a different
            // Wi-Fi. Probe now rather than on the next interval, and re-verify. Do not
            // claim Online on the strength of the interface alone: a check has to succeed.
            probe_now().notify_one();
            wake("network");
        }
        Event::Resume => wake("resume"),
        Event::Unlock => wake("unlock"),
    }
}

fn wake(reason: &'static str) {
    let now = crate::error::now_ms();
    let last = LAST_WAKE.load(Ordering::SeqCst);
    if now - last < WAKE_DEBOUNCE_MS {
        return;
    }
    LAST_WAKE.store(now, Ordering::SeqCst);
    health::wake(reason);
}

// Interface-level connectivity, as Electron's net.isOnline() reported it: any
// non-loopback interface holding a routable address.
pub fn has_network() -> bool {
    if_addrs::get_if_addrs()
        .map(|ifs| {
            ifs.iter().any(|i| {
                !i.is_loopback()
                    && match i.ip() {
                        std::net::IpAddr::V4(a) => !a.is_link_local(),
                        std::net::IpAddr::V6(a) => (a.segments()[0] & 0xffc0) != 0xfe80,
                    }
            })
        })
        .unwrap_or(true)
}

pub fn start(app: &tauri::AppHandle, on_change: Broadcast) {
    let _ = BROADCAST.set(on_change);

    if !has_network() {
        creds::mark_unreachable(NO_NETWORK);
    }
    native::install(app);
    poll();
    probe_loop();
}

// The fallback: catches whatever the native sources miss or cannot provide.
fn poll() {
    async_runtime::spawn(async {
        let mut last = SystemTime::now();
        let mut had_network = has_network();
        loop {
            tokio::time::sleep(POLL).await;
            // Timers do not run while the machine sleeps, so a tick that arrives far
            // later than scheduled means the machine was suspended.
            let now = SystemTime::now();
            let gap = now.duration_since(last).unwrap_or_default();
            last = now;
            if gap > POLL + SLEPT {
                signal(Event::Resume);
            }

            let net = has_network();
            if net != had_network {
                had_network = net;
                signal(Event::Network);
            }
        }
    });
}

// A proxy means the CLI does not connect to AWS directly, so a direct connect proves
// nothing either way.
fn behind_proxy() -> bool {
    ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .any(|k| std::env::var(k).map(|v| !v.trim().is_empty()).unwrap_or(false))
}

async fn reachable(region: &str) -> bool {
    let host = format!("ssm.{region}.amazonaws.com:443");
    matches!(tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(host)).await, Ok(Ok(_)))
}

fn probe_loop() {
    async_runtime::spawn(async {
        loop {
            let wait = if PROBE_FAILS.load(Ordering::SeqCst) == 1 { PROBE_RETRY } else { PROBE_EVERY };
            let _ = tokio::time::timeout(wait, probe_now().notified()).await;

            if !creds::has() || behind_proxy() || !has_network() {
                PROBE_FAILS.store(0, Ordering::SeqCst);
                continue;
            }
            let region = creds::region();
            if reachable(&region).await {
                // Recovery is never declared from here. If this probe had put the app
                // Offline, ask for the real check, which is what clears it.
                if PROBE_FAILS.swap(0, Ordering::SeqCst) >= PROBE_FAILS_FOR_OFFLINE {
                    wake("network");
                }
                continue;
            }
            let fails = PROBE_FAILS.fetch_add(1, Ordering::SeqCst) + 1;
            #[cfg(debug_assertions)]
            eprintln!("[sysevents] probe of ssm.{region} failed ({fails})");
            if fails == PROBE_FAILS_FOR_OFFLINE {
                creds::mark_unreachable(&format!(
                    "AWS cannot be reached from this network — ssm.{region}.amazonaws.com is not answering. \
                     Check your network or VPN connection."
                ));
                broadcast();
            }
        }
    });
}

#[cfg(target_os = "macos")]
mod native {
    //! SystemConfiguration for network paths, IOKit for wake, and the distributed
    //! notification centre for unlock. All three deliver on the main run loop, so they
    //! are installed from the main thread.

    use super::{signal, Event};
    use std::ffi::{c_char, c_void, CString};
    use std::ptr::null;

    type CFTypeRef = *const c_void;
    type CFStringRef = CFTypeRef;
    type CFArrayRef = CFTypeRef;
    type CFRunLoopRef = CFTypeRef;
    type CFRunLoopSourceRef = CFTypeRef;
    type CFIndex = isize;
    type SCDynamicStoreRef = CFTypeRef;
    type IONotificationPortRef = *mut c_void;
    type IoObject = u32;

    const UTF8: u32 = 0x0800_0100;
    const DELIVER_IMMEDIATELY: CFIndex = 4;
    const MSG_CAN_SYSTEM_SLEEP: u32 = 0xe000_0270;
    const MSG_SYSTEM_WILL_SLEEP: u32 = 0xe000_0280;
    const MSG_SYSTEM_HAS_POWERED_ON: u32 = 0xe000_0300;

    #[repr(C)]
    struct CFArrayCallBacks {
        _private: [u8; 0],
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFRunLoopCommonModes: CFStringRef;
        static kCFTypeArrayCallBacks: CFArrayCallBacks;
        fn CFRunLoopGetMain() -> CFRunLoopRef;
        fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
        fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFStringRef;
        fn CFArrayCreate(alloc: CFTypeRef, values: *const CFTypeRef, n: CFIndex, cb: *const CFArrayCallBacks) -> CFArrayRef;
        fn CFNotificationCenterGetDistributedCenter() -> CFTypeRef;
        fn CFNotificationCenterAddObserver(
            center: CFTypeRef,
            observer: *const c_void,
            callback: extern "C" fn(CFTypeRef, *const c_void, CFStringRef, *const c_void, CFTypeRef),
            name: CFStringRef,
            object: *const c_void,
            behavior: CFIndex,
        );
    }

    #[link(name = "SystemConfiguration", kind = "framework")]
    extern "C" {
        fn SCDynamicStoreCreate(
            alloc: CFTypeRef,
            name: CFStringRef,
            callout: extern "C" fn(SCDynamicStoreRef, CFArrayRef, *mut c_void),
            context: *mut c_void,
        ) -> SCDynamicStoreRef;
        fn SCDynamicStoreSetNotificationKeys(store: SCDynamicStoreRef, keys: CFArrayRef, patterns: CFArrayRef) -> u8;
        fn SCDynamicStoreCreateRunLoopSource(alloc: CFTypeRef, store: SCDynamicStoreRef, order: CFIndex) -> CFRunLoopSourceRef;
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IORegisterForSystemPower(
            refcon: *mut c_void,
            port: *mut IONotificationPortRef,
            callback: extern "C" fn(*mut c_void, IoObject, u32, *mut c_void),
            notifier: *mut IoObject,
        ) -> u32;
        fn IONotificationPortGetRunLoopSource(port: IONotificationPortRef) -> CFRunLoopSourceRef;
        fn IOAllowPowerChange(kernel_port: u32, notification_id: isize) -> i32;
    }

    // Never released: these live for the life of the app.
    fn cfstr(s: &str) -> CFStringRef {
        let c = CString::new(s).unwrap();
        unsafe { CFStringCreateWithCString(null(), c.as_ptr(), UTF8) }
    }

    fn cfarray(items: &[&str]) -> CFArrayRef {
        let refs: Vec<CFTypeRef> = items.iter().map(|s| cfstr(s)).collect();
        unsafe { CFArrayCreate(null(), refs.as_ptr(), refs.len() as CFIndex, &kCFTypeArrayCallBacks) }
    }

    extern "C" fn on_network(_store: SCDynamicStoreRef, _keys: CFArrayRef, _info: *mut c_void) {
        signal(Event::Network);
    }

    extern "C" fn on_unlock(_c: CFTypeRef, _o: *const c_void, _n: CFStringRef, _obj: *const c_void, _u: CFTypeRef) {
        signal(Event::Unlock);
    }

    static mut ROOT_PORT: u32 = 0;

    extern "C" fn on_power(_refcon: *mut c_void, _service: IoObject, message: u32, arg: *mut c_void) {
        match message {
            // Sleep must be acknowledged or the system waits on us before sleeping.
            MSG_CAN_SYSTEM_SLEEP | MSG_SYSTEM_WILL_SLEEP => unsafe {
                IOAllowPowerChange(ROOT_PORT, arg as isize);
            },
            MSG_SYSTEM_HAS_POWERED_ON => signal(Event::Resume),
            _ => {}
        }
    }

    unsafe fn install_main_thread() {
        let main = CFRunLoopGetMain();

        // Any change to the primary service or to any interface's addresses — which is
        // what a VPN (utun) coming or going looks like.
        let store = SCDynamicStoreCreate(null(), cfstr("Tunnel Keeper"), on_network, std::ptr::null_mut());
        if !store.is_null() {
            let keys = cfarray(&["State:/Network/Global/IPv4", "State:/Network/Global/IPv6", "State:/Network/Global/DNS"]);
            let patterns = cfarray(&["State:/Network/Interface/.*/IPv4", "State:/Network/Interface/.*/IPv6"]);
            if SCDynamicStoreSetNotificationKeys(store, keys, patterns) != 0 {
                let src = SCDynamicStoreCreateRunLoopSource(null(), store, 0);
                if !src.is_null() {
                    CFRunLoopAddSource(main, src, kCFRunLoopCommonModes);
                    #[cfg(debug_assertions)]
                    eprintln!("[sysevents] network watcher installed");
                }
            }
        }

        let mut port: IONotificationPortRef = std::ptr::null_mut();
        let mut notifier: IoObject = 0;
        let root = IORegisterForSystemPower(std::ptr::null_mut(), &mut port, on_power, &mut notifier);
        if root != 0 && !port.is_null() {
            ROOT_PORT = root;
            CFRunLoopAddSource(main, IONotificationPortGetRunLoopSource(port), kCFRunLoopCommonModes);
            #[cfg(debug_assertions)]
            eprintln!("[sysevents] power watcher installed");
        }

        CFNotificationCenterAddObserver(
            CFNotificationCenterGetDistributedCenter(),
            null(),
            on_unlock,
            cfstr("com.apple.screenIsUnlocked"),
            null(),
            DELIVER_IMMEDIATELY,
        );
    }

    pub fn install(app: &tauri::AppHandle) {
        let _ = app.run_on_main_thread(|| unsafe { install_main_thread() });
    }
}

#[cfg(windows)]
mod native {
    //! IP Helper change notifications for the network, a suspend/resume callback for
    //! wake, and session notifications (via a subclass of our own window) for unlock.

    use super::{signal, Event};
    use std::ffi::c_void;
    use tauri::Manager;
    use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        NotifyIpInterfaceChange, NotifyUnicastIpAddressChange, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
        MIB_UNICASTIPADDRESS_ROW,
    };
    use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;
    use windows_sys::Win32::System::Power::{
        PowerRegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
    };
    use windows_sys::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, WM_WTSSESSION_CHANGE};

    const PBT_APMRESUMESUSPEND: u32 = 0x7;
    const PBT_APMRESUMEAUTOMATIC: u32 = 0x12;
    const WTS_SESSION_UNLOCK: usize = 0x8;

    unsafe extern "system" fn on_interface(_ctx: *const c_void, _row: *const MIB_IPINTERFACE_ROW, _t: MIB_NOTIFICATION_TYPE) {
        signal(Event::Network);
    }

    unsafe extern "system" fn on_address(_ctx: *const c_void, _row: *const MIB_UNICASTIPADDRESS_ROW, _t: MIB_NOTIFICATION_TYPE) {
        signal(Event::Network);
    }

    unsafe extern "system" fn on_power(_ctx: *const c_void, kind: u32, _setting: *const c_void) -> u32 {
        if kind == PBT_APMRESUMEAUTOMATIC || kind == PBT_APMRESUMESUSPEND {
            signal(Event::Resume);
        }
        0
    }

    unsafe extern "system" fn on_window(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if msg == WM_WTSSESSION_CHANGE && wparam == WTS_SESSION_UNLOCK {
            signal(Event::Unlock);
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    pub fn install(app: &tauri::AppHandle) {
        unsafe {
            // Handles are deliberately never cancelled: these live for the life of the app.
            let mut h: HANDLE = std::ptr::null_mut();
            NotifyIpInterfaceChange(AF_UNSPEC, Some(on_interface), std::ptr::null(), 0, &mut h);
            let mut h2: HANDLE = std::ptr::null_mut();
            NotifyUnicastIpAddressChange(AF_UNSPEC, Some(on_address), std::ptr::null(), 0, &mut h2);

            let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(on_power),
                Context: std::ptr::null_mut(),
            }));
            let mut reg: *mut c_void = std::ptr::null_mut();
            PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                params as *mut _ as HANDLE,
                &mut reg,
            );
        }

        // Session messages need a window; ours is never destroyed, only hidden.
        if let Some(w) = app.get_webview_window("main") {
            if let Ok(hwnd) = w.hwnd() {
                // Crosses to the main thread as an integer; a raw pointer is not Send.
                let raw = hwnd.0 as usize;
                let _ = app.run_on_main_thread(move || unsafe {
                    let hwnd = raw as HWND;
                    SetWindowSubclass(hwnd, Some(on_window), 1, 0);
                    WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
                });
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod native {
    // Polling only.
    pub fn install(_app: &tauri::AppHandle) {}
}
