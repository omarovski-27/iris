//! Detects a second launch of an already-running Iris and turns it into a
//! request to show the first instance's window, rather than starting a
//! second resident process.
//!
//! A tray-resident app has nothing on screen by default, so a second
//! double-click looks identical to a broken one unless something answers it.
//! [`acquire`] is that answer: a named Win32 mutex marks one process as
//! primary, and a named auto-reset event lets a later launch — still early
//! enough that it has not yet opened the microphone, installed the hotkey
//! hook or created a second tray icon — wake the primary's window instead of
//! running alongside it.
//!
//! Both names are qualified with `USERNAME` rather than left in the default,
//! session-local kernel-object namespace: Remote Desktop / Terminal Services
//! can put more than one interactive user in the same session, and this is
//! the difference between "one Iris per user" and "one Iris per session".
//!
//! Only the Win32 half needs a real OS; the non-Windows half exists so this
//! module type-checks in the portable `cargo check --workspace` loop like
//! every other module in this crate, even though `main`'s non-Windows `run`
//! never actually calls into it (there is no resident loop there to protect).

/// What launching found: either this is the only Iris and should proceed, or
/// another one is already running and has just been asked to show itself.
pub enum Instance {
    /// The only instance. `reopen` fires once for every later launch that
    /// found this one already running.
    Primary {
        /// Holds the lock for the life of the process; never read.
        guard: Guard,
        /// Forwards each later launch's signal.
        reopen: crossbeam_channel::Receiver<()>,
    },
    /// Another Iris holds the lock and has just been signalled. This process
    /// should exit without touching the microphone, the hotkey hook or the
    /// tray.
    AlreadyRunning,
}

#[cfg(windows)]
pub use win::{acquire, Guard};

#[cfg(not(windows))]
pub use portable::{acquire, Guard};

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use anyhow::{Context, Result};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
        WIN32_ERROR,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE,
    };

    use super::Instance;

    fn object_name(suffix: &str) -> HSTRING {
        let user = std::env::var("USERNAME").unwrap_or_default();
        HSTRING::from(format!("Iris-SingleInstance-{suffix}-{user}"))
    }

    /// The named mutex that marks this process as the primary instance.
    /// Dropping it releases the lock, so a later launch starts a new primary
    /// rather than waiting behind a dead one.
    pub struct Guard(HANDLE);

    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: `self.0` was returned by `CreateMutexW` in `acquire`
            // and is closed exactly once, here.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Find out whether this is the only Iris, claiming the lock if so.
    ///
    /// # Errors
    ///
    /// If the mutex or the event cannot be created at all. The caller treats
    /// that as "assume primary and start normally" — a single-instance check
    /// that cannot run must never be the reason dictation stops working.
    pub fn acquire() -> Result<Instance> {
        let mutex_name = object_name("Mutex");
        // SAFETY: no security attributes, not initially owned, a valid
        // NUL-terminated name that outlives the call. `SetLastError` first so
        // the read below is never a stale error from something else this
        // thread did earlier.
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let mutex = unsafe { CreateMutexW(None, false, &mutex_name) }
            .context("creating the single-instance mutex")?;
        // SAFETY: querying the error state the call above just set.
        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

        let event_name = object_name("Reopen");
        // SAFETY: same as the mutex above. Auto-reset (`bmanualreset =
        // false`) so each `SetEvent` wakes exactly one
        // `WaitForSingleObject`. If the event already exists (the primary
        // created it first), this opens that same object instead of creating
        // a new one — the documented behaviour of a named `CreateEventW`
        // call, and exactly what a second launch needs.
        let event = unsafe { CreateEventW(None, false, false, &event_name) }
            .context("creating the single-instance reopen event")?;

        if already_running {
            // SAFETY: `event` is the just-opened handle above.
            unsafe { SetEvent(event) }.context("signalling the running instance")?;
            // SAFETY: both handles were just opened by this function and are
            // not used again on this path.
            unsafe {
                let _ = CloseHandle(event);
                let _ = CloseHandle(mutex);
            }
            return Ok(Instance::AlreadyRunning);
        }

        // `HANDLE` wraps a raw pointer, so it is not `Send` on its own — carry
        // it across the thread boundary as the plain integer it really is and
        // rebuild it on the other side.
        let event_addr = event.0 as isize;
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("iris-single-instance".into())
            .spawn(move || {
                let event = HANDLE(event_addr as *mut core::ffi::c_void);
                loop {
                    // SAFETY: `event` is a valid handle for the life of this
                    // thread, which is the life of the process — nothing else
                    // ever closes it.
                    let waited = unsafe { WaitForSingleObject(event, INFINITE) };
                    if waited != WAIT_OBJECT_0 {
                        // WAIT_FAILED: nothing more to watch for.
                        return;
                    }
                    if tx.send(()).is_err() {
                        // The loop that would act on this is gone.
                        return;
                    }
                }
            })
            .context("spawning the single-instance watcher thread")?;

        Ok(Instance::Primary {
            guard: Guard(mutex),
            reopen: rx,
        })
    }
}

#[cfg(not(windows))]
mod portable {
    use super::Instance;

    /// Nothing to hold off Windows.
    pub struct Guard;

    /// Always the only instance off Windows, since there is no resident loop
    /// here for a second launch to collide with.
    pub fn acquire() -> anyhow::Result<Instance> {
        Ok(Instance::Primary {
            guard: Guard,
            reopen: crossbeam_channel::never(),
        })
    }
}
