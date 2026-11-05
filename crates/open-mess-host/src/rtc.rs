//! Module media connections (spec 2026-10-29-live-captions, 5.2): a WebRTC peer connection with
//! one data channel and nothing else, for a module whose manifest names its media peers. No audio
//! or video section is ever offered or accepted, so no RTP goes either way, and every packet
//! passes a socket that drops what goes to or comes from an address outside the manifest's
//! ranges. Data channel messages reach the module in batches through its `events` export.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCBundlePolicy, RTCConfigurationBuilder, RTCIceGatheringState,
    RTCPeerConnectionState, RTCRtcpMuxPolicy, RTCSessionDescription, SettingEngineBuilder,
};
use webrtc::runtime::{AsyncInterval, AsyncTcpListener, AsyncTcpStream, AsyncUdpSocket, JoinHandle, RecvMeta, Runtime, TokioRuntime, Transmit};

use crate::actor::{GuestEvent, Task};
use crate::error::HostError;
use crate::ip_policy::{self, AddressPolicy};
use crate::limits;
use crate::net::{HttpError, blocked};

fn network(e: impl ToString) -> HttpError {
    HttpError::Network(e.to_string())
}

/// The address ranges of a manifest's `media` (spec 5.2), each network address masked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Peers(Vec<(IpAddr, u32)>);

impl Peers {
    /// CIDR ranges: at most `MAX_MEDIA_RANGES`, a prefix of at least /8 (IPv4) or /16 (IPv6), none
    /// touching a private or special network.
    pub(crate) fn parse(ranges: &[String]) -> Result<Peers, HostError> {
        if ranges.is_empty() || ranges.len() > limits::MAX_MEDIA_RANGES {
            return Err(HostError::Load(format!("media access needs 1 to {} address ranges", limits::MAX_MEDIA_RANGES)));
        }
        let mut peers = Vec::new();
        for range in ranges {
            let refuse = |why: &str| HostError::Load(format!("media range {range:?}: {why}"));
            let (addr, len) = range.split_once('/').ok_or_else(|| refuse("not a CIDR range"))?;
            let addr: IpAddr = addr.parse().map_err(|_| refuse("not an IP address"))?;
            let len: u32 = len.parse().map_err(|_| refuse("not a prefix length"))?;
            let (min, max) = if addr.is_ipv4() { (8, 32) } else { (16, 128) };
            if !(min..=max).contains(&len) {
                return Err(refuse(&format!("the prefix must be /{min} to /{max}")));
            }
            let net = mask(addr, len);
            if ip_policy::range_has_special(net, len) {
                return Err(refuse("it holds private or special addresses"));
            }
            peers.push((net, len));
        }
        Ok(Peers(peers))
    }

    fn contains(&self, ip: IpAddr) -> bool {
        let ip = plain(ip);
        self.0.iter().any(|&(net, len)| mask(ip, len) == net && ip.is_ipv4() == net.is_ipv4())
    }
}

/// An IPv4 address mapped into IPv6 (as a dual-stack socket reports it) as plain IPv4.
fn plain(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn mask(ip: IpAddr, len: u32) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4((u32::from(v4) & u32::MAX.checked_shl(32 - len.min(32)).unwrap_or(0)).into()),
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & u128::MAX.checked_shl(128 - len.min(128)).unwrap_or(0)).into()),
    }
}

/// Who a module's media connections may exchange packets with: an address in the manifest's
/// ranges that the address policy allows, and loopback under the tests' policy. Once a connection
/// has its answer, only the answer's candidates (address and port) that the system proxy leaves
/// direct: a peer-reflexive address or port in the ranges was never asked about the proxy.
#[derive(Clone, Debug)]
pub(crate) struct Allowed {
    peers: Peers,
    policy: AddressPolicy,
    /// Set by `accept`; shared with the connection's sockets.
    direct: Arc<Mutex<Option<Vec<SocketAddr>>>>,
}

impl Allowed {
    pub(crate) fn new(peers: Peers, policy: AddressPolicy) -> Allowed {
        Allowed { peers, policy, direct: Arc::default() }
    }

    /// The same ranges with a pinned set of its own, for one connection.
    fn for_connection(&self) -> Allowed {
        Allowed { direct: Arc::default(), ..self.clone() }
    }

    fn pin(&self, direct: &[SocketAddr]) {
        let direct = direct.iter().map(|addr| SocketAddr::new(plain(addr.ip()), addr.port())).collect();
        *self.direct.lock().unwrap_or_else(PoisonError::into_inner) = Some(direct);
    }

    /// Whether a packet may go to or come from `addr`.
    fn allows(&self, addr: SocketAddr) -> bool {
        if let Some(direct) = &*self.direct.lock().unwrap_or_else(PoisonError::into_inner)
            && !direct.contains(&SocketAddr::new(plain(addr.ip()), addr.port()))
        {
            return false;
        }
        self.in_ranges(addr.ip())
    }

    /// Whether `ip` is in the manifest's ranges and allowed by the address policy.
    fn in_ranges(&self, ip: IpAddr) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        if self.policy == AddressPolicy::AllowLoopbackForTests && ip.is_loopback() {
            return true;
        }
        self.peers.contains(ip) && ip_policy::is_allowed(ip, self.policy)
    }

    /// The local addresses to bind: every interface (the library skips loopback and link-local),
    /// or loopback alone in tests. ponytail: IPv4 only; add `[::]:0` once a media peer needs IPv6.
    fn bind(&self) -> String {
        #[cfg(any(test, feature = "test-support"))]
        if self.policy == AddressPolicy::AllowLoopbackForTests {
            return "127.0.0.1:0".to_string();
        }
        "0.0.0.0:0".to_string()
    }
}

/// A UDP socket that sends only to allowed addresses and drops datagrams from any other, so
/// neither a candidate nor a peer-reflexive address outside the ranges ever carries traffic.
#[derive(Debug)]
struct PolicedUdp {
    inner: Arc<dyn AsyncUdpSocket>,
    allowed: Allowed,
}

impl AsyncUdpSocket for PolicedUdp {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn poll_send(&self, cx: &mut Context<'_>, transmit: &Transmit<'_>) -> Poll<io::Result<usize>> {
        if !self.allowed.allows(transmit.destination) {
            // Reported as sent: an error could make the library give the socket up.
            return Poll::Ready(Ok(transmit.contents.len()));
        }
        self.inner.poll_send(cx, transmit)
    }

    fn poll_recv(&self, cx: &mut Context<'_>, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        // One datagram at a time (no GRO), so a refused one is simply read past.
        let (Some(buf), Some(meta)) = (bufs.get_mut(..1), meta.get_mut(..1)) else { return Poll::Ready(Ok(0)) };
        loop {
            match self.inner.poll_recv(cx, buf, meta) {
                Poll::Ready(Ok(n)) if n > 0 && meta.first().is_some_and(|m| !self.allowed.allows(m.addr)) => continue,
                other => return other,
            }
        }
    }
}

/// The tokio runtime with its sockets policed: UDP through [`PolicedUdp`], no TCP (ICE over TCP is
/// not used) and no name lookups (there is no STUN or TURN server to look up).
#[derive(Debug)]
struct Policed {
    inner: TokioRuntime,
    allowed: Allowed,
}

fn refused<T>(what: &str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{what} is not allowed for module media")))
}

impl Runtime for Policed {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) -> Box<dyn JoinHandle> {
        self.inner.spawn(future)
    }

    fn wrap_udp_socket(&self, socket: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        let inner = self.inner.wrap_udp_socket(socket)?;
        Ok(Arc::new(PolicedUdp { inner, allowed: self.allowed.clone() }))
    }

    fn wrap_tcp_listener(&self, _listener: std::net::TcpListener) -> io::Result<Arc<dyn AsyncTcpListener>> {
        refused("a TCP listener")
    }

    fn connect_tcp<'a>(&'a self, _remote_addr: SocketAddr) -> Pin<Box<dyn Future<Output = io::Result<Arc<dyn AsyncTcpStream>>> + Send + 'a>> {
        Box::pin(async { refused("a TCP connection") })
    }

    fn resolve_host<'a>(&'a self, _host: &'a str) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        Box::pin(async { refused("a name lookup") })
    }

    fn now(&self) -> std::time::Instant {
        self.inner.now()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        self.inner.sleep(duration)
    }

    fn interval(&self, period: Duration) -> Box<dyn AsyncInterval> {
        self.inner.interval(period)
    }

    fn block_on(&self, future: Pin<Box<dyn Future<Output = ()> + '_>>) {
        self.inner.block_on(future)
    }

    fn name(&self) -> &'static str {
        "open_mess"
    }
}

/// Passes the connection's state changes and the end of candidate gathering on.
struct Handler {
    states: mpsc::Sender<RTCPeerConnectionState>,
    gathered: mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.try_send(()).ok();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        self.states.try_send(state).ok();
    }
}

/// The checked answer: only its data section, only the candidates in the module's ranges.
fn check_answer(answer: &str, allowed: &Allowed) -> Result<(Vec<String>, Vec<SocketAddr>), HttpError> {
    let mut lines = Vec::new();
    let mut candidates = Vec::new();
    let mut sections = 0;
    for line in answer.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(media) = line.strip_prefix("m=") {
            sections += 1;
            if sections > 1 || !(media.starts_with("application ") && media.contains("DTLS/SCTP")) {
                return Err(blocked("the answer has a section other than the data channel"));
            }
        }
        if let Some(candidate) = line.strip_prefix("a=candidate:") {
            // foundation component transport priority address port "typ" type ...
            let fields: Vec<&str> = candidate.split_whitespace().collect();
            let addr = fields.get(4).and_then(|a| a.parse::<IpAddr>().ok()).zip(fields.get(5).and_then(|p| p.parse::<u16>().ok()));
            match addr {
                Some((ip, port)) if allowed.in_ranges(ip) => candidates.push(SocketAddr::new(ip, port)),
                // A name (mDNS), a malformed line or an address outside the ranges.
                _ => continue,
            }
        }
        lines.push(line.to_string());
    }
    if sections == 0 {
        return Err(blocked("the answer has no data channel section"));
    }
    if candidates.is_empty() {
        return Err(blocked("no candidate of the answer is in the module's media ranges"));
    }
    Ok((lines, candidates))
}

/// The answer without the candidates the system proxy covers, and the addresses left; refused when
/// none is left.
async fn without_proxied(lines: Vec<String>, candidates: Vec<SocketAddr>) -> Result<(String, Vec<SocketAddr>), HttpError> {
    let direct = crate::proxy::direct(candidates).await?;
    if direct.is_empty() {
        return Err(blocked("proxy: your network sends traffic through a proxy; media needs a direct connection"));
    }
    let kept: Vec<String> = lines
        .into_iter()
        .filter(|line| {
            let Some(candidate) = line.strip_prefix("a=candidate:") else { return true };
            let fields: Vec<&str> = candidate.split_whitespace().collect();
            let addr = fields.get(4).and_then(|a| a.parse::<IpAddr>().ok()).zip(fields.get(5).and_then(|p| p.parse::<u16>().ok()));
            addr.is_some_and(|(ip, port)| direct.contains(&SocketAddr::new(ip, port)))
        })
        .collect();
    Ok((kept.join("\r\n") + "\r\n", direct))
}

/// Data channel messages waiting for the module, within the per-connection limits.
#[derive(Default)]
struct Pending {
    messages: VecDeque<Vec<u8>>,
    bytes: usize,
    dropped: u32,
    window: Option<Instant>,
    window_messages: usize,
    window_bytes: usize,
}

impl Pending {
    fn push(&mut self, data: Vec<u8>) {
        let now = Instant::now();
        if self.window.is_none_or(|start| now.duration_since(start) >= Duration::from_secs(1)) {
            (self.window, self.window_messages, self.window_bytes) = (Some(now), 0, 0);
        }
        let len = data.len();
        let over = len > limits::MAX_RTC_MESSAGE
            || self.window_messages >= limits::MAX_RTC_MESSAGES_PER_SECOND
            || self.window_bytes.saturating_add(len) > limits::MAX_RTC_BYTES_PER_SECOND
            || self.messages.len() >= limits::MAX_RTC_QUEUE
            || self.bytes.saturating_add(len) > limits::MAX_RTC_QUEUE_BYTES;
        if over {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.window_messages += 1;
        self.window_bytes += len;
        self.bytes += len;
        self.messages.push_back(data);
    }

    fn take(&mut self, conn: u32) -> Option<GuestEvent> {
        if self.messages.is_empty() && self.dropped == 0 {
            return None;
        }
        self.bytes = 0;
        let messages = std::mem::take(&mut self.messages).into();
        Some(GuestEvent::RtcData { conn, messages, dropped: std::mem::take(&mut self.dropped) })
    }
}

/// Moves one connection's data channel messages and state to the module until the connection
/// fails, closes or reaches its lifetime.
async fn serve(conn: u32, channel: Arc<dyn DataChannel>, mut states: mpsc::Receiver<RTCPeerConnectionState>, events: mpsc::Sender<GuestEvent>) {
    let mut pending = Pending::default();
    // A state for the module after the messages before it; the last one ends the loop.
    let mut state: Option<&'static str> = None;
    let mut ending = false;
    let end = tokio::time::sleep(limits::RTC_LIFETIME);
    tokio::pin!(end);
    loop {
        let ready = !pending.messages.is_empty() || pending.dropped > 0 || state.is_some();
        tokio::select! {
            permit = events.reserve(), if ready => {
                let Ok(permit) = permit else { return };
                match pending.take(conn) {
                    Some(event) => permit.send(event),
                    None => {
                        if let Some(state) = state.take() {
                            permit.send(GuestEvent::RtcState { conn, state: state.to_string() });
                        }
                        if ending {
                            return;
                        }
                    }
                }
            }
            event = channel.poll(), if !ending => match event {
                Some(DataChannelEvent::OnMessage(message)) => pending.push(message.data.to_vec()),
                Some(DataChannelEvent::OnClose) | None => (state, ending) = (Some("closed"), true),
                Some(_) => {}
            },
            changed = states.recv(), if !ending => match changed {
                Some(RTCPeerConnectionState::Connected) => state = Some("connected"),
                Some(RTCPeerConnectionState::Failed) => (state, ending) = (Some("failed"), true),
                Some(RTCPeerConnectionState::Closed) | None => (state, ending) = (Some("closed"), true),
                Some(_) => {}
            },
            () = &mut end, if !ending => (state, ending) = (Some("closed"), true),
            else => return,
        }
    }
}

struct Conn {
    pc: Arc<dyn PeerConnection>,
    allowed: Allowed,
    answered: bool,
    _task: Task,
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Ends the library's own tasks and sockets; outside a runtime there is nothing to end.
        let pc = self.pc.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                pc.close().await.ok();
            });
        }
    }
}

/// One module instance's media connections; dropping it closes them.
#[derive(Default)]
pub(crate) struct Connections {
    open: HashMap<u32, Conn>,
    last: u32,
}

impl Connections {
    pub(crate) async fn open(
        &mut self,
        allowed: Option<&Allowed>,
        label: &str,
        events: &mpsc::Sender<GuestEvent>,
    ) -> Result<(u32, String), HttpError> {
        let allowed = allowed.ok_or_else(|| blocked("the module's manifest has no media access"))?.for_connection();
        if self.open.len() >= limits::MAX_RTC {
            return Err(blocked(format!("at most {} open media connections", limits::MAX_RTC)));
        }
        if label.is_empty() || label.len() > limits::MAX_RTC_LABEL {
            return Err(blocked(format!("the label must be 1 to {} bytes", limits::MAX_RTC_LABEL)));
        }
        let (state_tx, states) = mpsc::channel(8);
        let (gathered_tx, mut gathered) = mpsc::channel(1);
        #[cfg_attr(not(any(test, feature = "test-support")), allow(unused_mut))]
        let mut settings = SettingEngineBuilder::new();
        #[cfg(any(test, feature = "test-support"))]
        if allowed.policy == AddressPolicy::AllowLoopbackForTests {
            settings = settings.with_include_loopback_candidate(true);
        }
        let configuration =
            RTCConfigurationBuilder::new().with_bundle_policy(RTCBundlePolicy::MaxBundle).with_rtcp_mux_policy(RTCRtcpMuxPolicy::Require).build();
        let pc = PeerConnectionBuilder::new()
            .with_configuration(configuration)
            .with_media_engine(MediaEngine::default())
            .with_setting_engine(settings.build())
            .with_handler(Arc::new(Handler { states: state_tx, gathered: gathered_tx }))
            .with_runtime(Arc::new(Policed { inner: TokioRuntime, allowed: allowed.clone() }))
            .with_udp_addrs(vec![allowed.bind()])
            .build()
            .await
            .map_err(network)?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);
        let opened = async {
            let channel = pc.create_data_channel(label, None).await.map_err(network)?;
            let offer = pc.create_offer(None).await.map_err(network)?;
            pc.set_local_description(offer).await.map_err(network)?;
            tokio::time::timeout(limits::RTC_GATHER_TIMEOUT, gathered.recv()).await.ok();
            let offer = pc.local_description().await.ok_or_else(|| network("no local description"))?;
            Ok::<_, HttpError>((channel, offer.sdp))
        };
        let (channel, sdp) = match opened.await {
            Ok(opened) => opened,
            Err(e) => {
                pc.close().await.ok();
                return Err(e);
            }
        };
        self.last = self.last.wrapping_add(1);
        let conn = self.last;
        let task = Task(tokio::spawn(serve(conn, channel, states, events.clone())));
        self.open.insert(conn, Conn { pc, allowed, answered: false, _task: task });
        Ok((conn, sdp))
    }

    pub(crate) async fn accept(&mut self, conn: u32, answer: &str) -> Result<(), HttpError> {
        let open = self.open.get_mut(&conn).ok_or_else(|| network("no such media connection"))?;
        if open.answered {
            return Err(blocked("the connection has its answer already"));
        }
        if answer.len() > limits::MAX_SDP {
            return Err(HttpError::TooLarge);
        }
        let (lines, candidates) = check_answer(answer, &open.allowed)?;
        let (sdp, direct) = without_proxied(lines, candidates).await?;
        let answer = RTCSessionDescription::answer(sdp).map_err(network)?;
        // Before the answer nothing reaches a far end: ICE has no remote candidate to check.
        open.allowed.pin(&direct);
        open.pc.set_remote_description(answer).await.map_err(network)?;
        open.answered = true;
        Ok(())
    }

    /// Drops a connection: one the module closes, or one that failed or ended.
    pub(crate) fn close(&mut self, conn: u32) {
        self.open.remove(&conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(ranges: &[&str]) -> Allowed {
        Allowed::new(Peers::parse(&ranges.iter().map(|r| r.to_string()).collect::<Vec<_>>()).unwrap(), AddressPolicy::Strict)
    }

    #[test]
    fn media_ranges_are_checked() {
        assert!(Peers::parse(&["52.112.0.0/14".to_string(), "2603:1063::/38".to_string()]).is_ok());
        for bad in ["52.112.0.0", "52.112.0.0/7", "0.0.0.0/0", "10.0.0.0/8", "192.168.1.0/24", "2603::/15", "fe80::/64", "x/8", "52.112.0.0/33"] {
            assert!(Peers::parse(&[bad.to_string()]).is_err(), "{bad}");
        }
        assert!(Peers::parse(&[]).is_err());
        assert!(Peers::parse(&vec!["52.112.0.0/14".to_string(); limits::MAX_MEDIA_RANGES + 1]).is_err());
    }

    #[test]
    fn only_addresses_in_the_ranges_are_allowed() {
        let allowed = allowed(&["52.112.0.0/14"]);
        assert!(allowed.allows("52.113.1.2:3478".parse().unwrap()));
        assert!(allowed.allows("[::ffff:52.113.1.2]:3478".parse().unwrap()));
        for other in ["52.116.0.1:3478", "8.8.8.8:3478", "127.0.0.1:3478", "10.0.0.1:3478"] {
            assert!(!allowed.allows(other.parse().unwrap()), "{other}");
        }
    }

    const ANSWER: &str = "v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n\
        m=application 3478 UDP/DTLS/SCTP webrtc-datachannel\r\nc=IN IP4 52.113.1.2\r\na=mid:0\r\n\
        a=candidate:1 1 UDP 2130706431 52.113.1.2 3478 typ relay\r\n\
        a=candidate:2 1 UDP 2130706431 8.8.8.8 3478 typ relay\r\n\
        a=candidate:3 1 UDP 2130706431 host.local 3478 typ host\r\n\
        a=candidate:4 1 UDP 2130706431 10.0.0.1 3478 typ host\r\na=sctp-port:5000\r\n";

    #[test]
    fn after_the_answer_only_its_direct_candidates_are_reachable() {
        let allowed = allowed(&["52.112.0.0/14"]).for_connection();
        assert!(allowed.allows("52.113.1.3:3478".parse().unwrap()));
        allowed.pin(&["52.113.1.2:3478".parse().unwrap()]);
        assert!(allowed.allows("52.113.1.2:3478".parse().unwrap()));
        assert!(allowed.allows("[::ffff:52.113.1.2]:3478".parse().unwrap()));
        // In the ranges, but never asked about the proxy: a peer-reflexive address, or another port
        // of the candidate's address that a PAC script may send through a proxy.
        assert!(!allowed.allows("52.113.1.3:3478".parse().unwrap()));
        assert!(!allowed.allows("52.113.1.2:5000".parse().unwrap()));
        // Another connection of the module keeps its own set.
        assert!(allowed.for_connection().allows("52.113.1.3:3478".parse().unwrap()));
    }

    #[test]
    fn the_answer_keeps_only_candidates_in_the_ranges() {
        let (lines, candidates) = check_answer(ANSWER, &allowed(&["52.112.0.0/14"])).unwrap();
        assert_eq!(candidates, ["52.113.1.2:3478".parse::<SocketAddr>().unwrap()]);
        assert_eq!(lines.iter().filter(|l| l.starts_with("a=candidate:")).count(), 1);
        assert!(lines.iter().any(|l| l == "a=sctp-port:5000"));
    }

    #[test]
    fn an_answer_with_media_or_no_allowed_candidate_is_refused() {
        let audio = ANSWER.replace("a=sctp-port:5000\r\n", "a=sctp-port:5000\r\nm=audio 3478 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n");
        assert!(check_answer(&audio, &allowed(&["52.112.0.0/14"])).is_err());
        let video_first = ANSWER.replace("m=application 3478 UDP/DTLS/SCTP webrtc-datachannel", "m=video 3478 UDP/TLS/RTP/SAVPF 96");
        assert!(check_answer(&video_first, &allowed(&["52.112.0.0/14"])).is_err());
        assert!(check_answer(ANSWER, &allowed(&["40.0.0.0/8"])).is_err());
        assert!(check_answer("v=0\r\n", &allowed(&["52.112.0.0/14"])).is_err());
    }

    #[tokio::test]
    async fn a_module_without_media_access_cannot_open() {
        let (events, _queue) = mpsc::channel(1);
        let result = Connections::default().open(None, "main-channel", &events).await;
        assert!(matches!(result, Err(HttpError::Blocked(_))), "{result:?}");
    }

    #[test]
    fn pending_messages_keep_to_their_limits() {
        let mut pending = Pending::default();
        pending.push(vec![0; limits::MAX_RTC_MESSAGE + 1]);
        for _ in 0..limits::MAX_RTC_MESSAGES_PER_SECOND + 5 {
            pending.push(vec![1; 10]);
        }
        let Some(GuestEvent::RtcData { messages, dropped, .. }) = pending.take(7) else { panic!("a batch") };
        assert_eq!(messages.len(), limits::MAX_RTC_MESSAGES_PER_SECOND);
        assert_eq!(dropped, 6);
        assert!(pending.take(7).is_none());
    }
}
