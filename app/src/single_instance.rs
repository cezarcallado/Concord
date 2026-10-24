//! One concord at a time (v1.1): opening concord while it is already running brings the running
//! one to the front (out of the tray, un-minimized) and the new process quits at once.
//!
//! - Windows: a named mutex (`Local\concord-<key>-instance`, per Windows session) says one is
//!   running; a named auto-reset event (`Local\concord-<key>-show`) is how a second launch asks it
//!   to show itself. The second launch lets the running one take the foreground
//!   (`AllowSetForegroundWindow`: it was started by the user, the running one was not).
//! - macOS: an exclusive lock on `~/Library/Application Support/concord/<key>.lock` (released by
//!   the system when the process ends, even on a crash) and a Unix socket beside it for the
//!   "show" signal. (`LSMultipleInstancesProhibited` in Info.plist covers the Finder and Dock;
//!   this covers a direct start of the binary too.)
//!
//! Never in tests: `--autotest`, background test mode (`VOX_TEST_BACKGROUND`), `--settings-dir`
//! and design-review screenshots run many concords at once, so they skip it ([`key`]). The test
//! of this module itself (`VOX_SINGLE_INSTANCE_TEST=<name>`) uses a key of its own, `test-<name>`,
//! which never meets the real one (`app`).

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by a second launch's signal; taken by the app (`notify_tick`), which shows the window.
static SHOW: AtomicBool = AtomicBool::new(false);

/// The environment variable of this module's own test: its value names an isolated key.
pub const TEST_ENV: &str = "VOX_SINGLE_INSTANCE_TEST";

/// The key of the real app's lock and signal.
const APP_KEY: &str = "app";

/// The lock's key: `test-<name>` with [`TEST_ENV`] set (even in a test mode: that is its test),
/// none in any other test mode or with `VOX_NO_SINGLE_INSTANCE=1`, else the app's.
pub fn key(test_mode: bool, test_name: Option<&str>, opted_out: bool) -> Option<String> {
    if let Some(name) = test_name.map(sanitize).filter(|n| !n.is_empty()) {
        return Some(format!("test-{name}"));
    }
    (!test_mode && !opted_out).then(|| APP_KEY.to_string())
}

/// Letters, digits, `-` and `_` only (it goes into an object name and a file name), 40 at most.
fn sanitize(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(40).collect()
}

/// [`key`] from this process's arguments and environment.
pub fn key_from_env(test_mode: bool) -> Option<String> {
    let test_name = std::env::var(TEST_ENV).ok();
    let opted_out = std::env::var("VOX_NO_SINGLE_INSTANCE").is_ok_and(|v| !v.is_empty() && v != "0");
    key(test_mode, test_name.as_deref(), opted_out)
}

/// What starting found.
pub enum Start {
    /// We are the one: keep the guard for the life of the process, and [`Guard::listen`].
    First(Guard),
    /// Another concord with the same key is running and was asked to show itself (`signalled`:
    /// the signal got through).
    Running { signalled: bool },
    /// The check could not be made (no lock possible): run anyway.
    Unchecked(String),
}

/// Holds the lock while the app runs.
pub struct Guard {
    #[cfg(windows)]
    event: isize,
    #[cfg(windows)]
    mutex: isize,
    #[cfg(target_os = "macos")]
    _lock: std::fs::File,
    #[cfg(target_os = "macos")]
    listener: Option<std::os::unix::net::UnixListener>,
}

/// Take the lock for `key`, or signal the one holding it.
pub fn acquire(key: &str) -> Start {
    imp::acquire(key)
}

impl Guard {
    /// Wait (on a small thread of its own) for the next launches' signals: each sets the show flag
    /// ([`take_show`]) and calls `wake` (the window's repaint, so a hidden window hears it too).
    pub fn listen(&mut self, wake: impl Fn() + Send + 'static) {
        imp::listen(self, Box::new(move || {
            tracing::info!("single instance: another launch asked this one to show its window");
            SHOW.store(true, Ordering::Relaxed);
            wake();
        }));
    }
}

/// A second launch asked for the window since the last call.
pub fn take_show() -> bool {
    SHOW.swap(false, Ordering::Relaxed)
}

/// The running concord's guard, from [`hold`] until [`release`] (or the end of the process).
static HELD: std::sync::Mutex<Option<Guard>> = std::sync::Mutex::new(None);

/// Keep the guard (after [`Guard::listen`]) until the app closes.
pub fn hold(g: Guard) {
    if let Ok(mut h) = HELD.lock() {
        *h = Some(g);
    }
}

/// The app is closing: let the lock go now, not when the process ends. Closing takes seconds
/// (leaving the call, stopping a hosted server), and concord opened again meanwhile must start,
/// not ask a window that is going away to show itself.
pub fn release() {
    let g = HELD.lock().ok().and_then(|mut h| h.take());
    if g.is_some() {
        drop(g);
        tracing::info!("single instance: released (closing)");
    }
}

#[cfg(windows)]
mod imp {
    use super::{Guard, Start};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn names(key: &str) -> (Vec<u16>, Vec<u16>) {
        (wide(&format!("Local\\concord-{key}-instance")), wide(&format!("Local\\concord-{key}-show")))
    }

    pub fn acquire(key: &str) -> Start {
        let (mutex_name, event_name) = names(key);
        // SAFETY: plain object creation with NUL-terminated names; the handles are kept for the
        // life of the process (the guard), or closed right here.
        unsafe {
            let mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
            if mutex.is_null() {
                return Start::Unchecked(format!("CreateMutexW failed ({})", GetLastError()));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(mutex);
                return Start::Running { signalled: signal(&event_name) };
            }
            // Auto-reset: one wake per launch.
            let event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
            if event.is_null() {
                return Start::Unchecked(format!("CreateEventW failed ({})", GetLastError()));
            }
            Start::First(Guard { event: event as isize, mutex: mutex as isize })
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            // Only the mutex: the listener may be waiting on the event, and closing a handle that
            // is being waited on is undefined (the event goes with the process).
            // SAFETY: our own handle, closed once.
            unsafe { CloseHandle(self.mutex as _) };
        }
    }

    /// Ask the running one to show itself (it may still be starting: the event comes right after
    /// the mutex, so a few tries).
    fn signal(event_name: &[u16]) -> bool {
        for _ in 0..30 {
            // SAFETY: plain calls; the handle is closed after use.
            unsafe {
                let h = OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr());
                if !h.is_null() {
                    // The running app may take the foreground: this launch was the user's doing.
                    AllowSetForegroundWindow(ASFW_ANY);
                    let ok = SetEvent(h) != 0;
                    CloseHandle(h);
                    return ok;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }

    pub fn listen(g: &mut Guard, on_show: Box<dyn Fn() + Send>) {
        let event = g.event;
        let spawned = std::thread::Builder::new().name("single-instance".into()).spawn(move || loop {
            // SAFETY: the event handle lives as long as the process (dropping the guard closes
            // only the mutex).
            let r = unsafe { WaitForSingleObject(event as _, INFINITE) };
            if r != WAIT_OBJECT_0 {
                tracing::warn!(result = r, "single instance: wait failed, no longer listening");
                return;
            }
            on_show();
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "single instance: could not start the listener");
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    use super::{Guard, Start};

    pub(super) fn dir() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join("Library").join("Application Support").join("concord"))
    }

    pub fn acquire(key: &str) -> Start {
        let Some(dir) = dir() else { return Start::Unchecked("no HOME".into()) };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Start::Unchecked(format!("{}: {e}", dir.display()));
        }
        let (lock_path, sock_path) = (dir.join(format!("{key}.lock")), dir.join(format!("{key}.sock")));
        let lock = match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path) {
            Ok(f) => f,
            Err(e) => return Start::Unchecked(format!("{}: {e}", lock_path.display())),
        };
        match lock.try_lock() {
            Ok(()) => {
                // A socket left by a crash is stale (we hold the lock now).
                let _ = std::fs::remove_file(&sock_path);
                let listener = match UnixListener::bind(&sock_path) {
                    Ok(l) => Some(l),
                    Err(e) => {
                        tracing::warn!(error = %e, "single instance: no socket, a second launch cannot bring this one to the front");
                        None
                    }
                };
                Start::First(Guard { _lock: lock, listener })
            }
            Err(std::fs::TryLockError::WouldBlock) => Start::Running { signalled: signal(&sock_path) },
            Err(std::fs::TryLockError::Error(e)) => Start::Unchecked(format!("lock: {e}")),
        }
    }

    fn signal(sock: &std::path::Path) -> bool {
        for _ in 0..30 {
            if let Ok(mut s) = UnixStream::connect(sock) {
                return s.write_all(b"show\n").is_ok();
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }

    pub fn listen(g: &mut Guard, on_show: Box<dyn Fn() + Send>) {
        let Some(listener) = g.listener.take() else { return };
        let spawned = std::thread::Builder::new().name("single-instance".into()).spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut buf = [0u8; 8];
                if conn.read(&mut buf).is_ok_and(|n| buf[..n].starts_with(b"show")) {
                    on_show();
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "single instance: could not start the listener");
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::{Guard, Start};

    pub fn acquire(_key: &str) -> Start {
        Start::Unchecked("not on this system".into())
    }

    pub fn listen(_g: &mut Guard, _on_show: Box<dyn Fn() + Send>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_never_share_the_real_key() {
        // The real app.
        assert_eq!(key(false, None, false).as_deref(), Some("app"));
        // Any test mode: no single instance at all.
        assert_eq!(key(true, None, false), None);
        // Opted out.
        assert_eq!(key(false, None, true), None);
        // This module's own test: an isolated key, even in a test mode.
        assert_eq!(key(true, Some("gate 1/x"), false).as_deref(), Some("test-gate1x"));
        assert_ne!(key(true, Some("app"), false).as_deref(), Some("app"));
        // An empty test name is no test name.
        assert_eq!(key(true, Some("  "), false), None);
        assert_eq!(sanitize(&"a".repeat(99)).len(), 40);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn a_second_start_signals_the_first() {
        let k = format!("test-unit-{}", std::process::id());
        // macOS: the lock and the socket are files beside the settings; gone when the test ends.
        #[cfg(target_os = "macos")]
        struct Cleanup(Option<std::path::PathBuf>, String);
        #[cfg(target_os = "macos")]
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Some(d) = &self.0 {
                    let _ = std::fs::remove_file(d.join(format!("{}.lock", self.1)));
                    let _ = std::fs::remove_file(d.join(format!("{}.sock", self.1)));
                }
            }
        }
        #[cfg(target_os = "macos")]
        let _cleanup = Cleanup(imp::dir(), k.clone());
        let Start::First(mut g) = acquire(&k) else { panic!("the first start must get the lock") };
        let (tx, rx) = std::sync::mpsc::channel();
        g.listen(move || {
            let _ = tx.send(());
        });
        match acquire(&k) {
            Start::Running { signalled } => assert!(signalled, "the second start could not signal"),
            _ => panic!("the second start must find the first"),
        }
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the first never heard the signal");
        assert!(take_show());
        assert!(!take_show());
    }
}
