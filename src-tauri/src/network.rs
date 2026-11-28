//! The same authenticated peer connection carries lobby/signaling streams and
//! encrypted WebRTC UDP packets, whether iroh chooses a direct path or relay.
use crate::diagnostics::{Diagnostics, Event, Role, RoomDiagnostics, RoomPhase};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bytes::Bytes;
use futures_util::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use iroh::{
    endpoint::{presets, Builder, Connection, PathEvent, SendDatagramError},
    Endpoint, EndpointAddr, SecretKey, TransportAddr,
};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{timeout, timeout_at, Instant},
};

const ALPN: &[u8] = b"duipin/1";
const PREFIX: &str = "duipin://join/";
const MAX_PACKET: usize = 65509;
const MAX_FLOWS: usize = 64;
const FLOW_IDLE: Duration = Duration::from_secs(90);
const MAX_QUEUED_MEDIA_STREAMS: usize = 8;
const MEDIA_STREAM_SEND_TIMEOUT: Duration = Duration::from_secs(3);
const TRANSPORT_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Clone, serde::Serialize)]
#[serde(tag = "transport_event", rename_all = "snake_case")]
pub(crate) enum TransportEvent {
    Sample {
        link_id: u64,
        role: Role,
        phase: TransportSamplePhase,
        path: &'static str,
        rtt_ms: Option<f64>,
        quic_packets_sent: u64,
        quic_packets_lost: u64,
        quic_udp_payload_bytes_sent: u64,
        quic_udp_payload_bytes_received: u64,
        quic_datagram_frames_sent: u64,
        quic_datagram_frames_received: u64,
        quic_datagram_send_buffer_free_bytes: usize,
        forwarded_udp_quic_datagram_admitted_packets: u64,
        forwarded_udp_quic_datagram_admitted_bytes: u64,
        forwarded_udp_stream_queue_admitted_packets: u64,
        forwarded_udp_stream_queue_admitted_bytes: u64,
        forwarded_udp_receive_admitted_packets: u64,
        forwarded_udp_receive_admitted_bytes: u64,
        media_stream_fallbacks: u64,
        media_stream_queue_drops: u64,
        media_stream_queue_expirations: u64,
        media_stream_open_timeouts: u64,
        media_stream_write_timeouts: u64,
        media_stream_send_errors: u64,
        media_streams_finished: u64,
    },
    SelectedPathChanged {
        link_id: u64,
        role: Role,
        path: &'static str,
        rtt_ms: Option<f64>,
    },
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransportSamplePhase {
    Initial,
    Periodic,
    Final,
}

#[derive(Default)]
struct UdpPayloadCounters {
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl UdpPayloadCounters {
    fn record(&self, payload_bytes: usize) {
        self.bytes
            .fetch_add(payload_bytes as u64, Ordering::Relaxed);
        self.packets.fetch_add(1, Ordering::Relaxed);
    }

    fn sample(&self) -> (u64, u64) {
        (
            self.packets.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

#[derive(Default)]
struct MediaCounters {
    udp_datagram: UdpPayloadCounters,
    udp_stream_queue: UdpPayloadCounters,
    udp_receive: UdpPayloadCounters,
    queue_drops: AtomicU64,
    queue_expirations: AtomicU64,
    open_timeouts: AtomicU64,
    write_timeouts: AtomicU64,
    send_errors: AtomicU64,
    finished: AtomicU64,
}

fn transport_label(address: &TransportAddr) -> &'static str {
    if address.is_relay() {
        "relay"
    } else if address.is_ip() {
        "direct"
    } else {
        "unknown"
    }
}

fn selected_path(conn: &Connection) -> Option<(&'static str, Duration)> {
    conn.paths()
        .iter()
        .find(|path| path.is_selected())
        .map(|path| (transport_label(path.remote_addr()), path.rtt()))
}

fn observe_transport(
    conn: &Connection,
    role: Role,
    diagnostics: Diagnostics,
    media: Arc<MediaCounters>,
) {
    let link_id = diagnostics.next_id();
    let mut paths = conn.path_events();
    let conn = conn.clone();
    tokio::spawn(async move {
        let sample = |phase| {
            let (path, rtt_ms) = selected_path(&conn)
                .map(|(path, rtt)| (path, Some(rtt.as_secs_f64() * 1000.0)))
                .unwrap_or(("unknown", None));
            let quic = conn.stats();
            let (datagram_packets, datagram_bytes) = media.udp_datagram.sample();
            let (stream_queue_packets, stream_queue_bytes) = media.udp_stream_queue.sample();
            let (receive_packets, receive_bytes) = media.udp_receive.sample();
            diagnostics.record(Event::Transport(TransportEvent::Sample {
                link_id,
                role,
                phase,
                path,
                rtt_ms,
                quic_packets_sent: quic.sent_packets,
                quic_packets_lost: quic.lost_packets,
                quic_udp_payload_bytes_sent: quic.udp_tx.bytes,
                quic_udp_payload_bytes_received: quic.udp_rx.bytes,
                quic_datagram_frames_sent: quic.frame_tx.datagram,
                quic_datagram_frames_received: quic.frame_rx.datagram,
                quic_datagram_send_buffer_free_bytes: conn.datagram_send_buffer_space(),
                forwarded_udp_quic_datagram_admitted_packets: datagram_packets,
                forwarded_udp_quic_datagram_admitted_bytes: datagram_bytes,
                forwarded_udp_stream_queue_admitted_packets: stream_queue_packets,
                forwarded_udp_stream_queue_admitted_bytes: stream_queue_bytes,
                forwarded_udp_receive_admitted_packets: receive_packets,
                forwarded_udp_receive_admitted_bytes: receive_bytes,
                media_stream_fallbacks: stream_queue_packets,
                media_stream_queue_drops: media.queue_drops.load(Ordering::Relaxed),
                media_stream_queue_expirations: media.queue_expirations.load(Ordering::Relaxed),
                media_stream_open_timeouts: media.open_timeouts.load(Ordering::Relaxed),
                media_stream_write_timeouts: media.write_timeouts.load(Ordering::Relaxed),
                media_stream_send_errors: media.send_errors.load(Ordering::Relaxed),
                media_streams_finished: media.finished.load(Ordering::Relaxed),
            }));
        };
        sample(TransportSamplePhase::Initial);
        let mut interval = tokio::time::interval_at(
            Instant::now() + TRANSPORT_SAMPLE_INTERVAL,
            TRANSPORT_SAMPLE_INTERVAL,
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let closed = conn.closed();
        tokio::pin!(closed);
        loop {
            tokio::select! {
                biased;
                _ = &mut closed => break,
                _ = interval.tick() => sample(TransportSamplePhase::Periodic),
                Some(event) = paths.next() => {
                    if let PathEvent::Selected { id, remote_addr, .. } = event {
                        diagnostics.record(Event::Transport(TransportEvent::SelectedPathChanged {
                            link_id,
                            role,
                            path: transport_label(&remote_addr),
                            rtt_ms: conn.rtt(id).map(|rtt| rtt.as_secs_f64() * 1000.0),
                        }));
                    }
                }
            }
        }
        sample(TransportSamplePhase::Final);
    });
}

pub(crate) fn local_ip() -> Ipv4Addr {
    let route = || {
        let sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
        // UDP connect asks Windows for its chosen interface without sending a packet.
        sock.connect("192.0.2.1:9").ok()?;
        match sock.local_addr().ok()?.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            _ => None,
        }
    };
    route().unwrap_or(Ipv4Addr::LOCALHOST)
}

fn force_relay() -> bool {
    std::env::var("DUIPIN_FORCE_RELAY").as_deref() == Ok("1")
}

#[cfg(test)]
tokio::task_local! {pub(crate) static TEST_RELAY: iroh::RelayMap;}

fn endpoint_builder() -> Builder {
    let builder = Endpoint::builder(presets::N0);
    #[cfg(test)]
    let builder = match TEST_RELAY.try_with(Clone::clone) {
        Ok(map) => {
            return builder
                .clear_ip_transports()
                .clear_address_lookup()
                .relay_mode(iroh::RelayMode::Custom(map))
        }
        Err(_) => builder,
    };
    if force_relay() {
        builder.clear_ip_transports()
    } else {
        builder
    }
}

// ICE consent keeps an active UDP flow fresh. Remove the least recently active
// source at capacity so media reconnections never exhaust a guest session.
struct Flows<T>(HashMap<u16, (Instant, T)>);
impl<T> Flows<T> {
    fn new() -> Self {
        Self(HashMap::new())
    }
    fn expire(&mut self) {
        self.0.retain(|_, (last, _)| last.elapsed() < FLOW_IDLE);
    }
    fn get(&mut self, id: u16) -> Option<&T> {
        let (last, value) = self.0.get_mut(&id)?;
        *last = Instant::now();
        Some(value)
    }
    fn insert(&mut self, id: u16, value: T) {
        if self.0.len() >= MAX_FLOWS && !self.0.contains_key(&id) {
            if let Some(old) = self
                .0
                .iter()
                .min_by_key(|(_, (last, _))| *last)
                .map(|(&id, _)| id)
            {
                self.0.remove(&old);
            }
        }
        self.0.insert(id, (Instant::now(), value));
    }
}

struct HostFlow {
    socket: Arc<UdpSocket>,
    receiver: JoinHandle<()>,
}
impl Drop for HostFlow {
    fn drop(&mut self) {
        self.receiver.abort();
    }
}

pub struct Ticket {
    pub address: EndpointAddr,
    pub key: String,
}
impl Ticket {
    pub fn parse(invite: &str) -> Result<Self> {
        let invite = invite.trim();
        let web_url;
        let invite = if invite.starts_with("https://") {
            web_url = url::Url::parse(invite).map_err(err)?;
            web_url.fragment().ok_or("链接缺少房间邀请")?
        } else {
            invite
        };
        let parts = invite
            .trim()
            .strip_prefix(PREFIX)
            .and_then(|s| s.split_once('#'))
            .ok_or("请粘贴完整的友声邀请链接")?;
        if parts.0.len() > 16000
            || parts.1.len() != 64
            || !parts.1.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("邀请格式无效".into());
        }
        let address =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts.0).map_err(err)?).map_err(err)?;
        Ok(Self {
            address,
            key: parts.1.into(),
        })
    }
}

pub struct HostEndpoint {
    ep: Endpoint,
    accept: JoinHandle<()>,
}
impl HostEndpoint {
    pub async fn start(
        seed: [u8; 32],
        key: String,
        gateway: u16,
        media: SocketAddrV4,
        diagnostics: Diagnostics,
    ) -> Result<Self> {
        let ep = endpoint_builder()
            .secret_key(SecretKey::from_bytes(&seed))
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .map_err(err)?;
        let endpoint = ep.clone();
        let accept = tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let key = key.clone();
                let diagnostics = diagnostics.clone();
                tokio::spawn(async move {
                    let Ok(Ok(conn)) = timeout(Duration::from_secs(12), incoming).await else {
                        return;
                    };
                    if run_host(&conn, &key, gateway, media, diagnostics)
                        .await
                        .is_err()
                    {
                        conn.close(1u8.into(), b"end")
                    }
                });
            }
        });
        Ok(Self { ep, accept })
    }
    #[cfg(test)]
    pub fn id(&self) -> String {
        self.ep.id().to_string()
    }
    pub fn invite(&self, key: &str) -> Result<String> {
        Self::encode_invite(self.ep.addr(), key)
    }
    // Only sharing an invitation needs internet relay discovery. A host can
    // accept local players as soon as its endpoint and the voice server bind.
    pub fn share_invite(
        &self,
        key: String,
    ) -> impl std::future::Future<Output = Result<String>> + 'static {
        let ep = self.ep.clone();
        async move {
            if force_relay() {
                timeout(Duration::from_secs(12), ep.online())
                    .await
                    .map_err(|_| "强制中转模式无法连接公共中转服务".to_string())?;
            } else {
                let _ = timeout(Duration::from_secs(2), ep.online()).await;
            }
            Self::encode_invite(ep.addr(), &key)
        }
    }
    fn encode_invite(address: EndpointAddr, key: &str) -> Result<String> {
        let direct = format!(
            "{PREFIX}{}#{key}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&address).map_err(err)?)
        );
        if let Some(base) = option_env!("DUIPIN_INVITE_BASE").filter(|v| !v.is_empty()) {
            let mut url = url::Url::parse(base).map_err(err)?;
            if url.scheme() != "https" {
                return Err("邀请页需要 HTTPS 地址".into());
            }
            url.set_fragment(Some(&direct));
            Ok(url.to_string())
        } else {
            Ok(direct)
        }
    }
    pub async fn close(&self) {
        self.accept.abort();
        self.ep.close().await;
    }
}
impl Drop for HostEndpoint {
    fn drop(&mut self) {
        self.accept.abort();
        if self.ep.is_closed() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let endpoint = self.ep.clone();
            runtime.spawn(async move {
                endpoint.close().await;
            });
        }
    }
}

async fn run_host(
    conn: &Connection,
    key: &str,
    gateway: u16,
    media: SocketAddrV4,
    diagnostics: Diagnostics,
) -> Result<()> {
    #[cfg(test)]
    let peer = conn.remote_id();
    timeout(Duration::from_secs(8), async {
        let (mut tx, mut rx) = conn.accept_bi().await.map_err(err)?;
        let mut supplied = [0u8; 64];
        rx.read_exact(&mut supplied).await.map_err(err)?;
        if !bool::from(supplied.as_slice().ct_eq(key.as_bytes())) {
            return Err("邀请无效".into());
        }
        tx.write_all(b"OK").await.map_err(err)?;
        tx.finish().map_err(err)
    })
    .await
    .map_err(err)??;
    let media_counters = Arc::new(MediaCounters::default());
    observe_transport(conn, Role::Host, diagnostics, media_counters.clone());
    let bridge = conn.clone();
    let streams = tokio::spawn(async move {
        while let Ok((tx, rx)) = bridge.accept_bi().await {
            tokio::spawn(async move {
                if let Ok(tcp) = TcpStream::connect((Ipv4Addr::LOCALHOST, gateway)).await {
                    pipe(tcp, tx, rx).await;
                }
            });
        }
    });
    let outgoing = Arc::new(OutgoingPackets::new(conn, media_counters.clone()));
    let mut flows = Flows::<HostFlow>::new();
    let result = async {
        let mut expire = tokio::time::interval(FLOW_IDLE);
        let mut packets = IncomingPackets::default();
        loop {
            let packet = tokio::select! {
                packet=packets.receive(conn) => packet?,
                _=expire.tick()=>{flows.expire();continue;},
            };
            if packet.len() < 3 {
                continue;
            }
            let id = u16::from_be_bytes([packet[0], packet[1]]);
            let socket = if let Some(flow) = flows.get(id) {
                flow.socket.clone()
            } else {
                let socket = Arc::new(
                    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                        .await
                        .map_err(err)?,
                );
                socket.connect(media).await.map_err(err)?;
                let receiving = socket.clone();
                let outgoing = outgoing.clone();
                let receiver = tokio::spawn(async move {
                    let mut buf = vec![0; MAX_PACKET];
                    loop {
                        tokio::select! {
                            _=outgoing.conn.closed()=>break,
                            received=receiving.recv(&mut buf)=>{
                                let Ok(n)=received else{break};
                                #[cfg(test)]
                                crate::scenario_tests::rtp_trace::packet("sfu-out", peer, id, &buf[..n]);
                                if outgoing.send(id,&buf[..n]).is_err(){break}
                            }
                        }
                    }
                });
                flows.insert(
                    id,
                    HostFlow {
                        socket: socket.clone(),
                        receiver,
                    },
                );
                socket
            };
            #[cfg(test)]
            crate::scenario_tests::rtp_trace::packet("sfu-in", peer, id, &packet[2..]);
            media_counters.udp_receive.record(packet.len() - 2);
            socket.send(&packet[2..]).await.map_err(err)?;
        }
    }
    .await;
    streams.abort();
    result
}

async fn pipe(tcp: TcpStream, tx: iroh::endpoint::SendStream, rx: iroh::endpoint::RecvStream) {
    let mut duplex = tokio::io::join(rx, tx);
    let mut tcp = tcp;
    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut duplex).await;
}

struct OutgoingPackets {
    conn: Connection,
    large: mpsc::Sender<(Instant, Bytes)>,
    _stop: oneshot::Sender<()>,
    counters: Arc<MediaCounters>,
}

impl OutgoingPackets {
    fn new(conn: &Connection, counters: Arc<MediaCounters>) -> Self {
        let (large, queue) = mpsc::channel(MAX_QUEUED_MEDIA_STREAMS);
        let (stop, stopped) = oneshot::channel();
        tokio::spawn(Self::send_large(
            conn.clone(),
            queue,
            stopped,
            counters.clone(),
        ));
        Self {
            conn: conn.clone(),
            large,
            _stop: stop,
            counters,
        }
    }

    fn send(&self, id: u16, payload: &[u8]) -> Result<()> {
        let id = id.to_be_bytes();
        let frame = || Bytes::from([id.as_slice(), payload].concat());
        if self
            .conn
            .max_datagram_size()
            .is_some_and(|size| id.len() + payload.len() <= size)
        {
            match self.conn.send_datagram(frame()) {
                Ok(()) => {
                    self.counters.udp_datagram.record(payload.len());
                    return Ok(());
                }
                Err(SendDatagramError::ConnectionLost(error)) => return Err(err(error)),
                Err(_) => {}
            }
        }
        if let Ok(slot) = self.large.try_reserve() {
            self.counters.udp_stream_queue.record(payload.len());
            slot.send((Instant::now() + MEDIA_STREAM_SEND_TIMEOUT, frame()));
        } else {
            self.counters.queue_drops.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn send_large(
        conn: Connection,
        mut queue: mpsc::Receiver<(Instant, Bytes)>,
        mut stopped: oneshot::Receiver<()>,
        counters: Arc<MediaCounters>,
    ) {
        loop {
            let next = tokio::select! {
                biased;
                _ = &mut stopped => return,
                _ = conn.closed() => return,
                packet = queue.recv() => packet,
            };
            let Some((deadline, frame)) = next else {
                return;
            };
            if deadline <= Instant::now() {
                counters.queue_expirations.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let mut stream = tokio::select! {
                biased;
                _ = &mut stopped => return,
                opened = timeout_at(deadline, conn.open_uni()) => match opened {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(_)) => {
                        counters.send_errors.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    Err(_) => {
                        counters.open_timeouts.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                },
            };
            let written = tokio::select! {
                biased;
                _ = &mut stopped => {
                    let _ = stream.reset(1u8.into());
                    return;
                }
                written = timeout_at(deadline, stream.write_chunk(frame)) => written,
            };
            match written {
                Ok(Ok(())) => {
                    // Finishing locally does not wait for peer receipt.
                    let counter = if stream.finish().is_ok() {
                        &counters.finished
                    } else {
                        &counters.send_errors
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Ok(Err(_)) => counters.send_errors.fetch_add(1, Ordering::Relaxed),
                Err(_) => counters.write_timeouts.fetch_add(1, Ordering::Relaxed),
            };
            let _ = stream.reset(1u8.into());
        }
    }
}

#[derive(Default)]
struct IncomingPackets {
    large: FuturesUnordered<BoxFuture<'static, Option<Bytes>>>,
}

impl IncomingPackets {
    async fn receive(&mut self, conn: &Connection) -> Result<Bytes> {
        // Keep partial DTLS/oversize packets across polls without delaying live audio.
        // Eight bounded reads use at most about 512 KiB of payload per peer.
        const MAX_PACKET_READS: usize = 8;
        loop {
            tokio::select! {
                frame = conn.read_datagram() => return frame.map_err(err),
                stream = conn.accept_uni(), if self.large.len() < MAX_PACKET_READS => {
                    let mut stream = stream.map_err(err)?;
                    self.large.push(async move {
                        timeout(Duration::from_secs(3), stream.read_to_end(MAX_PACKET + 2))
                            .await.ok()?.ok().map(Bytes::from)
                    }.boxed());
                }
                Some(frame) = self.large.next(), if !self.large.is_empty() => {
                    if let Some(frame) = frame {
                        return Ok(frame);
                    }
                }
            }
        }
    }
}

pub struct GuestEndpoint {
    ep: Endpoint,
    conn: Connection,
    tasks: Vec<JoinHandle<()>>,
    pub gateway: u16,
    pub media: SocketAddrV4,
}
impl GuestEndpoint {
    pub async fn start(ticket: &Ticket, diagnostics: &RoomDiagnostics) -> Result<Self> {
        let ep = endpoint_builder().bind().await.map_err(err)?;
        if force_relay() {
            timeout(Duration::from_secs(12), ep.online())
                .await
                .map_err(|_| "本机无法连接中转服务".to_string())?;
        }
        let conn = diagnostics
            .measure(RoomPhase::DialPeer, async {
                timeout(
                    Duration::from_secs(20),
                    ep.connect(ticket.address.clone(), ALPN),
                )
                .await
                .map_err(|_| "房间暂时离线，或此网络无法连接中转".to_string())?
                .map_err(|e| format!("无法建立到房间的加密连接：{e}"))
            })
            .await?;
        diagnostics
            .measure(RoomPhase::AuthenticateInvite, async {
                let (mut tx, mut rx) = conn
                    .open_bi()
                    .await
                    .map_err(|e| format!("无法打开房间认证连接：{e}"))?;
                tx.write_all(ticket.key.as_bytes()).await.map_err(err)?;
                tx.finish().map_err(err)?;
                let mut ack = [0; 2];
                timeout(Duration::from_secs(6), rx.read_exact(&mut ack))
                    .await
                    .map_err(|_| "房间邀请认证响应超时".to_string())?
                    .map_err(|e| format!("房间未接受邀请：{e}"))?;
                if &ack != b"OK" {
                    return Err("邀请被拒绝".to_owned());
                }
                Ok(())
            })
            .await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(err)?;
        let gateway = listener.local_addr().map_err(err)?.port();
        let ip = local_ip();
        let udp = UdpSocket::bind((ip, 0)).await.map_err(err)?;
        let media = SocketAddrV4::new(ip, udp.local_addr().map_err(err)?.port());
        let media_counters = Arc::new(MediaCounters::default());
        observe_transport(
            &conn,
            Role::Guest,
            diagnostics.handle(),
            media_counters.clone(),
        );
        let stream_conn = conn.clone();
        let streams = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=stream_conn.closed()=>break,
                    next=listener.accept()=>{
                        let Ok((tcp,_))=next else{break}; let stream_conn=stream_conn.clone();
                        tokio::spawn(async move {if let Ok((tx,rx))=stream_conn.open_bi().await {pipe(tcp,tx,rx).await;}});
                    }
                }
            }
        });
        let media_conn = conn.clone();
        #[cfg(test)]
        let peer = ep.id();
        let media_task = tokio::spawn(async move {
            let outgoing = OutgoingPackets::new(&media_conn, media_counters.clone());
            let mut senders = Flows::new();
            let mut buf = vec![0; MAX_PACKET];
            let mut packets = IncomingPackets::default();
            let mut expire = tokio::time::interval(FLOW_IDLE);
            loop {
                tokio::select! {
                    _=expire.tick()=>senders.expire(),
                    received=udp.recv_from(&mut buf)=>{
                        let (n, source) = match received {
                            Ok(packet) => packet,
                            // A late packet to a closed WebRTC port can report a peer
                            // error on this shared socket's next receive (WSAECONNRESET).
                            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused) => continue,
                            Err(_) => break,
                        };
                        if !matches!(source,SocketAddr::V4(v) if *v.ip()==ip || v.ip().is_loopback()) {continue}
                        senders.insert(source.port(),source);
                        #[cfg(test)]
                        crate::scenario_tests::rtp_trace::packet("guest-out", peer, source.port(), &buf[..n]);
                        if outgoing.send(source.port(),&buf[..n]).is_err(){break}
                    }
                    received=packets.receive(&media_conn)=>{
                        let Ok(frame)=received else{break};
                        if frame.len()<3 {continue}
                        if let Some(addr)=senders.get(u16::from_be_bytes([frame[0],frame[1]])) {
                            #[cfg(test)]
                            crate::scenario_tests::rtp_trace::packet("guest-in", peer, addr.port(), &frame[2..]);
                            media_counters.udp_receive.record(frame.len() - 2);
                            let _=udp.send_to(&frame[2..],addr).await;
                        }
                    }
                }
            }
        });
        Ok(Self {
            ep,
            conn,
            tasks: vec![streams, media_task],
            gateway,
            media,
        })
    }
    pub async fn close(self) {
        self.conn.close(0u8.into(), b"leave");
        for task in &self.tasks {
            task.abort()
        }
        self.ep.close().await;
    }

    pub(crate) fn host_id(&self) -> iroh::EndpointId {
        self.conn.remote_id()
    }

    #[cfg(test)]
    pub(crate) fn selected_transport(&self) -> Option<&'static str> {
        self.selected_path().map(|(transport, _)| transport)
    }

    pub(crate) fn selected_path(&self) -> Option<(&'static str, Duration)> {
        selected_path(&self.conn)
    }
}

impl Drop for GuestEndpoint {
    fn drop(&mut self) {
        self.conn.close(0u8.into(), b"leave");
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::endpoint::QuicTransportConfig;

    fn local_endpoint() -> Builder {
        Endpoint::builder(presets::N0)
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_address_lookup()
            .alpns(vec![ALPN.to_vec()])
    }

    async fn connect_local(host: &Endpoint, guest: &Endpoint) -> (Connection, Connection) {
        let port = host
            .bound_sockets()
            .iter()
            .find(|s| s.is_ipv4())
            .unwrap()
            .port();
        let address = EndpointAddr::new(host.id())
            .with_ip_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        let (sender, receiver) = timeout(Duration::from_secs(5), async {
            tokio::join!(guest.connect(address, ALPN), async {
                host.accept().await.unwrap().await
            })
        })
        .await
        .unwrap();
        (sender.unwrap(), receiver.unwrap())
    }

    #[tokio::test]
    async fn media_stream_counters_distinguish_fallbacks_and_full_queue_drops() {
        let host = local_endpoint().bind().await.unwrap();
        let guest = local_endpoint().bind().await.unwrap();
        let (guest_conn, host_conn) = connect_local(&host, &guest).await;
        let counters = Arc::new(MediaCounters::default());
        let outgoing = OutgoingPackets::new(&host_conn, counters.clone());
        let payload = vec![7; 16_000];

        outgoing.send(0x1234, b"live audio").unwrap();
        for _ in 0..=MAX_QUEUED_MEDIA_STREAMS {
            outgoing.send(0x1234, &payload).unwrap();
        }

        assert_eq!(
            counters.udp_stream_queue.sample(),
            (
                MAX_QUEUED_MEDIA_STREAMS as u64,
                (MAX_QUEUED_MEDIA_STREAMS * payload.len()) as u64
            ),
        );
        assert_eq!(counters.queue_drops.load(Ordering::Relaxed), 1);
        assert_eq!(
            timeout(Duration::from_secs(2), guest_conn.read_datagram())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\x12\x34live audio")
        );
        let frame = timeout(Duration::from_secs(2), async {
            guest_conn
                .accept_uni()
                .await
                .unwrap()
                .read_to_end(MAX_PACKET + 2)
                .await
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(&frame[2..], &payload);
        assert!(counters.finished.load(Ordering::Relaxed) > 0);
    }

    #[tokio::test]
    async fn media_stream_counters_distinguish_queue_expiry_and_stream_open_timeout() {
        let host = local_endpoint().bind().await.unwrap();
        let guest = local_endpoint()
            .transport_config(
                QuicTransportConfig::builder()
                    .max_concurrent_uni_streams(0u8.into())
                    .build(),
            )
            .bind()
            .await
            .unwrap();
        let (_guest_conn, host_conn) = connect_local(&host, &guest).await;
        let counters = Arc::new(MediaCounters::default());
        let (outbound, queue) = mpsc::channel(2);
        let (_stop, stopped) = oneshot::channel();
        outbound.try_send((Instant::now(), Bytes::new())).unwrap();
        outbound
            .try_send((Instant::now() + Duration::from_millis(500), Bytes::new()))
            .unwrap();
        drop(outbound);

        OutgoingPackets::send_large(host_conn, queue, stopped, counters.clone()).await;

        assert_eq!(counters.queue_expirations.load(Ordering::Relaxed), 1);
        assert_eq!(counters.open_timeouts.load(Ordering::Relaxed), 1);
        assert_eq!(counters.write_timeouts.load(Ordering::Relaxed), 0);
        assert_eq!(counters.finished.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_datagram_overtakes_media_waiting_for_send_stream_credit() {
        let host = local_endpoint().bind().await.unwrap();
        let guest = local_endpoint()
            .transport_config(
                QuicTransportConfig::builder()
                    .max_concurrent_uni_streams(0u8.into())
                    .build(),
            )
            .bind()
            .await
            .unwrap();
        let (guest_conn, host_conn) = connect_local(&host, &guest).await;
        assert!(futures_util::poll!(Box::pin(host_conn.open_uni())).is_pending());
        let large = vec![7; 16_000];
        assert!(large.len() > host_conn.max_datagram_size().unwrap());

        let media = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let SocketAddr::V4(media_addr) = media.local_addr().unwrap() else {
            unreachable!();
        };
        let key = "a".repeat(64);
        let diagnostics = Diagnostics::default();
        let host_diagnostics = diagnostics.clone();
        let host_task = tokio::spawn(async move {
            run_host(&host_conn, &key, 0, media_addr, host_diagnostics).await
        });
        let (mut auth, mut ack) = guest_conn.open_bi().await.unwrap();
        auth.write_all(&[b'a'; 64]).await.unwrap();
        auth.finish().unwrap();
        assert_eq!(
            timeout(Duration::from_secs(5), ack.read_to_end(2))
                .await
                .unwrap()
                .unwrap(),
            b"OK"
        );

        guest_conn
            .send_datagram(Bytes::from_static(b"\x12\x34probe"))
            .unwrap();
        let mut probe = [0; 16];
        let (size, bridge_addr) = timeout(Duration::from_secs(5), media.recv_from(&mut probe))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&probe[..size], b"probe");
        media.send_to(&large, bridge_addr).await.unwrap();
        media.send_to(b"live audio", bridge_addr).await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), guest_conn.read_datagram())
                .await
                .expect("live audio stalled waiting for stream credit")
                .unwrap(),
            Bytes::from_static(b"\x12\x34live audio")
        );

        guest_conn.set_max_concurrent_uni_streams(1u8.into());
        let frame = timeout(Duration::from_secs(2), async {
            guest_conn
                .accept_uni()
                .await
                .unwrap()
                .read_to_end(MAX_PACKET + 2)
                .await
                .unwrap()
        })
        .await
        .expect("large packet was not sent after stream credit became available");
        assert_eq!(&frame[..2], b"\x12\x34");
        assert_eq!(&frame[2..], &large);
        guest_conn.close(0u8.into(), b"done");
        timeout(Duration::from_secs(2), host_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();

        let directory = tempfile::tempdir().unwrap();
        let sample = timeout(Duration::from_secs(2), async {
            loop {
                let report: serde_json::Value = tokio::task::block_in_place(|| {
                    let path = diagnostics.export(directory.path()).unwrap();
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
                });
                let sample = report["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find_map(|event| {
                        let data = &event["data"];
                        (data["transport_event"] == "sample" && data["phase"] == "final")
                            .then_some(data)
                    });
                if let Some(sample) = sample {
                    break sample.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("transport observer did not record a final sample");
        assert_eq!(sample["forwarded_udp_quic_datagram_admitted_packets"], 1);
        assert_eq!(
            sample["forwarded_udp_quic_datagram_admitted_bytes"],
            b"live audio".len()
        );
        assert_eq!(sample["forwarded_udp_stream_queue_admitted_packets"], 1);
        assert_eq!(
            sample["forwarded_udp_stream_queue_admitted_bytes"],
            large.len()
        );
        assert_eq!(sample["media_stream_fallbacks"], 1);
        assert_eq!(sample["forwarded_udp_receive_admitted_packets"], 1);
        assert_eq!(
            sample["forwarded_udp_receive_admitted_bytes"],
            b"probe".len()
        );
        assert!(sample["quic_udp_payload_bytes_sent"].as_u64().unwrap() > large.len() as u64);
        assert!(
            sample["quic_udp_payload_bytes_received"].as_u64().unwrap() > b"probe".len() as u64
        );
        assert!(sample["quic_datagram_frames_sent"].as_u64().unwrap() >= 1);
        assert!(sample["quic_datagram_frames_received"].as_u64().unwrap() >= 1);
        assert!(sample["quic_datagram_send_buffer_free_bytes"].is_u64());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incomplete_media_stream_is_reset_after_send_timeout() {
        let host = local_endpoint().bind().await.unwrap();
        let guest = local_endpoint()
            .transport_config(
                QuicTransportConfig::builder()
                    .stream_receive_window(64u8.into())
                    .build(),
            )
            .bind()
            .await
            .unwrap();
        let (guest_conn, host_conn) = connect_local(&host, &guest).await;
        let counters = Arc::new(MediaCounters::default());
        let outgoing = OutgoingPackets::new(&host_conn, counters.clone());
        let payload = vec![7; 16_000];
        outgoing.send(0x1234, &payload).unwrap();

        let mut stream = timeout(Duration::from_secs(2), guest_conn.accept_uni())
            .await
            .expect("oversized media did not start writing")
            .unwrap();
        let reset = timeout(Duration::from_secs(5), stream.received_reset())
            .await
            .expect("oversized media stream remained open after the send deadline")
            .unwrap();
        assert!(
            reset.is_some(),
            "partial media was finished without a reset"
        );
        assert_eq!(counters.write_timeouts.load(Ordering::Relaxed), 1);
        assert_eq!(counters.finished.load(Ordering::Relaxed), 0);

        outgoing.send(0x1234, b"live audio").unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), guest_conn.read_datagram())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\x12\x34live audio")
        );
        guest_conn.close(0u8.into(), b"done");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_datagram_overtakes_partial_media_stream() {
        let host = local_endpoint().bind().await.unwrap();
        let guest = local_endpoint().bind().await.unwrap();
        let (sender, receiver) = connect_local(&host, &guest).await;
        let initial_stream_frames = receiver.stats().frame_rx.stream;
        let mut slow = sender.open_uni().await.unwrap();
        let payload = vec![7; 1600];
        slow.write_all(&payload[..20]).await.unwrap();
        // Only the prefix of the fallback packet is available when the receiver polls.
        timeout(Duration::from_secs(2), async {
            while receiver.stats().frame_rx.stream == initial_stream_frames {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut packets = IncomingPackets::default();
        let mut next = Box::pin(packets.receive(&receiver));
        assert!(futures_util::poll!(&mut next).is_pending());
        let audio = Bytes::from_static(b"fresh audio");
        sender.send_datagram(audio.clone()).unwrap();
        let started = Instant::now();
        let first = timeout(Duration::from_millis(500), next).await;
        assert!(
            first.is_ok(),
            "audio remained blocked for {:?} on a partial stream",
            started.elapsed()
        );
        assert_eq!(first.unwrap().unwrap(), audio);
        println!(
            "audio delivered in {:?} with fallback stream still unfinished",
            started.elapsed()
        );
        slow.write_all(&payload[20..]).await.unwrap();
        slow.finish().unwrap();
        assert_eq!(
            &timeout(Duration::from_secs(2), packets.receive(&receiver))
                .await
                .unwrap()
                .unwrap()[..],
            &payload[..]
        );
        sender.close(0u8.into(), b"done");
    }

    #[test]
    fn dormant_media_flows_expire_and_new_connections_replace_old_ports() {
        let mut flows = Flows::new();
        for id in 0..MAX_FLOWS as u16 {
            flows.insert(id, id);
        }
        flows.0.get_mut(&4).unwrap().0 = Instant::now() - Duration::from_secs(20);
        flows.insert(65, 65);
        assert!(flows.get(4).is_none());
        assert_eq!(flows.get(65), Some(&65));
        flows.0.get_mut(&65).unwrap().0 = Instant::now() - FLOW_IDLE;
        flows.expire();
        assert!(flows.get(65).is_none());
        assert_eq!(flows.0.len(), MAX_FLOWS - 1);
    }
}
