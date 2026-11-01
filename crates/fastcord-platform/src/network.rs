//! Event-driven network-change hints, without polling or network identifiers.
//! The app debounces callbacks for 500 ms and invalidates stale TLS candidates;
//! a notification never disconnects a healthy session or starts a probe.

use std::sync::Arc;

use fastcord_net::NetworkError;
use fastcord_net::selector::NetworkChangeSource;

#[cfg(target_os = "windows")]
mod helper;
#[cfg(target_os = "windows")]
mod packet;
#[cfg(any(target_os = "windows", test))]
mod packet_plan;
#[cfg(target_os = "windows")]
pub use helper::{PacketFactory, PacketHelper};

/// Native packet operations shipped by this platform build. This is capability,
/// not runtime helper availability or evidence that a filtered route works.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketCapabilities {
    pub explicit_authorization_required: bool,
    pub tcp_fake: bool,
    pub tcp_overlap: bool,
    pub udp_discovery_fake: bool,
    pub udp_ipv4_fragment: bool,
    pub udp_ipv6_fragment: bool,
}

pub const fn packet_capabilities() -> PacketCapabilities {
    let windows = cfg!(target_os = "windows");
    PacketCapabilities {
        explicit_authorization_required: windows,
        tcp_fake: windows,
        tcp_overlap: windows,
        udp_discovery_fake: windows,
        udp_ipv4_fragment: windows,
        udp_ipv6_fragment: false,
    }
}

/// Special child entry; malformed helper invocations never start the client.
pub fn packet_helper_entry(suffix: &str, parent: u32) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        helper::entry(suffix, parent)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (suffix, parent);
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

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

#[cfg(all(test, not(target_os = "windows")))]
mod packet_capability_tests {
    #[test]
    fn unsupported_operations_refuse_without_starting_a_helper() {
        let caps = super::packet_capabilities();
        assert!(!caps.tcp_fake && !caps.tcp_overlap && !caps.udp_discovery_fake);
        assert!(!caps.udp_ipv4_fragment && !caps.udp_ipv6_fragment);
        assert!(!caps.explicit_authorization_required);
        assert_eq!(
            super::packet_helper_entry("fixture", 1).unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
    }
}
#[cfg(target_os = "linux")]
pub mod profile;
