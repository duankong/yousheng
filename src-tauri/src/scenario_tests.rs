pub(crate) mod rtp_trace;
mod tunnel;

use crate::{
    host::Host,
    lobby::ROOM_PROTOCOL,
    network::{GuestEndpoint, Ticket},
    preferences::Profile,
    voice::{join_microphone, ScenarioPlayback},
};
use futures_util::{
    future::{join_all, try_join_all},
    SinkExt, StreamExt,
};
use livekit::{
    prelude::{RemoteAudioTrack, RemoteTrack, Room, RoomEvent},
    webrtc::{
        audio_source::native::NativeAudioSource,
        audio_stream::native::NativeAudioStream,
        prelude::AudioFrame,
        stats::{InboundRtpStats, RtcStats},
    },
};
use serde_json::{json, Value};
use std::{
    net::SocketAddrV4,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, OnceLock,
    },
    thread,
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{mpsc::UnboundedReceiver, Barrier},
    task::JoinSet,
    time::{timeout, timeout_at, Instant},
};
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
) -> Result<(Socket, Value)> {
    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .map_err(err)?;
    send(
        &mut ws,
        json!({"type":"hello","roomProtocol":ROOM_PROTOCOL,"inviteKey":invite_key,"identity":identity,"nickname":"PCM test",
        "playbackAddress":playback.map(|addr|addr.to_string())}),
    )
    .await?;
    let v = event(&mut ws, |v| v["type"] == "snapshot").await?;
    Ok((ws, v))
}

async fn ticket(ws: &mut Socket, room: &str) -> Result<String> {
    let voice = event(ws, |v| {
        v["type"] == "voice" && v["roomId"] == room && v["token"].is_string()
    })
    .await?;
    Ok(voice["token"]
        .as_str()
        .ok_or("voice event missing token")?
        .into())
}

async fn keep_alive(ws: &mut Socket) {
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

async fn verify_guest_media(room: &Room, guest: &GuestEndpoint) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let stats = room.get_stats().await.map_err(err)?;
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
            return Err(format!("decoded tones; guest forwarder {}, iroh {:?}, subscriber {subscriber_ok}, publisher {publisher_ok}\nsubscriber stats: {}\npublisher stats: {}",
                guest.media, guest.selected_transport(), describe_routes(&stats.subscriber_stats),
                describe_routes(&stats.publisher_stats)));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
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

const MARKER_COUNT: usize = 10;
const MARKER_SLOT_MS: u64 = 160;
const MARKER_LENGTH_MS: u64 = 80;
const PLAYERS: usize = 4;
const OBSERVATION_MS: u64 = MARKER_SLOT_MS * (PLAYERS * MARKER_COUNT) as u64 + 400;
type MarkerTimes = [OnceLock<Instant>; MARKER_COUNT];

#[derive(Default)]
struct AudioProbe {
    start: OnceLock<Instant>,
    sent: [MarkerTimes; PLAYERS],
    stop: AtomicBool,
    input_frames: [AtomicU64; PLAYERS],
    max_input_gap_us: [AtomicU64; PLAYERS],
    max_capture_call_us: [AtomicU64; PLAYERS],
}

impl AudioProbe {
    fn schedule(&self) -> Instant {
        *self
            .start
            .get_or_init(|| Instant::now() + Duration::from_millis(350))
    }

    fn deadline(&self) -> Instant {
        self.schedule() + Duration::from_millis(OBSERVATION_MS)
    }
}

struct CpuContention {
    stop: Arc<AtomicBool>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl CpuContention {
    fn start() -> Self {
        let cpus = thread::available_parallelism().map_or(1, usize::from);
        let count = (cpus / 2).clamp(1, 8);
        let stop = Arc::new(AtomicBool::new(false));
        let workers = (0..count)
            .map(|_| {
                let stop = stop.clone();
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let work_until = std::time::Instant::now() + Duration::from_millis(20);
                        while std::time::Instant::now() < work_until {
                            std::hint::spin_loop();
                        }
                        thread::park_timeout(Duration::from_millis(10));
                    }
                })
            })
            .collect();
        println!("CPU contention: {count} workers, 20 ms busy / 10 ms paused");
        Self { stop, workers }
    }
}

impl Drop for CpuContention {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn tone_block(frequency: f64) -> [i16; 480] {
    std::array::from_fn(|index| {
        (7000. * (std::f64::consts::TAU * index as f64 * frequency / 48_000.).sin()) as i16
    })
}

fn marker_frequency(sender: usize, number: usize) -> f64 {
    1800. + 100. * (sender * MARKER_COUNT + number) as f64
}

async fn feed_probe(
    source: &NativeAudioSource,
    base: f64,
    sender: usize,
    probe: &AudioProbe,
) -> Result<()> {
    let regular = tone_block(base);
    let markers: [_; MARKER_COUNT] =
        std::array::from_fn(|number| tone_block(marker_frequency(sender, number)));
    let period = Duration::from_millis(10);
    let mut next = Instant::now();
    let mut previous_frame = next;
    loop {
        thread::sleep(next.saturating_duration_since(Instant::now()));
        if probe.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let now = Instant::now();
        let tick = next;
        let missed = (now.saturating_duration_since(tick).as_millis() / 10) as u32;
        next += period * (1 + missed);
        let measuring = probe.start.get().is_some_and(|&start| {
            now >= start && now < start + Duration::from_millis(OBSERVATION_MS)
        });
        if measuring {
            probe.input_frames[sender].fetch_add(1, Ordering::Relaxed);
            let gap_us = now.duration_since(previous_frame).as_micros() as u64;
            probe.max_input_gap_us[sender].fetch_max(gap_us, Ordering::Relaxed);
        }
        let mut samples = &regular;
        if let Some(elapsed) = probe
            .start
            .get()
            .and_then(|start| tick.checked_duration_since(*start))
        {
            let elapsed_ms = elapsed.as_millis() as u64;
            let slot = (elapsed_ms / MARKER_SLOT_MS) as usize;
            let number = slot / PLAYERS;
            if number < markers.len()
                && slot % PLAYERS == sender
                && elapsed_ms % MARKER_SLOT_MS < MARKER_LENGTH_MS
            {
                probe.sent[sender][number].get_or_init(|| {
                    rtp_trace::marker("input", sender, None, number, now.into_std());
                    now
                });
                samples = &markers[number];
            }
        }
        previous_frame = now;
        source
            .capture_frame(&AudioFrame {
                data: samples.as_slice().into(),
                sample_rate: 48_000,
                num_channels: 1,
                samples_per_channel: 480,
            })
            .await
            .map_err(err)?;
        if measuring {
            let capture_us = now.elapsed().as_micros() as u64;
            probe.max_capture_call_us[sender].fetch_max(capture_us, Ordering::Relaxed);
        }
    }
}

fn tone_power(samples: &[i16], frequency: f64) -> f64 {
    let coefficient = 2. * (std::f64::consts::TAU * frequency / 48_000.).cos();
    let (mut latest, mut previous) = (0., 0.);
    for &sample in samples {
        (latest, previous) = (f64::from(sample) + coefficient * latest - previous, latest);
    }
    latest * latest + previous * previous - coefficient * previous * latest
}

#[derive(Default)]
struct MeasuredPath {
    sender: usize,
    receiver: usize,
    marks: [Option<Duration>; MARKER_COUNT],
    delivered: usize,
    unrecognized: usize,
    current_gap: usize,
    longest_gap: usize,
    current_quiet: usize,
    longest_quiet: usize,
}

impl MeasuredPath {
    fn observe(
        &mut self,
        samples: &[i16],
        base: f64,
        at: Instant,
        probe: &AudioProbe,
        power_fraction: f64,
    ) {
        let energy = samples
            .iter()
            .map(|&sample| f64::from(sample).powi(2))
            .sum::<f64>();
        let threshold = 0.5 * power_fraction * samples.len() as f64 * energy;
        let audible = energy > samples.len() as f64 * 300_f64.powi(2);
        let mut recognized = audible && tone_power(samples, base) > threshold;
        if audible && !recognized {
            for (number, sent) in probe.sent[self.sender].iter().enumerate() {
                let Some(delay) = sent.get().and_then(|sent| at.checked_duration_since(*sent))
                else {
                    continue;
                };
                if delay < Duration::from_secs(1)
                    && tone_power(samples, marker_frequency(self.sender, number)) > threshold
                {
                    self.marks[number].get_or_insert_with(|| {
                        let endpoint = if power_fraction > 0.5 {
                            "decoded"
                        } else {
                            "mixed"
                        };
                        rtp_trace::marker(
                            endpoint,
                            self.sender,
                            Some(self.receiver),
                            number,
                            at.into_std(),
                        );
                        delay
                    });
                    recognized = true;
                    break;
                }
            }
        }
        if at >= probe.schedule() {
            self.delivered += samples.len();
            if audible {
                self.current_quiet = 0;
            } else {
                self.current_quiet += samples.len();
                self.longest_quiet = self.longest_quiet.max(self.current_quiet);
            }
            if recognized {
                self.current_gap = 0;
            } else {
                self.unrecognized += samples.len();
                self.current_gap += samples.len();
                self.longest_gap = self.longest_gap.max(self.current_gap);
            }
        }
    }
}

async fn measure_track(
    track: &RemoteAudioTrack,
    base: f64,
    sender: usize,
    receiver: usize,
    probe: &AudioProbe,
) -> Result<MeasuredPath> {
    let mut stream = NativeAudioStream::new(track.rtc_track(), 48_000, 1);
    let mut path = MeasuredPath {
        sender,
        receiver,
        ..Default::default()
    };
    while let Ok(frame) = timeout_at(probe.deadline(), stream.next()).await {
        let at = Instant::now();
        let frame = frame.ok_or("decoded stream ended while measuring latency")?;
        if frame.sample_rate != 48_000 || frame.num_channels != 1 {
            return Err("unexpected format in decoded audio stream".into());
        }
        path.observe(&frame.data, base, at, probe, 0.60);
    }
    Ok(path)
}

async fn measure_playout(
    mut playback: ScenarioPlayback,
    receiver: usize,
    probe: Arc<AudioProbe>,
    senders: Vec<(usize, f64)>,
) -> Result<Vec<MeasuredPath>> {
    tokio::task::spawn_blocking(move || {
        let mut paths: Vec<_> = senders
            .into_iter()
            .map(|(sender, frequency)| {
                (
                    MeasuredPath {
                        sender,
                        receiver,
                        ..Default::default()
                    },
                    frequency,
                )
            })
            .collect();
        let period = Duration::from_millis(10);
        let deadline = probe.deadline();
        // Match the device path: drain from the moment the sink attaches, including
        // warm-up. Waiting for the measurement window would create a PCM backlog.
        let mut next = Instant::now();
        let mut output = [0; 480];
        while next < deadline {
            thread::sleep(next.saturating_duration_since(Instant::now()));
            let callback = Instant::now();
            if callback >= deadline || probe.stop.load(Ordering::Relaxed) {
                break;
            }
            playback.render(&mut output);
            let at = Instant::now();
            for (path, base) in &mut paths {
                path.observe(&output, *base, at, &probe, 0.16);
            }
            let intervals = (callback.saturating_duration_since(next).as_millis() / 10) as u32;
            next += period * (intervals + 1);
        }
        paths.into_iter().map(|(path, _)| path).collect()
    })
    .await
    .map_err(err)
}

fn latency_ms(sorted: &[Duration], percentile: usize) -> f64 {
    let rank = (sorted.len() * percentile).div_ceil(100);
    sorted
        .get(rank.saturating_sub(1))
        .map_or(f64::NAN, |value| value.as_secs_f64() * 1000.)
}

fn report_probe(endpoint: &str, paths: &[MeasuredPath], minimum_delivered: usize) -> Result<()> {
    let expected = OBSERVATION_MS as usize * 48;
    let mut all = Vec::new();
    for path in paths {
        let mut marks: Vec<_> = path.marks.iter().copied().flatten().collect();
        marks.sort_unstable();
        println!(
            "{endpoint} {} -> {}: {}/{} marks, p50={:.1} ms p95={:.1} ms, {} unrecognized samples, {} delivered (nominal {}), longest unrecognized run={:.1} ms, longest low-level run={:.1} ms",
            path.sender + 1,
            path.receiver + 1,
            marks.len(),
            path.marks.len(),
            latency_ms(&marks, 50),
            latency_ms(&marks, 95),
            path.unrecognized,
            path.delivered,
            expected,
            path.longest_gap as f64 / 48.,
            path.longest_quiet as f64 / 48.,
        );
        all.extend(marks);
    }
    all.sort_unstable();
    println!(
        "{endpoint} total: {}/{} marks across {} paths, p50={:.1} ms p95={:.1} ms max={:.1} ms",
        all.len(),
        paths.len() * MARKER_COUNT,
        paths.len(),
        latency_ms(&all, 50),
        latency_ms(&all, 95),
        latency_ms(&all, 100),
    );
    if paths.len() != PLAYERS * (PLAYERS - 1)
        || paths.iter().any(|path| {
            path.marks.iter().flatten().count() < MARKER_COUNT - 1
                || path.delivered < minimum_delivered
                || path.unrecognized > path.delivered / 20
        })
    {
        return Err(format!(
            "four-player {endpoint} PCM lost markers or sustained audio"
        ));
    }
    Ok(())
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
    verify_tone(&track, hz).await
}

async fn verify_tone(track: &RemoteAudioTrack, hz: f64) -> Result<()> {
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
    let (mut host_ws, host_snapshot) = login(host.gateway, key, &host.credential, None).await?;
    let room = host_snapshot["roomId"].as_str().ok_or("no room id")?;
    let host_id = host_snapshot["selfId"].as_str().ok_or("no host id")?;
    let guest_identity = guest_profile.player_credential(&host.overlay.id());
    let host_url = format!("ws://127.0.0.1:{}", host.gateway);
    let guest_url = format!("ws://127.0.0.1:{}", guest.gateway);
    // Keep the host in the room while the guest returns over the same iroh tunnel.
    // A different guest tone on return rules out decoded audio from the previous call.
    for (attempt, guest_hz) in [("initial join", 1700.), ("same-room rejoin", 2300.)] {
        let (mut guest_ws, guest_snapshot) =
            login(guest.gateway, key, &guest_identity, Some(guest.media)).await?;
        let guest_id = guest_snapshot["selfId"].as_str().ok_or("no guest id")?;
        if guest_snapshot["roomId"] != room
            || guest_snapshot["members"].as_array().map(Vec::len) != Some(2)
        {
            return Err(format!("{attempt}: guest did not enter the host's room"));
        }
        let (host_ticket, guest_ticket) =
            tokio::try_join!(ticket(&mut host_ws, room), ticket(&mut guest_ws, room))?;
        let chat = format!("chat before voice: {attempt}");
        send(
            &mut guest_ws,
            json!({"type":"chat","roomId":room,"text":chat,"requestId":1}),
        )
        .await?;
        let (_, guest_reply) = tokio::try_join!(
            event(&mut host_ws, |v| v["messages"].as_array().is_some_and(
                |messages| messages
                    .iter()
                    .any(|message| message["text"] == chat && message["authorId"] == guest_id)
            )),
            event(&mut guest_ws, |v| v["requestId"] == 1),
        )?;
        if !guest_reply["error"].is_null() {
            return Err(format!("{attempt}: guest chat was not acknowledged"));
        }
        let (publisher, mut host_events, host_mic) = join_microphone(&host_url, &host_ticket)
            .await
            .map_err(|e| format!("host {attempt} LiveKit join: {e}"))?;
        let call = async {
            let (receiver, mut guest_events, guest_mic) =
                join_microphone(&guest_url, &guest_ticket)
                    .await
                    .map_err(|e| format!("guest {attempt} through iroh: {e}"))?;
            let listen = async {
                tokio::try_join!(
                    hear_tone(&mut guest_events, host_id, 1000.),
                    hear_tone(&mut host_events, guest_id, guest_hz),
                )?;
                verify_guest_media(&receiver, guest).await
            };
            let result = tokio::select! {
                _ = keep_alive(&mut guest_ws) => Err("guest room socket disconnected".into()),
                _ = keep_alive(&mut host_ws) => Err("host room socket disconnected".into()),
                r = feed_tone(&guest_mic, guest_hz) => r,
                r = feed_tone(&host_mic, 1000.) => r,
                r = listen => r,
            };
            let _ = receiver.close().await;
            result
        };
        let result = timeout(Duration::from_secs(42), call)
            .await
            .unwrap_or_else(|_| {
                Err("timed out waiting for LiveKit/decoded tone/selected ICE path".into())
            });
        let _ = publisher.close().await;
        result.map_err(|e| format!("{attempt}: {e}"))?;
        guest_ws.close(None).await.map_err(err)?;
        event(&mut host_ws, |v| {
            v["type"] == "voice" && v["roomId"] == room && v["token"].is_null()
        })
        .await?;
    }
    Ok(())
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
            println!("verified chat and two-way PCM on first join and same-room rejoin through selected iroh {route} path");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts real packaged LiveKit, native WebRTC and iroh endpoints"]
async fn four_players_exchange_pcm_and_chat_through_iroh() -> Result<()> {
    tunnel::with_requested_relay(four_players()).await
}

async fn four_players() -> Result<()> {
    let trace = rtp_trace::TraceFile::start()?;
    let storage = tempfile::tempdir().map_err(err)?;
    let host_profile = Profile::load(storage.path().join("host"))?;
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/livekit-server.exe");
    let host = Host::start(&host_profile, &binary).await?;
    let mut guests = Vec::new();
    let scenario = async {
        let invite = Ticket::parse(&host.overlay.share_invite(host_profile.invite_key).await?)?;
        for _ in 0..3 {
            guests.push(GuestEndpoint::start(&invite).await?);
        }
        exercise_four_players(&host, &guests, &invite.key, storage.path()).await
    };
    let result = timeout(Duration::from_secs(100), scenario)
        .await
        .map_err(|_| "four-player iroh scenario timed out".to_string());
    join_all(guests.into_iter().map(GuestEndpoint::close)).await;
    host.close().await;
    trace.write()?;
    result?
}

async fn exercise_four_players(
    host: &Host,
    guests: &[GuestEndpoint],
    key: &str,
    storage: &Path,
) -> Result<()> {
    let (host_ws, snapshot) = login(host.gateway, key, &host.credential, None).await?;
    let room_id = snapshot["roomId"].as_str().ok_or("no room id")?;
    let host_id = snapshot["selfId"].as_str().ok_or("no host id")?;
    let mut lobbies = vec![(host_ws, host.gateway)];
    let mut tones = vec![(host_id.to_string(), 500.)];
    for (index, (guest, frequency)) in guests.iter().zip([800., 1100., 1400.]).enumerate() {
        let profile = Profile::load(storage.join(format!("guest-{index}")))?;
        let credential = profile.player_credential(&host.overlay.id());
        let (ws, snapshot) = login(guest.gateway, key, &credential, Some(guest.media)).await?;
        let guest_id = snapshot["selfId"].as_str().ok_or("no guest id")?;
        if snapshot["roomId"] != room_id
            || snapshot["members"].as_array().map(Vec::len) != Some(index + 2)
        {
            return Err(format!("guest {guest_id} did not enter the host's room"));
        }
        lobbies.push((ws, guest.gateway));
        tones.push((guest_id.to_string(), frequency));
    }
    let mut calls = join_all(lobbies.iter_mut().zip(&tones).map(
        |((ws, gateway), (id, _))| async move {
            let token = ticket(ws, room_id).await?;
            let url = format!("ws://127.0.0.1:{gateway}");
            timeout(Duration::from_secs(25), join_microphone(&url, &token))
                .await
                .map_err(|_| format!("{id} timed out joining LiveKit"))?
                .map_err(|e| format!("{id} LiveKit join: {e}"))
        },
    ))
    .await;
    let messages: Vec<_> = tones
        .iter()
        .enumerate()
        .map(|(index, (id, _))| (id.as_str(), format!("player {} in voice", index + 1)))
        .collect();
    let probe = Arc::new(AudioProbe::default());
    let mut feeds = JoinSet::new();
    let exercise = async {
        let tones = &tones;
        let probe = &probe;
        let chat_start = Barrier::new(tones.len() + 1);
        let chat_start = &chat_start;
        let mut listeners = Vec::new();
        for (index, (call, (id, frequency))) in calls.iter_mut().zip(tones).enumerate() {
            let (room, events, source) = call.as_mut().map_err(|e| e.clone())?;
            let guest = index.checked_sub(1).map(|index| &guests[index]);
            let feed_source = source.clone();
            let feed_probe_state = probe.clone();
            let frequency = *frequency;
            // Give each synthetic source its own clock so the test models device callbacks.
            let runtime = tokio::runtime::Handle::current();
            feeds.spawn_blocking(move || {
                runtime.block_on(feed_probe(
                    &feed_source,
                    frequency,
                    index,
                    &feed_probe_state,
                ))
            });
            listeners.push(async move {
                let paths = hear_other_players(events, id, tones, chat_start, index, probe).await?;
                if let Some(guest) = guest {
                    verify_guest_media(room, guest).await?;
                }
                Ok::<_, String>(paths)
            });
        }
        let chat = async {
            chat_start.wait().await;
            probe.schedule();
            println!("measuring four active PCM senders for {OBSERVATION_MS} ms");
            let exchanges = lobbies
                .iter_mut()
                .zip(&messages)
                .map(|((ws, _), (id, text))| chat_in_call(ws, room_id, id, text, &messages));
            let contention = async {
                if std::env::var("DUIPIN_TEST_CPU_CONTENTION").as_deref() == Ok("1") {
                    let _workers = CpuContention::start();
                    tokio::time::sleep_until(probe.deadline()).await;
                }
                Ok(())
            };
            tokio::try_join!(try_join_all(exchanges), contention)
        };
        let verification = async {
            let (directions, _) = tokio::try_join!(try_join_all(listeners), chat)?;
            let (decoded, rendered): (Vec<_>, Vec<_>) = directions.into_iter().unzip();
            let decoded = decoded.into_iter().flatten().collect::<Vec<_>>();
            let rendered = rendered.into_iter().flatten().collect::<Vec<_>>();
            let minimum_delivered = OBSERVATION_MS as usize * 48 * 95 / 100;
            for (sender, (gap, capture)) in probe
                .max_input_gap_us
                .iter()
                .zip(&probe.max_capture_call_us)
                .enumerate()
            {
                println!(
                    "sender {} max PCM injection gap: {:.1} ms, max capture call: {:.1} ms, frames={}",
                    sender + 1,
                    gap.load(Ordering::Relaxed) as f64 / 1000.,
                    capture.load(Ordering::Relaxed) as f64 / 1000.,
                    probe.input_frames[sender].load(Ordering::Relaxed),
                );
            }
            let decoded = report_probe("decoded", &decoded, minimum_delivered);
            println!("mixed callback low-level runs describe the entire receiver mix");
            let rendered = report_probe("mixed callback", &rendered, minimum_delivered);
            decoded.and(rendered)
        };
        tokio::select! {
            result = feeds.join_next() => {
                result.ok_or("no PCM senders")?.map_err(err)??;
                Err("PCM sender stopped during verification".into())
            }
            result = verification => result,
        }
    };
    let mut result = timeout(Duration::from_secs(30), exercise)
        .await
        .unwrap_or_else(|_| Err("four-player media and chat timed out".into()));
    probe.stop.store(true, Ordering::Relaxed);
    while let Some(feed) = feeds.join_next().await {
        result = result.and(feed.map_err(err).and_then(|result| result));
    }
    join_all(
        calls
            .iter()
            .filter_map(|call| call.as_ref().ok().map(|(room, _, _)| room.close())),
    )
    .await;
    result?;
    let expected = if tunnel::expect_relay() {
        "relay"
    } else {
        "direct"
    };
    for (guest, (id, _)) in guests.iter().zip(tones.iter().skip(1)) {
        let actual = guest
            .selected_transport()
            .ok_or("iroh has no selected path")?;
        if actual != expected {
            return Err(format!(
                "guest {id}: expected iroh {expected}, selected {actual}"
            ));
        }
    }
    println!(
        "verified 12 decoded and mixed PCM paths and four-player chat through iroh {expected}"
    );
    Ok(())
}

async fn hear_other_players(
    events: &mut UnboundedReceiver<RoomEvent>,
    listener: &str,
    tones: &[(String, f64)],
    chat_start: &Barrier,
    receiver: usize,
    probe: &Arc<AudioProbe>,
) -> Result<(Vec<MeasuredPath>, Vec<MeasuredPath>)> {
    let mut pending: Vec<_> = tones
        .iter()
        .enumerate()
        .filter(|(_, (id, _))| id != listener)
        .collect();
    let mut tracks = Vec::new();
    while !pending.is_empty() {
        let event = timeout(Duration::from_secs(12), events.recv())
            .await
            .map_err(|_| format!("{listener} waiting for audio tracks from {pending:?}"))?
            .ok_or("subscriber event stream ended")?;
        match event {
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                participant,
                ..
            } => {
                if let Some(index) = pending
                    .iter()
                    .position(|(_, (id, _))| *id == participant.identity().as_str())
                {
                    let (sender, (id, frequency)) = pending.swap_remove(index);
                    tracks.push((track, id, *frequency, sender));
                }
            }
            RoomEvent::Disconnected { reason } => {
                return Err(format!("{listener} disconnected: {reason:?}"));
            }
            _ => {}
        }
    }
    try_join_all(tracks.iter().map(|(track, id, frequency, _)| async move {
        timeout(Duration::from_secs(8), verify_tone(track, *frequency))
            .await
            .map_err(|_| format!("{listener} did not hear {id} at {frequency} Hz"))?
    }))
    .await?;
    let baseline = if std::env::var("DUIPIN_TEST_RTC_STATS").as_deref() == Ok("1") {
        Some(
            try_join_all(
                tracks
                    .iter()
                    .map(|(track, _, _, _)| inbound_audio_stats(track)),
            )
            .await?,
        )
    } else {
        None
    };
    chat_start.wait().await;
    let playback = ScenarioPlayback::new(
        tracks
            .iter()
            .map(|(track, id, _, _)| (track.clone(), (*id).clone()))
            .collect(),
        48_000,
    );
    let senders = tracks
        .iter()
        .map(|(_, _, frequency, sender)| (*sender, *frequency))
        .collect();
    let decoded = try_join_all(tracks.iter().map(|(track, _, frequency, sender)| {
        measure_track(track, *frequency, *sender, receiver, probe)
    }));
    let rendered = measure_playout(playback, receiver, probe.clone(), senders);
    let paths = tokio::try_join!(decoded, rendered)?;
    if let Some(baseline) = baseline {
        let final_stats = try_join_all(
            tracks
                .iter()
                .map(|(track, _, _, _)| inbound_audio_stats(track)),
        )
        .await?;
        for (((_, _, _, sender), before), after) in tracks.iter().zip(baseline).zip(final_stats) {
            if before.stream.ssrc != after.stream.ssrc {
                return Err(format!(
                    "receive stream changed for {} -> {}",
                    sender + 1,
                    receiver + 1
                ));
            }
            let emitted = after
                .inbound
                .jitter_buffer_emitted_count
                .saturating_sub(before.inbound.jitter_buffer_emitted_count);
            let average_delay_ms = 1000.0
                * (after.inbound.jitter_buffer_delay - before.inbound.jitter_buffer_delay)
                / emitted as f64;
            println!(
                "RTP {} -> {}: lost={}, discarded={}, concealed={} samples ({} silent), mean jitter buffer={average_delay_ms:.1} ms ({emitted} emitted)",
                sender + 1,
                receiver + 1,
                after.received.packets_lost - before.received.packets_lost,
                after.inbound.packets_discarded.saturating_sub(before.inbound.packets_discarded),
                after.inbound.concealed_samples.saturating_sub(before.inbound.concealed_samples),
                after.inbound.silent_concealed_samples.saturating_sub(before.inbound.silent_concealed_samples),
            );
        }
    }
    Ok(paths)
}

async fn inbound_audio_stats(track: &RemoteAudioTrack) -> Result<InboundRtpStats> {
    timeout(Duration::from_secs(3), track.get_stats())
        .await
        .map_err(|_| "timed out reading inbound RTP stats")?
        .map_err(err)?
        .into_iter()
        .find_map(|stat| match stat {
            RtcStats::InboundRtp(stream) if stream.stream.kind == "audio" => Some(stream),
            _ => None,
        })
        .ok_or_else(|| "the subscribed audio track had no inbound RTP stats".into())
}

async fn chat_in_call(
    ws: &mut Socket,
    room: &str,
    sender: &str,
    text: &str,
    messages: &[(&str, String)],
) -> Result<()> {
    send(
        ws,
        json!({"type":"chat","roomId":room,"text":text,"requestId":1}),
    )
    .await?;
    let mut pending: Vec<_> = messages.iter().collect();
    let mut acknowledged = false;
    while !acknowledged || !pending.is_empty() {
        let next = event(ws, |event| {
            event["type"] == "snapshot" || event["requestId"] == 1
        })
        .await
        .map_err(|e| {
            format!("{sender} chat, acknowledged={acknowledged}, missing {pending:?}: {e}")
        })?;
        if next["requestId"] == 1 {
            if next["type"] != "result" || !next["error"].is_null() {
                return Err(format!("{sender} chat was not acknowledged: {next}"));
            }
            acknowledged = true;
        }
        if let Some(received) = next["messages"].as_array() {
            pending.retain(|(id, text)| {
                !received
                    .iter()
                    .any(|message| message["authorId"] == *id && message["text"] == *text)
            });
        }
    }
    Ok(())
}
