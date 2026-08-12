//! Safe local listener defaults for WSL deployments.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Listener settings for the local Claude Code gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenConfig {
    pub host: IpAddr,
    pub port: u16,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 3456,
        }
    }
}

impl ListenConfig {
    pub const fn new(host: IpAddr, port: u16) -> Self {
        Self { host, port }
    }

    pub const fn socket_addr(self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_loopback_for_wsl_safety() {
        assert_eq!(
            ListenConfig::default().socket_addr().to_string(),
            "127.0.0.1:3456"
        );
    }
}
