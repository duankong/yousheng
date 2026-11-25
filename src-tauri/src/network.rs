//! The same authenticated peer connection carries lobby/signaling streams and
//! encrypted WebRTC UDP packets, whether iroh chooses a direct path or relay.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bytes::Bytes;
use iroh::{
    endpoint::{presets, Builder, Connection},
    Endpoint, EndpointAddr, SecretKey,
};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinHandle,
    time::{timeout, Instant},
};

const ALPN: &[u8] = b"duipin/1";
const PREFIX: &str = "duipin://join/";
const MAX_PACKET: usize = 65509;
const MAX_FLOWS: usize = 64;
const FLOW_IDLE: Duration = Duration::from_secs(90);
type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
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
// source at capacity so many channel switches never exhaust a guest session.
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
            web_url.fragment().ok_or("链接缺少大厅邀请")?
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
                tokio::spawn(async move {
                    let Ok(Ok(conn)) = timeout(Duration::from_secs(12), incoming).await else {
                        return;
                    };
                    if run_host(&conn, &key, gateway, media).await.is_err() {
                        conn.close(1u8.into(), b"end")
                    }
                });
            }
        });
        Ok(Self { ep, accept })
    }
    pub fn id(&self) -> String {
        self.ep.id().to_string()
    }
    pub fn invite(&self, key: &str) -> Result<String> {
        Self::encode_invite(self.ep.addr(), key)
    }
    // Only sharing an invitation needs internet relay discovery. A host can
    // enter a local channel as soon as its endpoint and the voice server bind.
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

async fn run_host(conn: &Connection, key: &str, gateway: u16, media: SocketAddrV4) -> Result<()> {
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
    let mut flows = Flows::<HostFlow>::new();
    let result = async {
        let mut expire = tokio::time::interval(FLOW_IDLE);
        let mut next = Box::pin(receive_packet(conn));
        loop {
            let packet = tokio::select! {
                packet=&mut next => packet?,
                _=expire.tick()=>{flows.expire();continue;},
            };
            next.set(receive_packet(conn));
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
                let channel = conn.clone();
                let receiver = tokio::spawn(async move {
                    let mut buf = vec![0; MAX_PACKET];
                    loop {
                        tokio::select! {
                            _=channel.closed()=>break,
                            received=receiving.recv(&mut buf)=>{
                                let Ok(n)=received else{break};
                                if send_packet(&channel,id,&buf[..n]).await.is_err(){break}
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

async fn send_packet(conn: &Connection, id: u16, payload: &[u8]) -> Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 2);
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(payload);
    if conn
        .max_datagram_size()
        .is_some_and(|size| frame.len() <= size)
    {
        conn.send_datagram(Bytes::from(frame)).map_err(err)
    } else {
        // DTLS handshakes can exceed the negotiated QUIC datagram MTU.
        let mut stream = conn.open_uni().await.map_err(err)?;
        stream.write_all(&frame).await.map_err(err)?;
        stream.finish().map_err(err)
    }
}

async fn receive_packet(conn: &Connection) -> Result<Bytes> {
    tokio::select! {
        frame=conn.read_datagram()=>frame.map_err(err),
        stream=conn.accept_uni()=>{
            let mut stream=stream.map_err(err)?;
            Ok(timeout(Duration::from_secs(3),stream.read_to_end(MAX_PACKET+2)).await.map_err(err)?.map_err(err)?.into())
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
    pub async fn start(ticket: &Ticket) -> Result<Self> {
        let ep = endpoint_builder().bind().await.map_err(err)?;
        if force_relay() {
            timeout(Duration::from_secs(12), ep.online())
                .await
                .map_err(|_| "本机无法连接中转服务".to_string())?;
        }
        let conn = timeout(
            Duration::from_secs(20),
            ep.connect(ticket.address.clone(), ALPN),
        )
        .await
        .map_err(|_| "大厅暂时离线，或此网络无法连接中转".to_string())?
        .map_err(|e| format!("无法建立到大厅的加密连接：{e}"))?;
        let (mut tx, mut rx) = conn
            .open_bi()
            .await
            .map_err(|e| format!("无法打开大厅认证通道：{e}"))?;
        tx.write_all(ticket.key.as_bytes()).await.map_err(err)?;
        tx.finish().map_err(err)?;
        let mut ack = [0; 2];
        timeout(Duration::from_secs(6), rx.read_exact(&mut ack))
            .await
            .map_err(|_| "大厅邀请认证响应超时".to_string())?
            .map_err(|e| format!("大厅未接受邀请：{e}"))?;
        if &ack != b"OK" {
            return Err("邀请被拒绝".into());
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(err)?;
        let gateway = listener.local_addr().map_err(err)?.port();
        let ip = crate::host::local_ip();
        let udp = UdpSocket::bind((ip, 0)).await.map_err(err)?;
        let media = SocketAddrV4::new(ip, udp.local_addr().map_err(err)?.port());
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
        let media_task = tokio::spawn(async move {
            let mut senders = Flows::new();
            let mut buf = vec![0; MAX_PACKET];
            // A large QUIC stream can be partially read. Retain its future when an
            // outgoing UDP packet wins select, so that a DTLS flight isn't reset.
            let mut next = Box::pin(receive_packet(&media_conn));
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
                        if send_packet(&media_conn,source.port(),&buf[..n]).await.is_err(){break}
                    }
                    received=&mut next=>{
                        let Ok(frame)=received else{break};
                        next.set(receive_packet(&media_conn));
                        if frame.len()<3 {continue}
                        if let Some(addr)=senders.get(u16::from_be_bytes([frame[0],frame[1]])) {
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

    #[cfg(test)]
    pub(crate) fn selected_transport(&self) -> Option<&'static str> {
        self.conn.paths().iter().find(|p| p.is_selected()).map(|p| {
            if p.is_relay() {
                "relay"
            } else if p.is_ip() {
                "direct"
            } else {
                "other"
            }
        })
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

    #[test]
    fn dormant_media_flows_expire_and_new_channels_replace_old_ports() {
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
