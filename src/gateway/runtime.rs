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

    /// The gateway carries upstream credentials. Non-loopback binds are only
    /// permitted when a bearer token guards the API routes.
    pub fn validate(self, authenticated: bool) -> Result<(), &'static str> {
        if self.host.is_loopback() || authenticated {
            Ok(())
        } else {
            Err("non-loopback listeners require an authenticated gateway")
        }
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

    #[test]
    fn loopback_without_token_is_allowed() {
        let listen = ListenConfig::default();
        assert!(listen.validate(false).is_ok());
    }

    #[test]
    fn non_loopback_without_token_is_refused() {
        let listen = ListenConfig::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 3456);
        assert!(listen.validate(false).is_err());
    }

    #[test]
    fn non_loopback_with_token_is_allowed() {
        let listen = ListenConfig::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 3456);
        assert!(listen.validate(true).is_ok());
    }
}
