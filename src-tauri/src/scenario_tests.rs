mod tunnel;

use crate::{
    host::Host,
    network::{GuestEndpoint, Ticket},
    preferences::Profile,
    voice::join_microphone,
};
use futures_util::{SinkExt, StreamExt};
use livekit::{
    prelude::{RemoteTrack, RoomEvent},
    webrtc::{
        audio_source::native::NativeAudioSource, audio_stream::native::NativeAudioStream,
        prelude::AudioFrame, stats::RtcStats,
    },
};
use serde_json::{json, Value};
use std::{net::SocketAddrV4, path::Path, time::Duration};
use tokio::{net::TcpStream, sync::mpsc::UnboundedReceiver, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

async fn send(ws: &mut Socket, op: Value) -> Result<()> {
    ws.send(Message::Text(op.to_string().into()))
        .await
        .map_err(err)
}

async fn event(ws: &mut Socket, accept: impl Fn(&Value) -> bool) -> Result<Value> {
    timeout(Duration::from_secs(7), async {
        while let Some(frame) = ws.next().await {
            match frame.map_err(err)? {
                Message::Text(t) => {
                    let v: Value = serde_json::from_str(&t).map_err(err)?;
                    if v["type"] == "error" {
                        return Err(v["message"].to_string());
                    }
                    if accept(&v) {
                        return Ok(v);
                    }
                }
                Message::Ping(p) => ws.send(Message::Pong(p)).await.map_err(err)?,
                Message::Close(_) => break,
                _ => {}
            }
        }
        Err("lobby socket closed".to_string())
    })
    .await
    .map_err(|_| "lobby event timed out".to_string())?
}

async fn login(
    port: u16,
    invite_key: &str,
    identity: &str,
    playback: Option<SocketAddrV4>,
) -> Result<(Socket, String)> {
    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .map_err(err)?;
    send(
        &mut ws,
        json!({"type":"hello","inviteKey":invite_key,"identity":identity,"nickname":"PCM test",
        "playbackAddress":playback.map(|addr|addr.to_string())}),
    )
    .await?;
    let v = event(&mut ws, |v| v["type"] == "snapshot").await?;
    Ok((
        ws,
        v["selfId"]
            .as_str()
            .ok_or("snapshot missing selfId")?
            .into(),
    ))
}

async fn ticket(ws: &mut Socket, room: &str) -> Result<String> {
    send(ws, json!({"type":"join","channelId":room})).await?;
    let voice = event(ws, |v| v["type"] == "voice" && v["channelId"] == room).await?;
    Ok(voice["token"]
        .as_str()
        .ok_or("voice event missing token")?
        .into())
}

async fn keep_alive(mut ws: Socket) {
    while let Some(Ok(message)) = ws.next().await {
        match message {
            Message::Ping(p) => {
                if ws.send(Message::Pong(p)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

// Verify the receiver really selected the guest's iroh UDP forwarder. Two
// processes on the same machine could otherwise negotiate a direct ICE route.
fn selected_proxy(stats: &[RtcStats], proxy: SocketAddrV4, sending: bool) -> bool {
    stats.iter().any(|entry| {
        let RtcStats::Transport(transport) = entry else {
            return false;
        };
        if !stats.iter().any(|s| match s {
            RtcStats::InboundRtp(r) if !sending => {
                r.stream.transport_id == transport.rtc.id
                    && r.stream.kind == "audio"
                    && r.received.packets_received > 0
            }
            RtcStats::OutboundRtp(r) if sending => {
                r.stream.transport_id == transport.rtc.id
                    && r.stream.kind == "audio"
                    && r.sent.packets_sent > 0
            }
            _ => false,
        }) {
            return false;
        }
        stats.iter().any(|pair| {
            let RtcStats::CandidatePair(pair) = pair else {
                return false;
            };
            if pair.rtc.id != transport.transport.selected_candidate_pair_id
                || (if sending {
                    pair.candidate_pair.bytes_sent
                } else {
                    pair.candidate_pair.bytes_received
                }) == 0
            {
                return false;
            }
            stats.iter().any(|remote| {
                let RtcStats::RemoteCandidate(remote) = remote else {
                    return false;
                };
                remote.rtc.id == pair.candidate_pair.remote_candidate_id
                    && remote.remote_candidate.address == proxy.ip().to_string()
                    && remote.remote_candidate.port == i32::from(proxy.port())
                    && remote.remote_candidate.protocol.eq_ignore_ascii_case("udp")
            })
        })
    })
}

fn describe_routes(stats: &[RtcStats]) -> String {
    let mut rows = Vec::new();
    for stat in stats {
        match stat {
            RtcStats::Transport(t) => rows.push(format!("transport {} selected={} ice={:?}", t.rtc.id,
                t.transport.selected_candidate_pair_id, t.transport.ice_state)),
            RtcStats::CandidatePair(p) if p.candidate_pair.nominated || stats.iter().any(|entry|
                matches!(entry, RtcStats::Transport(t) if t.transport.selected_candidate_pair_id == p.rtc.id)
            ) => rows.push(format!("pair {} local={} remote={} state={:?} tx={} rx={}",
                p.rtc.id, p.candidate_pair.local_candidate_id, p.candidate_pair.remote_candidate_id,
                p.candidate_pair.state, p.candidate_pair.bytes_sent, p.candidate_pair.bytes_received)),
            RtcStats::LocalCandidate(c) => rows.push(format!("local {} {}:{} {} {:?}",
                c.rtc.id, c.local_candidate.address, c.local_candidate.port,
                c.local_candidate.protocol, c.local_candidate.candidate_type)),
            RtcStats::RemoteCandidate(c) => rows.push(format!("remote {} {}:{} {} {:?}",
                c.rtc.id, c.remote_candidate.address, c.remote_candidate.port,
                c.remote_candidate.protocol, c.remote_candidate.candidate_type)),
            RtcStats::InboundRtp(r) => rows.push(format!("in {} transport={} packets={}",
                r.stream.kind, r.stream.transport_id, r.received.packets_received)),
            RtcStats::OutboundRtp(r) => rows.push(format!("out {} transport={} packets={}",
                r.stream.kind, r.stream.transport_id, r.sent.packets_sent)),
            _ => (),
        }
    }
    rows.join("; ")
}

// Correlate against both quadratures so the received codec frame can start at
// any phase. An unrelated microphone, comfort noise, or silent stream must fail.
fn contains_tone(samples: &[i16], hz: f64) -> bool {
    let (mut sine, mut cosine, mut energy) = (0., 0., 0.);
    for (i, &v) in samples.iter().enumerate() {
        let phase = std::f64::consts::TAU * i as f64 * hz / 48_000.;
        let sample = v as f64;
        sine += sample * phase.sin();
        cosine += sample * phase.cos();
        energy += sample * sample;
    }
    let n = samples.len() as f64;
    energy > n * 300_f64.powi(2) && 2. * (sine * sine + cosine * cosine) > 0.60 * n * energy
}

async fn feed_tone(source: &NativeAudioSource, hz: f64) -> Result<()> {
    let samples: Vec<i16> = (0..480)
        .map(|i| (7000. * (std::f64::consts::TAU * i as f64 * hz / 48_000.).sin()) as i16)
        .collect();
    let frame = AudioFrame {
        data: samples.as_slice().into(),
        sample_rate: 48_000,
        num_channels: 1,
        samples_per_channel: 480,
    };
    let mut clock = tokio::time::interval(Duration::from_millis(10));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        clock.tick().await;
        source.capture_frame(&frame).await.map_err(err)?;
    }
}

async fn hear_tone(
    events: &mut UnboundedReceiver<RoomEvent>,
    sender_id: &str,
    hz: f64,
) -> Result<()> {
    let track = loop {
        match events.recv().await.ok_or("subscriber event stream ended")? {
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                participant,
                ..
            } if participant.identity().to_string() == sender_id => break track,
            RoomEvent::Disconnected { reason } => {
                return Err(format!("subscriber disconnected: {reason:?}"))
            }
            _ => {}
        }
    };
    let mut stream = NativeAudioStream::new(track.rtc_track(), 48_000, 1);
    let mut verified_samples = 0;
    while verified_samples < 4800 {
        let frame = stream.next().await.ok_or("decoded stream ended early")?;
        if frame.sample_rate == 48_000 && frame.num_channels == 1 && contains_tone(&frame.data, hz)
        {
            verified_samples += frame.samples_per_channel;
        }
    }
    Ok(())
}

async fn exercise(
    host: &Host,
    guest: &GuestEndpoint,
    guest_profile: &Profile,
    key: &str,
) -> Result<()> {
    let (mut sender_ws, sender_id) = login(host.gateway, key, &host.credential, None).await?;
    let (mut receiver_ws, guest_id) = login(
        guest.gateway,
        key,
        &guest_profile.player_credential(&host.overlay.id()),
        Some(guest.media),
    )
    .await?;
    send(&mut sender_ws, json!({"type":"create","name":"PCM"})).await?;
    let state = event(&mut sender_ws, |v| v["channels"][0]["name"] == "PCM").await?;
    let room = state["channels"][0]["id"].as_str().ok_or("no room id")?;
    let pub_token = ticket(&mut sender_ws, room).await?;
    let sub_token = ticket(&mut receiver_ws, room).await?;

    let audio = async {
        let (publisher, mut host_events, host_mic) =
            join_microphone(&format!("ws://127.0.0.1:{}", host.gateway), &pub_token)
                .await
                .map_err(|e| format!("host LiveKit join: {e}"))?;
        let (receiver, mut guest_events, guest_mic) =
            match join_microphone(&format!("ws://127.0.0.1:{}", guest.gateway), &sub_token).await {
                Ok(joined) => joined,
                Err(e) => {
                    let _ = publisher.close().await;
                    return Err(format!("guest LiveKit join through iroh: {e}"));
                }
            };
        let listen = async {
            tokio::try_join!(
                hear_tone(&mut guest_events, &sender_id, 1000.),
                hear_tone(&mut host_events, &guest_id, 1700.),
            )?;
            // WebRTC may reuse a cached stats snapshot for a short interval.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let stats = receiver.get_stats().await.map_err(err)?;
                let subscriber_ok = selected_proxy(&stats.subscriber_stats, guest.media, false);
                let publisher_ok = selected_proxy(&stats.publisher_stats, guest.media, true);
                if subscriber_ok && publisher_ok {
                    println!(
                        "guest UDP proxy {} confirmed on sending and receiving ICE transports",
                        guest.media
                    );
                    return Ok(());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!("decoded both tones; guest forwarder {}, iroh {:?}, subscriber {subscriber_ok}, publisher {publisher_ok}\nsubscriber stats: {}\npublisher stats: {}",
                        guest.media, guest.selected_transport(), describe_routes(&stats.subscriber_stats),
                        describe_routes(&stats.publisher_stats)));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        let result = tokio::select! {
            r = feed_tone(&host_mic, 1000.) => r,
            r = feed_tone(&guest_mic, 1700.) => r,
            r = listen => r,
        };
        let _ = tokio::join!(publisher.close(), receiver.close());
        result
    };
    tokio::select! {
        _ = keep_alive(sender_ws) => Err("host lobby socket disconnected".into()),
        _ = keep_alive(receiver_ws) => Err("guest lobby socket disconnected".into()),
        r = timeout(Duration::from_secs(60),audio) => r.map_err(|_| "timed out waiting for LiveKit/decoded tone/selected ICE path".to_string())?,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts real packaged LiveKit, native WebRTC and iroh endpoints"]
async fn two_clients_deliver_pcm_through_iroh() -> Result<()> {
    tunnel::with_requested_relay(two_clients()).await
}

async fn two_clients() -> Result<()> {
    let storage = tempfile::tempdir().map_err(err)?;
    let host_profile = Profile::load(storage.path().join("host"))?;
    let guest_profile = Profile::load(storage.path().join("guest"))?;
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/livekit-server.exe");
    let host = Host::start(&host_profile, &binary).await?;
    let scenario = async {
        let invite = Ticket::parse(
            &host
                .overlay
                .share_invite(host_profile.invite_key.clone())
                .await?,
        )?;
        let guest = GuestEndpoint::start(&invite).await?;
        let result = exercise(&host, &guest, &guest_profile, &invite.key).await.and_then(|()| {
            let route = guest.selected_transport().ok_or("iroh has no selected path")?;
            if tunnel::expect_relay() && route != "relay" {
                return Err(format!("expected relay; iroh selected {route}"));
            }
            println!("verified 1 kHz host and 1.7 kHz guest PCM in both directions through selected iroh {route} path");
            Ok(())
        });
        guest.close().await;
        result
    };
    let result = timeout(Duration::from_secs(100), scenario)
        .await
        .map_err(|_| "iroh scenario timed out".to_string());
    host.close().await;
    result?
}
