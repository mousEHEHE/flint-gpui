#[derive(Debug, Clone)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
}

impl Connection {
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.host
        } else {
            &self.name
        }
    }
}

/// A fully-resolved proxy the SSH transport connects through, with no
/// dependency on the vault layer. `addr` is `host:port` of the proxy itself.
#[derive(Debug, Clone)]
pub enum ResolvedProxy {
    Socks5 {
        addr: String,
        auth: Option<(String, String)>,
    },
    Http {
        addr: String,
        auth: Option<(String, String)>,
    },
}

/// Which way a port forward tunnels. Shared by the vault (which stores it) and
/// the SSH layer (which runs it), so the two can never drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ForwardKind {
    /// `-L`: listen here, connect from the server.
    Local,
    /// `-R`: the server listens, we connect from here.
    Remote,
    /// `-D`: listen here as a SOCKS5 proxy; every client picks its own target.
    Dynamic,
}

impl ForwardKind {
    pub fn display_label(self) -> &'static str {
        match self {
            ForwardKind::Local => "Local",
            ForwardKind::Remote => "Remote",
            ForwardKind::Dynamic => "SOCKS5",
        }
    }
}

/// A fully-resolved port forward the SSH layer can run, with no dependency on
/// the vault layer, like [`ResolvedProxy`].
#[derive(Debug, Clone)]
pub struct ForwardSpec {
    /// Vault row id as a String, matching `Connection::id`, so events coming
    /// back from the SSH thread find the row they belong to.
    pub id: String,
    pub kind: ForwardKind,
    pub bind_address: String,
    /// 0 for `Remote` means "server picks a port" and is reported back once
    /// bound. Local and Dynamic always carry a real port.
    pub bind_port: u16,
    /// Ignored for `Dynamic`: each SOCKS5 client names its own target.
    pub target_host: String,
    /// Ignored for `Dynamic`, see `target_host`.
    pub target_port: u16,
}

impl ForwardSpec {
    /// One-line description of where the bytes go, for the popover and logs.
    pub fn route_label(&self) -> String {
        match self.kind {
            ForwardKind::Local => format!(
                "{}:{} → {}:{}",
                self.bind_address, self.bind_port, self.target_host, self.target_port
            ),
            ForwardKind::Remote => format!(
                "{}:{} ← {}:{}",
                self.bind_address, self.bind_port, self.target_host, self.target_port
            ),
            ForwardKind::Dynamic => {
                format!("SOCKS5 on {}:{}", self.bind_address, self.bind_port)
            }
        }
    }
}

/// True when only the machine doing the listening can reach the forward.
/// Anything else exposes the tunnel to the network, which both the SSH layer
/// (a warning in the log) and the server form (a warning in the UI) call out.
pub fn is_loopback_bind(address: &str) -> bool {
    if address.eq_ignore_ascii_case("localhost") {
        return true;
    }
    address
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: ForwardKind) -> ForwardSpec {
        ForwardSpec {
            id: "id".into(),
            kind,
            bind_address: "127.0.0.1".into(),
            bind_port: 8080,
            target_host: "db.internal".into(),
            target_port: 5432,
        }
    }

    #[test]
    fn route_label_names_both_ends_for_local_and_remote() {
        assert_eq!(
            spec(ForwardKind::Local).route_label(),
            "127.0.0.1:8080 → db.internal:5432"
        );
        assert_eq!(
            spec(ForwardKind::Remote).route_label(),
            "127.0.0.1:8080 ← db.internal:5432"
        );
    }

    #[test]
    fn route_label_hides_the_unused_target_for_dynamic() {
        let dynamic = ForwardSpec {
            target_host: String::new(),
            target_port: 0,
            ..spec(ForwardKind::Dynamic)
        };
        assert_eq!(dynamic.route_label(), "SOCKS5 on 127.0.0.1:8080");
    }

    #[test]
    fn only_loopback_addresses_are_private() {
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("localhost"));
        assert!(is_loopback_bind("::1"));
        assert!(is_loopback_bind("LOCALHOST"));
        assert!(is_loopback_bind("127.0.0.2"));
        assert!(!is_loopback_bind("::"));
        assert!(!is_loopback_bind("localhost.example.com"));
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind(""));
        assert!(!is_loopback_bind("198.51.100.10"));
    }
}
