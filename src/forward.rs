//! Port forwarding: the accept loops, the SOCKS5 server and the byte pumps.
//!
//! Nothing here touches the russh `client::Handle`. It is not `Clone`, and
//! `tcpip_forward` needs `&mut self`, so every use of it stays in
//! `ssh::run_connection`'s select loop; the tasks in this file ask for channels
//! over a [`ForwardMsg`] queue instead.

use std::net::SocketAddr;
use std::time::Duration;

use russh::{client, Channel};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use crate::connection::{ForwardKind, ForwardSpec};

/// Limit on a SOCKS5 client's greeting + request. A client that connects and
/// then says nothing must not pin a task forever.
const SOCKS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const SOCKS5_VERSION: u8 = 0x05;
const SOCKS5_NO_AUTH: u8 = 0x00;
const SOCKS5_NO_ACCEPTABLE_METHODS: u8 = 0xFF;
const SOCKS5_CMD_CONNECT: u8 = 0x01;
const SOCKS5_ATYP_IPV4: u8 = 0x01;
const SOCKS5_ATYP_DOMAIN: u8 = 0x03;
const SOCKS5_ATYP_IPV6: u8 = 0x04;

/// A request from a forwarding task to the one place allowed to speak to the
/// russh handle. Every variant is `Send`: `Inbound` crosses over from the
/// russh session task.
pub(crate) enum ForwardMsg {
    /// Open a `direct-tcpip` channel for a `-L` target or a SOCKS5 request.
    OpenChannel {
        host: String,
        port: u16,
        originator: SocketAddr,
        /// Carries the raw russh error: the SOCKS5 reply byte depends on which
        /// failure it was, so stringifying here would lose the answer.
        reply: tokio::sync::oneshot::Sender<Result<Channel<client::Msg>, russh::Error>>,
    },
    /// The server opened a channel for a port it is listening on for us (`-R`).
    Inbound {
        channel: Channel<client::Msg>,
        connected_address: String,
        connected_port: u16,
    },
    /// One forwarded connection started (`+1`) or ended (`-1`).
    ConnectionDelta { forward_id: String, delta: i32 },
    /// The local listener died, so the forward is over.
    ListenerFailed { forward_id: String, reason: String },
}

/// One forward that is currently up, owned by `ssh::run_connection`.
pub(crate) struct RunningForward {
    pub(crate) spec: ForwardSpec,
    /// The port actually listening, which differs from `spec.bind_port` only
    /// when the server was asked to pick one.
    pub(crate) bound_port: u16,
    pub(crate) active: u32,
    /// Never sent on. Held only so that dropping this struct closes the
    /// channel, which is what unwinds the accept loop and every live pump.
    _stop_tx: async_channel::Sender<()>,
    stop_rx: async_channel::Receiver<()>,
}

impl RunningForward {
    pub(crate) fn new(spec: ForwardSpec, bound_port: u16) -> Self {
        let (stop_tx, stop_rx) = async_channel::bounded(1);
        Self {
            spec,
            bound_port,
            active: 0,
            _stop_tx: stop_tx,
            stop_rx,
        }
    }

    /// Hand to every task this forward spawns; it resolves when the forward is
    /// dropped.
    pub(crate) fn stop_signal(&self) -> async_channel::Receiver<()> {
        self.stop_rx.clone()
    }
}

/// Accept local connections until the forward is stopped or the socket breaks.
pub(crate) async fn run_listener(
    spec: ForwardSpec,
    listener: TcpListener,
    stop_rx: async_channel::Receiver<()>,
    forward_tx: async_channel::Sender<ForwardMsg>,
) {
    loop {
        tokio::select! {
            _ = stop_rx.recv() => break,
            accepted = listener.accept() => {
                let (tcp, peer) = match accepted {
                    Ok(pair) => pair,
                    // A client that resets between the handshake and the accept
                    // fails this one call, not the socket. Tearing the forward
                    // down here would kill it on traffic that is entirely normal
                    // for a browser talking to a SOCKS5 proxy.
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionAborted => {
                        tracing::debug!(id = %spec.id, %e, "dropped connection before accept");
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(id = %spec.id, %e, "forward listener stopped");
                        let _ = forward_tx.try_send(ForwardMsg::ListenerFailed {
                            forward_id: spec.id.clone(),
                            reason: e.to_string(),
                        });
                        break;
                    }
                };
                tokio::task::spawn_local(handle_outbound(
                    spec.clone(),
                    tcp,
                    peer,
                    stop_rx.clone(),
                    forward_tx.clone(),
                ));
            }
        }
    }
    tracing::debug!(id = %spec.id, "forward listener finished");
}

/// One accepted local connection: work out where it wants to go, get a channel
/// for it, then move bytes until either end closes.
async fn handle_outbound(
    spec: ForwardSpec,
    mut tcp: TcpStream,
    peer: SocketAddr,
    stop_rx: async_channel::Receiver<()>,
    forward_tx: async_channel::Sender<ForwardMsg>,
) {
    let is_socks = spec.kind == ForwardKind::Dynamic;
    let target = match resolve_outbound_target(&spec, &mut tcp, peer).await {
        Some(target) => target,
        None => return,
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let queued = forward_tx.try_send(ForwardMsg::OpenChannel {
        host: target.host.clone(),
        port: target.port,
        originator: peer,
        reply: reply_tx,
    });
    if queued.is_err() {
        return;
    }

    let channel = match reply_rx.await {
        Ok(Ok(channel)) => channel,
        Ok(Err(e)) => {
            tracing::warn!(
                id = %spec.id, host = %target.host, port = target.port, %e,
                "the server would not open the forwarded connection",
            );
            if is_socks {
                let _ = tcp.write_all(&socks5_reply(socks5_error_code(&e))).await;
            }
            return;
        }
        // The connection died while the open was queued.
        Err(_) => return,
    };

    if is_socks && tcp.write_all(&socks5_reply(0x00)).await.is_err() {
        let _ = channel.close().await;
        return;
    }

    pump(&spec.id, tcp, channel, stop_rx, &forward_tx).await;
}

/// Where an accepted connection is headed: fixed for `-L`, negotiated per
/// connection for `-D`. `None` means it was answered and closed.
async fn resolve_outbound_target(
    spec: &ForwardSpec,
    tcp: &mut TcpStream,
    peer: SocketAddr,
) -> Option<Socks5Target> {
    if spec.kind != ForwardKind::Dynamic {
        return Some(Socks5Target {
            host: spec.target_host.clone(),
            port: spec.target_port,
        });
    }

    let handshake = tokio::time::timeout(SOCKS_HANDSHAKE_TIMEOUT, socks5_handshake(tcp)).await;
    let rejected = match handshake {
        Err(_) => {
            tracing::debug!(%peer, "socks5 client never finished its request");
            return None;
        }
        Ok(Ok(target)) => return Some(target),
        Ok(Err(reject)) => reject,
    };
    tracing::debug!(%peer, ?rejected, "socks5 request refused");
    if let Some(code) = rejected.reply_code() {
        let _ = tcp.write_all(&socks5_reply(code)).await;
    }
    None
}

/// One connection the server accepted on our behalf: dial the local target and
/// pump. The channel is already open, so a failure here has to close it.
pub(crate) async fn pump_inbound(
    spec: ForwardSpec,
    channel: Channel<client::Msg>,
    stop_rx: async_channel::Receiver<()>,
    forward_tx: async_channel::Sender<ForwardMsg>,
) {
    let target = (spec.target_host.as_str(), spec.target_port);
    // The dial watches the stop signal too. A target that blackholes us takes
    // the OS connect timeout to fail, and until then this task would still be
    // holding a channel for a forward the user has already stopped.
    let dialed = tokio::select! {
        _ = stop_rx.recv() => {
            let _ = channel.close().await;
            return;
        }
        dialed = TcpStream::connect(target) => dialed,
    };
    let tcp = match dialed {
        Ok(tcp) => tcp,
        Err(e) => {
            tracing::warn!(
                id = %spec.id, host = %spec.target_host, port = spec.target_port, %e,
                "remote forward could not reach its local target",
            );
            let _ = channel.close().await;
            return;
        }
    };
    pump(&spec.id, tcp, channel, stop_rx, &forward_tx).await;
}

/// Move bytes both ways until one side closes or the forward is stopped. The
/// connection count is bumped here so every increment has a matching decrement.
async fn pump(
    forward_id: &str,
    mut tcp: TcpStream,
    channel: Channel<client::Msg>,
    stop_rx: async_channel::Receiver<()>,
    forward_tx: &async_channel::Sender<ForwardMsg>,
) {
    let _ = forward_tx.try_send(ForwardMsg::ConnectionDelta {
        forward_id: forward_id.to_string(),
        delta: 1,
    });

    let mut ssh = channel.into_stream();
    tokio::select! {
        _ = stop_rx.recv() => {
            // `into_stream` consumed the Channel, so `close()` is gone; an EOF
            // makes the server drop its side and close the channel for us.
            let _ = ssh.shutdown().await;
        }
        copied = tokio::io::copy_bidirectional(&mut tcp, &mut ssh) => {
            match copied {
                Ok((to_server, to_client)) => tracing::debug!(
                    forward_id, to_server, to_client, "forwarded connection closed",
                ),
                Err(e) => tracing::debug!(forward_id, %e, "forwarded connection ended"),
            }
        }
    }

    let _ = forward_tx.try_send(ForwardMsg::ConnectionDelta {
        forward_id: forward_id.to_string(),
        delta: -1,
    });
}

/// Where a SOCKS5 client asked to be connected. A domain name is passed to the
/// server verbatim and never resolved here: remote DNS is the point of `-D`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Socks5Target {
    pub(crate) host: String,
    pub(crate) port: u16,
}

/// Why a SOCKS5 request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Socks5Reject {
    NoAcceptableMethods,
    Command,
    AddressType,
    Malformed,
}

impl Socks5Reject {
    /// REP byte to answer with. `None` = there is nothing to answer: either the
    /// greeting was already refused, or the stream is not SOCKS5 at all.
    fn reply_code(self) -> Option<u8> {
        match self {
            Socks5Reject::NoAcceptableMethods | Socks5Reject::Malformed => None,
            Socks5Reject::Command => Some(0x07),
            Socks5Reject::AddressType => Some(0x08),
        }
    }
}

/// Agree on "no authentication", then read the CONNECT request. Nothing past
/// the method selection is written here: the final reply depends on whether the
/// SSH channel opens, which only the caller knows.
async fn socks5_handshake<S>(stream: &mut S) -> Result<Socks5Target, Socks5Reject>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut greeting = [0u8; 2];
    read_or_malformed(stream, &mut greeting).await?;
    let mut methods = vec![0u8; greeting[1] as usize];
    read_or_malformed(stream, &mut methods).await?;

    match parse_socks5_greeting(greeting[0], &methods) {
        Ok(()) => {
            write_or_malformed(stream, &[SOCKS5_VERSION, SOCKS5_NO_AUTH]).await?;
        }
        Err(Socks5Reject::NoAcceptableMethods) => {
            let _ = stream
                .write_all(&[SOCKS5_VERSION, SOCKS5_NO_ACCEPTABLE_METHODS])
                .await;
            return Err(Socks5Reject::NoAcceptableMethods);
        }
        Err(other) => return Err(other),
    }

    let request = read_socks5_request(stream).await?;
    parse_socks5_request(&request)
}

/// Read a CONNECT request off the wire, one field at a time. `read_exact` and
/// nothing else: a buffered reader would swallow the first payload bytes the
/// client sends right after the request.
async fn read_socks5_request<S>(stream: &mut S) -> Result<Vec<u8>, Socks5Reject>
where
    S: AsyncRead + Unpin,
{
    let mut request = vec![0u8; 4];
    read_or_malformed(stream, &mut request).await?;

    let address_len = match request[3] {
        SOCKS5_ATYP_IPV4 => 4,
        SOCKS5_ATYP_IPV6 => 16,
        SOCKS5_ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            read_or_malformed(stream, &mut len).await?;
            request.push(len[0]);
            len[0] as usize
        }
        // Unknown type: its length is unknowable, so stop reading and let the
        // parser answer with REP 0x08.
        _ => return Ok(request),
    };

    let mut rest = vec![0u8; address_len + 2];
    read_or_malformed(stream, &mut rest).await?;
    request.extend_from_slice(&rest);
    Ok(request)
}

async fn read_or_malformed<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut [u8],
) -> Result<(), Socks5Reject> {
    stream
        .read_exact(buf)
        .await
        .map(|_| ())
        .map_err(|_| Socks5Reject::Malformed)
}

async fn write_or_malformed<S: AsyncWrite + Unpin>(
    stream: &mut S,
    bytes: &[u8],
) -> Result<(), Socks5Reject> {
    stream
        .write_all(bytes)
        .await
        .map_err(|_| Socks5Reject::Malformed)
}

/// `[ver][nmethods]` plus the offered methods. We only speak "no auth".
fn parse_socks5_greeting(version: u8, methods: &[u8]) -> Result<(), Socks5Reject> {
    if version != SOCKS5_VERSION {
        return Err(Socks5Reject::Malformed);
    }
    if !methods.contains(&SOCKS5_NO_AUTH) {
        return Err(Socks5Reject::NoAcceptableMethods);
    }
    Ok(())
}

/// `[ver][cmd][rsv][atyp][addr...][port]`, exactly as [`read_socks5_request`]
/// assembles it. Pure, so the wire format is tested without a socket.
fn parse_socks5_request(bytes: &[u8]) -> Result<Socks5Target, Socks5Reject> {
    let (head, body) = take(bytes, 4)?;
    if head[0] != SOCKS5_VERSION {
        return Err(Socks5Reject::Malformed);
    }
    if head[1] != SOCKS5_CMD_CONNECT {
        return Err(Socks5Reject::Command);
    }

    let (host, rest) = match head[3] {
        SOCKS5_ATYP_IPV4 => {
            let (addr, rest) = take(body, 4)?;
            let octets = [addr[0], addr[1], addr[2], addr[3]];
            (std::net::Ipv4Addr::from(octets).to_string(), rest)
        }
        SOCKS5_ATYP_IPV6 => {
            let (addr, rest) = take(body, 16)?;
            let mut octets = [0u8; 16];
            octets.copy_from_slice(addr);
            (std::net::Ipv6Addr::from(octets).to_string(), rest)
        }
        SOCKS5_ATYP_DOMAIN => {
            let (len, after_len) = take(body, 1)?;
            let (name, rest) = take(after_len, len[0] as usize)?;
            let host = String::from_utf8(name.to_vec()).map_err(|_| Socks5Reject::AddressType)?;
            (host, rest)
        }
        _ => return Err(Socks5Reject::AddressType),
    };

    let (port, tail) = take(rest, 2)?;
    if !tail.is_empty() {
        return Err(Socks5Reject::Malformed);
    }
    Ok(Socks5Target {
        host,
        port: u16::from_be_bytes([port[0], port[1]]),
    })
}

fn take(bytes: &[u8], count: usize) -> Result<(&[u8], &[u8]), Socks5Reject> {
    bytes.split_at_checked(count).ok_or(Socks5Reject::Malformed)
}

/// The 10-byte CONNECT reply. BND.ADDR / BND.PORT are zeroed; clients ignore
/// them for CONNECT.
fn socks5_reply(rep: u8) -> [u8; 10] {
    [
        SOCKS5_VERSION,
        rep,
        0x00,
        SOCKS5_ATYP_IPV4,
        0,
        0,
        0,
        0,
        0,
        0,
    ]
}

/// REP byte for a target the server refused to reach.
fn socks5_error_code(error: &russh::Error) -> u8 {
    match error {
        russh::Error::ChannelOpenFailure(russh::ChannelOpenFailure::ConnectFailed) => 0x05,
        russh::Error::ChannelOpenFailure(russh::ChannelOpenFailure::AdministrativelyProhibited) => {
            0x02
        }
        _ => 0x01,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(atyp: u8, address: &[u8], port: u16) -> Vec<u8> {
        let mut bytes = vec![SOCKS5_VERSION, SOCKS5_CMD_CONNECT, 0x00, atyp];
        bytes.extend_from_slice(address);
        bytes.extend_from_slice(&port.to_be_bytes());
        bytes
    }

    #[tokio::test]
    async fn dropping_a_forward_releases_every_task_waiting_on_it() {
        // The stop channel is never sent on, so only closing it can reach the
        // listener and every pump.
        let running = RunningForward::new(
            ForwardSpec {
                id: "id".into(),
                kind: ForwardKind::Remote,
                bind_address: "localhost".into(),
                bind_port: 8080,
                target_host: "db.internal".into(),
                target_port: 5432,
            },
            8080,
        );
        let listener = running.stop_signal();
        let first_pump = running.stop_signal();
        let second_pump = running.stop_signal();

        drop(running);

        assert!(listener.recv().await.is_err());
        assert!(first_pump.recv().await.is_err());
        assert!(second_pump.recv().await.is_err());
    }

    #[test]
    fn parses_an_ipv4_target() {
        let parsed = parse_socks5_request(&request(SOCKS5_ATYP_IPV4, &[192, 0, 2, 7], 443));
        assert_eq!(
            parsed,
            Ok(Socks5Target {
                host: "192.0.2.7".into(),
                port: 443
            })
        );
    }

    #[test]
    fn parses_a_domain_target_without_resolving_it() {
        let name = b"db.internal";
        let mut address = vec![name.len() as u8];
        address.extend_from_slice(name);
        let parsed = parse_socks5_request(&request(SOCKS5_ATYP_DOMAIN, &address, 5432));
        assert_eq!(
            parsed,
            Ok(Socks5Target {
                host: "db.internal".into(),
                port: 5432
            })
        );
    }

    #[test]
    fn parses_an_ipv6_target() {
        let mut address = [0u8; 16];
        address[15] = 1;
        let parsed = parse_socks5_request(&request(SOCKS5_ATYP_IPV6, &address, 80));
        assert_eq!(
            parsed,
            Ok(Socks5Target {
                host: "::1".into(),
                port: 80
            })
        );
    }

    #[test]
    fn only_connect_is_supported() {
        let mut bind = request(SOCKS5_ATYP_IPV4, &[192, 0, 2, 7], 443);
        bind[1] = 0x02;
        assert_eq!(parse_socks5_request(&bind), Err(Socks5Reject::Command));
        assert_eq!(Socks5Reject::Command.reply_code(), Some(0x07));
    }

    #[test]
    fn an_unknown_address_type_is_refused() {
        let bytes = vec![SOCKS5_VERSION, SOCKS5_CMD_CONNECT, 0x00, 0x02];
        assert_eq!(parse_socks5_request(&bytes), Err(Socks5Reject::AddressType));
        assert_eq!(Socks5Reject::AddressType.reply_code(), Some(0x08));
    }

    #[test]
    fn a_non_utf8_domain_is_refused() {
        let address = vec![2u8, 0xff, 0xfe];
        let parsed = parse_socks5_request(&request(SOCKS5_ATYP_DOMAIN, &address, 80));
        assert_eq!(parsed, Err(Socks5Reject::AddressType));
    }

    #[test]
    fn truncated_and_malformed_requests_are_rejected_without_a_reply() {
        // Nothing at all, a half-read header, an address cut short, and a
        // missing port.
        assert_eq!(parse_socks5_request(&[]), Err(Socks5Reject::Malformed));
        assert_eq!(
            parse_socks5_request(&[0x05, 0x01, 0x00]),
            Err(Socks5Reject::Malformed)
        );
        let short_address = vec![
            SOCKS5_VERSION,
            SOCKS5_CMD_CONNECT,
            0x00,
            SOCKS5_ATYP_IPV4,
            10,
            0,
        ];
        assert_eq!(
            parse_socks5_request(&short_address),
            Err(Socks5Reject::Malformed)
        );
        let no_port = vec![
            SOCKS5_VERSION,
            SOCKS5_CMD_CONNECT,
            0x00,
            SOCKS5_ATYP_IPV4,
            192,
            0,
            2,
            7,
        ];
        assert_eq!(parse_socks5_request(&no_port), Err(Socks5Reject::Malformed));
        assert_eq!(Socks5Reject::Malformed.reply_code(), None);
    }

    #[test]
    fn a_domain_shorter_than_its_length_byte_is_malformed() {
        // The length byte claims eight, but only "host" and the port follow.
        let mut address = vec![8u8];
        address.extend_from_slice(b"host");
        let parsed = parse_socks5_request(&request(SOCKS5_ATYP_DOMAIN, &address, 80));
        assert_eq!(parsed, Err(Socks5Reject::Malformed));
    }

    #[test]
    fn bytes_past_the_port_are_malformed() {
        let mut oversized = request(SOCKS5_ATYP_IPV4, &[192, 0, 2, 7], 443);
        oversized.push(0xff);
        assert_eq!(
            parse_socks5_request(&oversized),
            Err(Socks5Reject::Malformed)
        );
    }

    #[test]
    fn a_request_that_is_not_socks5_is_malformed() {
        let mut http = request(SOCKS5_ATYP_IPV4, &[192, 0, 2, 7], 443);
        http[0] = 0x04;
        assert_eq!(parse_socks5_request(&http), Err(Socks5Reject::Malformed));
    }

    #[tokio::test]
    async fn an_unknown_address_type_stops_the_read_before_the_address() {
        // The length of an unknown address type is unknowable, so reading on
        // would park until the handshake timeout instead of answering 0x08.
        let (mut client, mut server) = tokio::io::duplex(256);
        let head = [SOCKS5_VERSION, SOCKS5_CMD_CONNECT, 0x00, 0x07];
        client
            .write_all(&[head.as_slice(), &[1, 2, 3, 4]].concat())
            .await
            .unwrap();

        let request = read_socks5_request(&mut server).await.unwrap();
        assert_eq!(request, head);
        assert_eq!(
            parse_socks5_request(&request),
            Err(Socks5Reject::AddressType)
        );
    }

    #[tokio::test]
    async fn a_request_cut_short_on_the_wire_is_malformed() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let mut wire = vec![SOCKS5_VERSION, 1, SOCKS5_NO_AUTH];
        // A domain request that stops two bytes into an eleven-byte name.
        wire.extend_from_slice(&[SOCKS5_VERSION, SOCKS5_CMD_CONNECT, 0x00, SOCKS5_ATYP_DOMAIN]);
        wire.extend_from_slice(&[11, b'd', b'b']);
        client.write_all(&wire).await.unwrap();
        client.shutdown().await.unwrap();

        assert_eq!(
            socks5_handshake(&mut server).await,
            Err(Socks5Reject::Malformed)
        );
    }

    #[tokio::test]
    async fn the_largest_greeting_and_domain_a_client_can_send_are_accepted() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        // 255 methods, with the only one we speak last in the list.
        let mut wire = vec![SOCKS5_VERSION, 255];
        wire.extend_from_slice(&vec![0x02u8; 254]);
        wire.push(SOCKS5_NO_AUTH);
        let mut address = vec![255u8];
        address.extend_from_slice(&vec![b'a'; 255]);
        wire.extend_from_slice(&request(SOCKS5_ATYP_DOMAIN, &address, 65535));
        client.write_all(&wire).await.unwrap();

        let target = socks5_handshake(&mut server).await.unwrap();
        assert_eq!(
            target,
            Socks5Target {
                host: "a".repeat(255),
                port: 65535
            }
        );
    }

    #[test]
    fn only_the_no_auth_method_is_accepted() {
        assert_eq!(parse_socks5_greeting(SOCKS5_VERSION, &[0x00, 0x02]), Ok(()));
        assert_eq!(
            parse_socks5_greeting(SOCKS5_VERSION, &[0x02]),
            Err(Socks5Reject::NoAcceptableMethods),
        );
        assert_eq!(
            parse_socks5_greeting(SOCKS5_VERSION, &[]),
            Err(Socks5Reject::NoAcceptableMethods),
        );
        assert_eq!(
            parse_socks5_greeting(0x04, &[0x00]),
            Err(Socks5Reject::Malformed)
        );
        assert_eq!(Socks5Reject::NoAcceptableMethods.reply_code(), None);
    }

    #[test]
    fn the_success_reply_is_ten_zeroed_bytes() {
        assert_eq!(
            socks5_reply(0x00),
            [0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn a_refused_open_picks_the_matching_reply_code() {
        assert_eq!(
            socks5_error_code(&russh::Error::ChannelOpenFailure(
                russh::ChannelOpenFailure::ConnectFailed
            )),
            0x05,
        );
        assert_eq!(
            socks5_error_code(&russh::Error::ChannelOpenFailure(
                russh::ChannelOpenFailure::AdministrativelyProhibited
            )),
            0x02,
        );
        assert_eq!(socks5_error_code(&russh::Error::Disconnect), 0x01);
    }

    #[tokio::test]
    async fn the_handshake_answers_the_greeting_then_reads_the_request() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let mut wire = vec![SOCKS5_VERSION, 1, SOCKS5_NO_AUTH];
        wire.extend_from_slice(&request(SOCKS5_ATYP_IPV4, &[127, 0, 0, 1], 8080));
        // The bytes a client would send right after its request must survive.
        wire.extend_from_slice(b"GET / HTTP/1.1\r\n");
        client.write_all(&wire).await.unwrap();

        let target = socks5_handshake(&mut server).await.unwrap();
        assert_eq!(
            target,
            Socks5Target {
                host: "127.0.0.1".into(),
                port: 8080
            }
        );

        let mut selection = [0u8; 2];
        client.read_exact(&mut selection).await.unwrap();
        assert_eq!(selection, [SOCKS5_VERSION, SOCKS5_NO_AUTH]);

        let mut payload = [0u8; 16];
        server.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"GET / HTTP/1.1\r\n");
    }

    #[tokio::test]
    async fn a_client_offering_no_usable_method_is_told_so() {
        let (mut client, mut server) = tokio::io::duplex(256);
        client.write_all(&[SOCKS5_VERSION, 1, 0x02]).await.unwrap();

        let rejected = socks5_handshake(&mut server).await;
        assert_eq!(rejected, Err(Socks5Reject::NoAcceptableMethods));

        let mut selection = [0u8; 2];
        client.read_exact(&mut selection).await.unwrap();
        assert_eq!(selection, [SOCKS5_VERSION, SOCKS5_NO_ACCEPTABLE_METHODS]);
    }
}
