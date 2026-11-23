use crate::{
    lobby, network,
    preferences::{random_key, Profile},
};
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    sync::watch,
    task::JoinHandle,
};

pub struct Host {
    media: Child,
    stop: watch::Sender<bool>,
    serve: JoinHandle<()>,
    pub overlay: network::HostEndpoint,
    pub gateway: u16,
    pub invite: String,
    pub credential: String,
}

fn unused_tcp_port() -> Result<u16, String> {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| e.to_string())
}

pub fn local_ip() -> Ipv4Addr {
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

impl Host {
    pub async fn start(profile: &Profile, executable: &Path) -> Result<Self, String> {
        if !executable.is_file() {
            return Err("安装包缺少 LiveKit Server，请重新安装或先运行 prepare-livekit.ps1".into());
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| e.to_string())?;
        let gateway = listener.local_addr().map_err(|e| e.to_string())?.port();
        let signaling = unused_tcp_port()?;
        let media_ip = local_ip();
        let media_port = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|s| s.local_addr())
            .map_err(|e| e.to_string())?
            .port();
        let api_key = uuid::Uuid::new_v4().simple().to_string();
        let api_secret = hex::encode(random_key());
        // Keep the admin endpoint private. WebRTC authenticates the separate ICE/UDP port.
        // Clients initiate ICE toward a locally reachable UDP endpoint. The SFU
        // must not select a direct return route to a guest on the same LAN.
        let config = format!("port: {signaling}\nbind_addresses: [127.0.0.1]\nrtc:\n  node_ip: {media_ip}\n  use_external_ip: false\n  use_ice_lite: true\n  udp_port: {media_port}\n  tcp_port: 0\n  enable_loopback_candidate: true\nkeys:\n  {api_key}: {api_secret}\n");
        let mut cmd = Command::new(executable);
        cmd.env("LIVEKIT_CONFIG", &config)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(test)]
        if let Some(path) = std::env::var_os("DUIPIN_TEST_SFU_LOG") {
            let log = std::fs::File::create(path).map_err(|e| e.to_string())?;
            cmd.env(
                "LIVEKIT_CONFIG",
                format!("{config}logging:\n  level: debug\n  pion_level: info\n"),
            )
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);
            eprintln!("test SFU media {media_ip}:{media_port}, signaling 127.0.0.1:{signaling}");
        }
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        let mut media = cmd.spawn().map_err(|e| format!("语音服务启动失败：{e}"))?;
        let ready = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(status) = media.try_wait().map_err(|e| e.to_string())? {
                    return Err(format!("语音服务启动后退出 ({status})"));
                }
                if tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, signaling))
                    .await
                    .is_ok()
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(90)).await;
            }
        })
        .await
        .map_err(|_| "等待语音服务启动超时".to_string())?;
        ready?;
        let overlay = network::HostEndpoint::start(
            profile.host_key,
            profile.invite_key.clone(),
            gateway,
            SocketAddrV4::new(media_ip, media_port),
        )
        .await?;
        let credential = profile.player_credential(&overlay.id());
        let (stop, mut stopping) = watch::channel(false);
        let router = lobby::router_with_shutdown(
            lobby::Config {
                database: profile.directory.join("channels.sqlite3"),
                owner_identity: credential.clone(),
                invite_key: profile.invite_key.clone(),
                livekit_key: api_key,
                livekit_secret: api_secret,
                livekit_http_url: format!("http://127.0.0.1:{signaling}"),
            },
            stopping.clone(),
        )?;
        let serve = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stopping.wait_for(|v| *v).await;
                })
                .await;
        });
        let invite = overlay.invite(&profile.invite_key)?;
        Ok(Self {
            media,
            stop,
            serve,
            overlay,
            gateway,
            invite,
            credential,
        })
    }

    pub async fn close(mut self) {
        let _ = self.stop.send(true);
        self.overlay.close().await;
        self.serve.abort();
        let _ = self.media.kill().await;
    }
}
