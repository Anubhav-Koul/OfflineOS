//! Following the default microphone when it changes.
//!
//! cpal opens the default input device once and never notices if the user unplugs a
//! headset, switches to Bluetooth, or Windows re-picks the default. There is no
//! cpal API for device changes, so we hand-write the WASAPI one: an
//! [`IMMNotificationClient`] COM callback that fires on `OnDefaultDeviceChanged`.
//! When the default *capture* device changes, [`DeviceWatcher`] runs a callback the
//! pipeline uses to drop and reopen capture on the new mic
//! ([`crate::pipeline::RestartTrigger`]).
//!
//! The watcher is Windows-only; every other platform gets a no-op with the same
//! shape, so the driver wires it unconditionally. It cannot be meaningfully
//! unit-tested (it needs the audio device graph to change under it), so there is an
//! `#[ignore]`d smoke test that just registers and unregisters.

use std::sync::Arc;

/// A callback run when the default input device changes. Must be cheap and
/// non-blocking — it fires on a WASAPI thread.
pub type DeviceChangeFn = Arc<dyn Fn() + Send + Sync>;

#[cfg(windows)]
pub use imp::DeviceWatcher;
#[cfg(not(windows))]
pub use stub::DeviceWatcher;

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
        IMMNotificationClient_Impl, MMDeviceEnumerator, eCapture,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::core::{PCWSTR, Result as WinResult, implement};

    use super::DeviceChangeFn;
    use crate::error::{Error, Result};

    /// A registered WASAPI notification client. Unregisters on drop.
    pub struct DeviceWatcher {
        enumerator: IMMDeviceEnumerator,
        client: IMMNotificationClient,
    }

    // The registered COM objects are MTA (created with `COINIT_MULTITHREADED`) and
    // this value only ever touches them from `start` and `drop` on one thread; the
    // callback it carries is itself `Send + Sync`. So it is safe to move and share.
    unsafe impl Send for DeviceWatcher {}
    unsafe impl Sync for DeviceWatcher {}

    impl DeviceWatcher {
        /// Register for default-device-change notifications, invoking `on_change`
        /// whenever the default *input* device changes.
        pub fn start(on_change: DeviceChangeFn) -> Result<Self> {
            unsafe {
                // The widget's runtime may already have initialised COM on other
                // threads; MTA here is fine and a "changed mode" result is benign.
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|error| {
                        Error::audio(format!("creating the device enumerator: {error}"))
                    })?;

                let client: IMMNotificationClient = Notifier { on_change }.into();
                enumerator
                    .RegisterEndpointNotificationCallback(&client)
                    .map_err(|error| {
                        Error::audio(format!("registering device notifications: {error}"))
                    })?;

                tracing::debug!("registered for WASAPI default-device changes");
                Ok(Self { enumerator, client })
            }
        }
    }

    impl Drop for DeviceWatcher {
        fn drop(&mut self) {
            unsafe {
                let _ = self
                    .enumerator
                    .UnregisterEndpointNotificationCallback(&self.client);
            }
        }
    }

    /// The COM object WASAPI calls back into. Only `OnDefaultDeviceChanged` for the
    /// capture role matters; the rest are required by the interface and ignored.
    #[implement(IMMNotificationClient)]
    struct Notifier {
        on_change: DeviceChangeFn,
    }

    impl IMMNotificationClient_Impl for Notifier_Impl {
        fn OnDefaultDeviceChanged(
            &self,
            flow: EDataFlow,
            _role: ERole,
            _default_device_id: &PCWSTR,
        ) -> WinResult<()> {
            // `eCapture` == the input side. We reopen on any role change for capture
            // (console/communications) — they can differ, and either affects us.
            if flow == eCapture {
                (self.on_change)();
            }
            Ok(())
        }

        fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> WinResult<()> {
            Ok(())
        }
        fn OnDeviceAdded(&self, _id: &PCWSTR) -> WinResult<()> {
            Ok(())
        }
        fn OnDeviceRemoved(&self, _id: &PCWSTR) -> WinResult<()> {
            Ok(())
        }
        fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> WinResult<()> {
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod stub {
    use super::DeviceChangeFn;
    use crate::error::Result;

    /// A no-op watcher for non-Windows targets, so the driver wires it uniformly.
    pub struct DeviceWatcher;

    impl DeviceWatcher {
        /// Does nothing off Windows — there is no WASAPI to watch.
        pub fn start(_on_change: DeviceChangeFn) -> Result<Self> {
            Ok(Self)
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Registering and dropping the watcher must not panic. Ignored: it touches COM
    /// and the audio device graph. Run with `--ignored`.
    #[test]
    #[ignore = "touches WASAPI COM; run with --ignored"]
    fn registers_and_unregisters_cleanly() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_cb = Arc::clone(&fired);
        let watcher = DeviceWatcher::start(Arc::new(move || {
            fired_cb.store(true, Ordering::SeqCst);
        }))
        .expect("register");
        drop(watcher);
        // No assertion on `fired` — it only fires on a real device change.
        let _ = fired;
    }
}
