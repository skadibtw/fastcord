//! Event-driven network-change hints, without polling or network identifiers.
//! The app debounces callbacks for 500 ms and invalidates stale TLS candidates;
//! a notification never disconnects a healthy session or starts a probe.

use std::sync::Arc;

use fastcord_net::NetworkError;
use fastcord_net::selector::NetworkChangeSource;

/// Dropping the guard unregisters the event handler. No packet interception,
/// system proxy/DNS/firewall changes or administrator privileges are involved.
pub struct NetworkChanges {
    #[cfg(target_os = "windows")]
    cookie: i64,
}

impl NetworkChanges {
    pub fn start(callback: impl Fn() + Send + Sync + 'static) -> Result<Self, NetworkError> {
        <Self as NetworkChangeSource>::subscribe(Arc::new(callback))
    }
}

impl NetworkChangeSource for NetworkChanges {
    fn subscribe(callback: Arc<dyn Fn() + Send + Sync>) -> Result<Self, NetworkError> {
        #[cfg(target_os = "windows")]
        {
            use windows::Networking::Connectivity::{
                NetworkInformation, NetworkStatusChangedEventHandler,
            };

            let handler = NetworkStatusChangedEventHandler::new(move |_| {
                callback();
                Ok(())
            });
            let cookie = NetworkInformation::NetworkStatusChanged(&handler)
                .map_err(|_| NetworkError::Unsupported)?;
            Ok(Self { cookie })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = callback;
            // Reassessment on these backends remains failure/manual triggered;
            // there is no guessed portability layer or idle polling fallback.
            Err(NetworkError::Unsupported)
        }
    }
}

impl Drop for NetworkChanges {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            let _ =
                windows::Networking::Connectivity::NetworkInformation::RemoveNetworkStatusChanged(
                    self.cookie,
                );
        }
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    #[test]
    fn unsupported_backend_does_not_start_a_polling_thread() {
        assert!(matches!(
            NetworkChanges::start(|| panic!("unexpected callback")),
            Err(NetworkError::Unsupported)
        ));
    }
}
