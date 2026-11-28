use crate::network::{GuestEndpoint, HostEndpoint, Ticket};
use std::future::Future;
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    time::timeout,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opens real iroh endpoints and local TCP/UDP ports"]
async fn authenticated_tunnel_carries_tcp_and_both_udp_sizes() -> Result<(), String> {
    with_requested_relay(tunnel_exchange()).await
}

// Scope the relay override to this test task; simultaneous normal clients keep
// the packaged endpoint configuration. The local relay never binds a public interface.
pub(super) async fn with_requested_relay<F: Future<Output = Result<(), String>>>(
    test: F,
) -> Result<(), String> {
    if std::env::var("DUIPIN_TEST_LOCAL_RELAY").as_deref() != Ok("1") {
        return test.await;
    }
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, 0)));
    let server = Server::spawn(config).await.map_err(|e| e.to_string())?;
    let addr = server.http_addr().ok_or("test relay failed to bind")?;
    let url: iroh::RelayUrl = format!("http://{addr}")
        .parse()
        .map_err(|_| "invalid test relay address")?;
    println!(
        "local official iroh relay at {addr}; direct IP transports and address lookup disabled"
    );
    let result = crate::network::TEST_RELAY
        .scope([url].into_iter().collect(), test)
        .await;
    server.shutdown().await.map_err(|e| e.to_string())?;
    result
}

pub(super) fn expect_relay() -> bool {
    std::env::var("DUIPIN_FORCE_RELAY").as_deref() == Ok("1")
        || std::env::var("DUIPIN_TEST_LOCAL_RELAY").as_deref() == Ok("1")
}

async fn tunnel_exchange() -> Result<(), String> {
    let err = |e: std::io::Error| e.to_string();
    let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(err)?;
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(err)?;
    let key = "a".repeat(64);
    let host = HostEndpoint::start(
        crate::preferences::random_key(),
        key.clone(),
        tcp.local_addr().map_err(err)?.port(),
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, udp.local_addr().map_err(err)?.port()),
        crate::diagnostics::Diagnostics::default(),
    )
    .await?;
    let verify = async {
        let invite = host.share_invite(key.clone()).await?;
        let mut bad = Ticket::parse(&invite)?;
        bad.key = "b".repeat(64);
        let diagnostics = crate::diagnostics::Diagnostics::default();
        if let Ok(unexpected) =
            GuestEndpoint::start(&bad, &diagnostics.room(crate::diagnostics::Role::Guest)).await
        {
            unexpected.close().await;
            return Err("host accepted invalid invite key".to_string());
        }
        let guest = GuestEndpoint::start(
            &Ticket::parse(&invite)?,
            &diagnostics.room(crate::diagnostics::Role::Guest),
        )
        .await?;
        let exchanges = async {
            let mut guest_tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, guest.gateway))
                .await
                .map_err(err)?;
            let request = b"lobby and RTC signaling";
            guest_tcp.write_all(request).await.map_err(err)?;
            let (mut host_tcp, _) = tcp.accept().await.map_err(err)?;
            let mut request_copy = vec![0; request.len()];
            host_tcp.read_exact(&mut request_copy).await.map_err(err)?;
            if request_copy != request {
                return Err("TCP request changed in transit".to_string());
            }
            host_tcp.write_all(b"reply").await.map_err(err)?;
            let mut reply = [0; 5];
            guest_tcp.read_exact(&mut reply).await.map_err(err)?;
            if &reply != b"reply" {
                return Err("TCP response changed in transit".to_string());
            }
            let mut sender = UdpSocket::bind((*guest.media.ip(), 0)).await.map_err(err)?;
            // 32 KiB exceeds the locally negotiated QUIC datagram MTU and exercises the
            // unidirectional-stream fallback in both directions. Distinct replies prove routing.
            for size in [80, 32 * 1024] {
                let packet: Vec<u8> = (0..size).map(|i| (i % 241) as u8).collect();
                sender.send_to(&packet, guest.media).await.map_err(err)?;
                let mut buffer = vec![0; size + 1];
                let (n, upstream) = udp.recv_from(&mut buffer).await.map_err(err)?;
                if buffer[..n] != packet {
                    return Err(format!("UDP {size} request changed in transit"));
                }
                buffer[..n].reverse();
                udp.send_to(&buffer[..n], upstream).await.map_err(err)?;
                // Keep outbound traffic active while receiving the oversized stream.
                // A select loop that abandons an already accepted stream loses the reply.
                let mut tick = tokio::time::interval(Duration::from_millis(5));
                let n = loop {
                    tokio::select! {
                        result = sender.recv_from(&mut buffer) => break result.map_err(err)?.0,
                        _ = tick.tick(), if size > 1500 => {
                            sender.send_to(b"active audio", guest.media).await.map_err(err)?;
                        }
                    }
                };
                if !buffer[..n].iter().eq(packet.iter().rev()) {
                    return Err(format!("UDP {size} response changed in transit"));
                }
                if size == 80 {
                    // WebRTC opens new ports on rejoin. A delayed SFU packet to the
                    // closed port must not stop the tunnel serving the next sender.
                    sender = UdpSocket::bind((*guest.media.ip(), 0)).await.map_err(err)?;
                    udp.send_to(b"late media", upstream).await.map_err(err)?;
                }
            }
            let route = guest
                .selected_transport()
                .ok_or("iroh has no selected path")?;
            if expect_relay() && route != "relay" {
                return Err(format!("expected relay; iroh selected {route}"));
            }
            println!("authenticated iroh tunnel: selected {route}; TCP and UDP (80, 32768 bytes) round-tripped");
            Ok(())
        };
        let result = timeout(Duration::from_secs(10), exchanges)
            .await
            .map_err(|_| "tunnel exchange timed out".to_string());
        guest.close().await;
        result?
    };
    let result = timeout(Duration::from_secs(60), verify)
        .await
        .map_err(|_| "iroh test timed out".to_string());
    host.close().await;
    result?
}
