mod audio;
mod devices;

use futures_util::StreamExt;
use livekit::{
    prelude::*,
    webrtc::{
        audio_source::{native::NativeAudioSource, AudioSourceOptions},
        audio_stream::native::NativeAudioStream,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::{AppHandle, Emitter};
use tokio::{
    sync::{mpsc, watch, Mutex as AsyncMutex},
    task::JoinHandle,
};

use audio::AudioIo;
pub use devices::AudioDevices;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AudioSettings {
    pub mode: String,
    pub muted: bool,
    pub threshold: u16,
    pub input_gain: u16,
    pub output_gain: u16,
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    pub input_device: String,
    pub output_device: String,
    pub shortcut: String,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            mode: "open".into(),
            muted: false,
            threshold: 40,
            input_gain: 100,
            output_gain: 100,
            noise_suppression: true,
            echo_cancellation: true,
            input_device: String::new(),
            output_device: String::new(),
            shortcut: "F8".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerGain {
    pub volume: u16,
    pub muted: bool,
}

impl Default for PeerGain {
    fn default() -> Self {
        Self {
            volume: 100,
            muted: false,
        }
    }
}

struct Task(JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct VoiceTrack {
    player: String,
    gain: f32,
    samples: VecDeque<i16>,
    _pump: Task,
}

#[derive(Default)]
struct Mixer {
    tracks: HashMap<TrackSid, VoiceTrack>,
    peers: HashMap<String, PeerGain>,
}

impl Mixer {
    fn gain(&self, player: &str) -> f32 {
        let pref = self.peers.get(player).copied().unwrap_or_default();
        if pref.muted {
            0.0
        } else {
            f32::from(pref.volume) / 100.0
        }
    }

    fn set_peer_gain(&mut self, id: String, gain: PeerGain) {
        self.peers.insert(id.clone(), gain);
        let scale = self.gain(&id);
        for track in self.tracks.values_mut().filter(|track| track.player == id) {
            track.gain = scale;
        }
    }

    fn next_sample(&mut self, master: f32) -> f32 {
        let sum: f32 = self
            .tracks
            .values_mut()
            .map(|track| f32::from(track.samples.pop_front().unwrap_or_default()) * track.gain)
            .sum();
        (sum * master / 32768.0).clamp(-1.0, 1.0)
    }
}

fn status(app: &AppHandle, state: &str, message: Option<&str>) {
    let _ = app.emit(
        "voice-status",
        serde_json::json!({ "state":state, "message":message }),
    );
}

fn silence(app: &AppHandle) {
    let _ = app.emit("speakers", Vec::<String>::new());
    let _ = app.emit("voice-level", serde_json::json!({"level":0}));
}

struct Session {
    room: Room,
    source: NativeAudioSource,
    io: Arc<Mutex<Option<AudioIo>>>,
    events: Task,
}

#[derive(Default)]
struct Preview {
    requested: bool,
    input: Option<AudioIo>,
}

#[derive(Clone)]
struct AudioControls {
    app: AppHandle,
    preview: Arc<Mutex<Preview>>,
    settings: watch::Sender<AudioSettings>,
    pressed: Arc<AtomicBool>,
    mix: Arc<Mutex<Mixer>>,
}

impl AudioControls {
    fn open_io(
        &self,
        settings: &AudioSettings,
        source: Option<&NativeAudioSource>,
    ) -> Result<AudioIo, String> {
        AudioIo::open(
            &self.app,
            settings,
            self.settings.subscribe(),
            self.pressed.clone(),
            self.mix.clone(),
            source.cloned(),
        )
    }

    fn resume_preview(&self) {
        let mut preview = self.preview.lock().unwrap();
        if preview.requested && preview.input.is_none() {
            let prefs = self.settings.borrow().clone();
            match self.open_io(&prefs, None) {
                Ok(input) => preview.input = Some(input),
                Err(e) => status(&self.app, "device-error", Some(&e)),
            }
        }
    }
}

pub(crate) async fn join_microphone(
    url: &str,
    token: &str,
) -> Result<(Room, mpsc::UnboundedReceiver<RoomEvent>, NativeAudioSource), String> {
    // The local signaling bridge handles the dual-PC WebSocket protocol.
    let mut options = RoomOptions::default();
    options.single_peer_connection = false;
    options.join_retries = 1;
    // WASAPI's actual, post-gain speaker mix is supplied to our WebRTC APM.
    // Enabling the SDK's automatic ADM as well would play the peers twice.
    let source = NativeAudioSource::new(AudioSourceOptions::default(), 48_000, 1, 0);
    let track =
        LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(source.clone()));
    let publish = TrackPublishOptions {
        source: TrackSource::Microphone,
        dtx: true,
        ..Default::default()
    };
    options
        .publish_tracks
        .push((LocalTrack::Audio(track), publish));
    let (room, events) = Room::connect(url, token, options)
        .await
        .map_err(|e| e.to_string())?;
    Ok((room, events, source))
}

pub struct VoiceEngine {
    session: AsyncMutex<Option<Session>>,
    audio: AudioControls,
}

impl VoiceEngine {
    pub fn new(app: AppHandle) -> Self {
        Self {
            session: AsyncMutex::new(None),
            audio: AudioControls {
                app,
                preview: Default::default(),
                settings: watch::channel(AudioSettings::default()).0,
                pressed: Arc::new(AtomicBool::new(false)),
                mix: Default::default(),
            },
        }
    }

    pub async fn connect(&self, url: &str, token: &str) {
        let mut slot = self.session.lock().await;
        if let Some(old) = slot.take() {
            self.stop(old).await;
        }
        self.set_ptt(false);
        status(&self.audio.app, "connecting", None);
        let (room, events, source) = match join_microphone(url, token).await {
            Ok(joined) => joined,
            Err(error) => {
                silence(&self.audio.app);
                status(&self.audio.app, "error", Some(&error));
                return;
            }
        };
        let settings = self.audio.settings.borrow().clone();
        let audio = match self.audio.open_io(&settings, Some(&source)) {
            Ok(audio) => audio,
            Err(e) => {
                tokio::spawn(async move {
                    let _ = room.close().await;
                });
                silence(&self.audio.app);
                status(&self.audio.app, "device-error", Some(&e));
                return;
            }
        };
        self.audio.preview.lock().unwrap().input.take();
        let io = Arc::new(Mutex::new(Some(audio)));
        status(&self.audio.app, "connected", None);
        let events = Task(tokio::spawn(room_events(
            self.audio.clone(),
            events,
            io.clone(),
            source.clone(),
        )));
        *slot = Some(Session {
            room,
            source,
            io,
            events,
        });
    }

    async fn stop(&self, session: Session) {
        // Closing continues if a newer invitation cancels this command.
        let close = tokio::spawn(async move {
            let _ = session.room.close().await;
        });
        session.events.0.abort();
        // Join before clearing the shared mixer so a stale subscription cannot reinsert a track.
        let mut events = session.events;
        let _ = (&mut events.0).await;
        session.io.lock().unwrap().take();
        self.audio.mix.lock().unwrap().tracks.clear();
        silence(&self.audio.app);
        let _ = tokio::time::timeout(Duration::from_secs(4), close).await;
    }

    pub async fn disconnect(&self) {
        let mut slot = self.session.lock().await;
        self.set_ptt(false);
        if let Some(session) = slot.take() {
            self.stop(session).await;
        }
        status(&self.audio.app, "disconnected", None);
        self.audio.resume_preview();
    }

    pub async fn monitor(&self, active: bool) -> Result<(), String> {
        let slot = self.session.lock().await;
        let in_call = slot
            .as_ref()
            .is_some_and(|s| s.io.lock().unwrap().is_some());
        let mut preview = self.audio.preview.lock().unwrap();
        preview.requested = active;
        if !active || in_call {
            if preview.input.take().is_some() {
                let _ = self
                    .audio
                    .app
                    .emit("voice-level", serde_json::json!({"level":0}));
            }
        } else if preview.input.as_ref().is_none_or(AudioIo::failed) {
            let prefs = self.audio.settings.borrow().clone();
            preview.input = Some(self.audio.open_io(&prefs, None)?);
        }
        Ok(())
    }

    pub async fn configure(&self, settings: AudioSettings) -> Result<(), String> {
        if !matches!(settings.mode.as_str(), "open" | "activation" | "ptt")
            || settings.threshold > 100
            || settings.input_gain > 200
            || settings.output_gain > 150
        {
            return Err("声音设置超出范围".into());
        }
        let slot = self.session.lock().await;
        let old = self.audio.settings.borrow().clone();
        if let Some(session) = slot.as_ref() {
            let mut io = session.io.lock().unwrap();
            if io.as_ref().is_some_and(|current| {
                current.failed()
                    || old.input_device != settings.input_device
                    || old.output_device != settings.output_device
            }) {
                let replacement = self.audio.open_io(&settings, Some(&session.source))?;
                *io = Some(replacement);
                if session.room.connection_state() == ConnectionState::Connected {
                    status(&self.audio.app, "connected", None);
                }
            }
        }
        let mut preview = self.audio.preview.lock().unwrap();
        if preview
            .input
            .as_ref()
            .is_some_and(|input| input.failed() || old.input_device != settings.input_device)
        {
            preview.input = Some(self.audio.open_io(&settings, None)?);
        }
        if old.mode != settings.mode || old.shortcut != settings.shortcut {
            self.set_ptt(false);
        }
        self.audio.settings.send_replace(settings);
        Ok(())
    }

    pub async fn devices(&self) -> Result<AudioDevices, String> {
        devices::enumerate()
    }

    pub async fn set_peer_gain(&self, id: String, gain: PeerGain) -> Result<(), String> {
        if gain.volume > 200 {
            return Err("队友音量超出范围".into());
        }
        self.audio.mix.lock().unwrap().set_peer_gain(id, gain);
        Ok(())
    }

    pub fn set_ptt(&self, pressed: bool) {
        self.audio.pressed.store(pressed, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mix_scales_and_mutes_each_listener_track_before_master_gain() {
        let mut mix = Mixer::default();
        for (id, sample) in [("one", 2048), ("two", 4096)] {
            mix.tracks.insert(
                format!("TR_{id}").try_into().unwrap(),
                VoiceTrack {
                    player: id.into(),
                    gain: 1.0,
                    samples: VecDeque::from(vec![sample; 4]),
                    _pump: Task(tokio::spawn(std::future::pending())),
                },
            );
        }
        assert_eq!(mix.next_sample(1.0), 6144.0 / 32768.0);
        mix.set_peer_gain(
            "one".into(),
            PeerGain {
                volume: 200,
                muted: false,
            },
        );
        assert_eq!(mix.next_sample(1.0), 8192.0 / 32768.0);
        mix.set_peer_gain(
            "one".into(),
            PeerGain {
                volume: 200,
                muted: true,
            },
        );
        assert_eq!(mix.next_sample(1.0), 4096.0 / 32768.0);
        mix.set_peer_gain(
            "one".into(),
            PeerGain {
                volume: 200,
                muted: false,
            },
        );
        assert_eq!(mix.next_sample(0.5), 4096.0 / 32768.0);
    }
}

async fn room_events(
    audio: AudioControls,
    mut events: mpsc::UnboundedReceiver<RoomEvent>,
    io: Arc<Mutex<Option<AudioIo>>>,
    source: NativeAudioSource,
) {
    let mut reason = None;
    let app = &audio.app;
    let mix = &audio.mix;
    let mut network_ready = true;
    let mut retry = tokio::time::interval(Duration::from_secs(2));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let event = tokio::select! {
            event = events.recv() => match event { Some(event) => event, None => break },
            _ = retry.tick() => {
                let mut current = io.lock().unwrap();
                if current.as_ref().is_some_and(AudioIo::failed) {
                    let settings = audio.settings.borrow().clone();
                    if let Ok(replacement) = audio.open_io(&settings, Some(&source)) {
                        *current = Some(replacement);
                        if network_ready { status(app, "connected", None); }
                    }
                }
                continue;
            }
        };
        match event {
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                participant,
                ..
            } => {
                let player = participant.identity().to_string();
                let sid = track.sid();
                let mut stream = NativeAudioStream::new(track.rtc_track(), 48_000, 1);
                let weak = Arc::downgrade(mix);
                let key = sid.clone();
                let mut mix = mix.lock().unwrap();
                let gain = mix.gain(&player);
                let pump = Task(tokio::spawn(async move {
                    while let Some(frame) = stream.next().await {
                        let Some(mix) = weak.upgrade() else { break };
                        let mut mix = mix.lock().unwrap();
                        let Some(track) = mix.tracks.get_mut(&key) else {
                            break;
                        };
                        let limit = 48_000 / 10; // Bound extra playout delay to 100 ms.
                        let excess = (track.samples.len() + frame.data.len()).saturating_sub(limit);
                        let remove = excess.min(track.samples.len());
                        track.samples.drain(..remove);
                        track
                            .samples
                            .extend(frame.data.iter().skip(excess - remove).copied());
                    }
                }));
                mix.tracks.insert(
                    sid,
                    VoiceTrack {
                        player,
                        gain,
                        samples: VecDeque::with_capacity(4800),
                        _pump: pump,
                    },
                );
            }
            RoomEvent::TrackUnsubscribed {
                track: RemoteTrack::Audio(track),
                ..
            } => {
                mix.lock().unwrap().tracks.remove(&track.sid());
            }
            RoomEvent::ActiveSpeakersChanged { speakers } => {
                let ids: Vec<_> = speakers
                    .iter()
                    .map(|speaker| speaker.identity().to_string())
                    .collect();
                let _ = app.emit("speakers", ids);
            }
            RoomEvent::Reconnecting => {
                network_ready = false;
                silence(app);
                status(app, "reconnecting", None);
            }
            RoomEvent::Reconnected => {
                network_ready = true;
                if io
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|audio| !audio.failed())
                {
                    status(app, "connected", None);
                } else {
                    status(app, "device-error", Some("请检查声音设置中的麦克风和播放设备"));
                }
            }
            RoomEvent::Disconnected { reason: why } => {
                reason = Some(format!("{why:?}"));
                break;
            }
            _ => (),
        }
    }
    io.lock().unwrap().take();
    mix.lock().unwrap().tracks.clear();
    silence(app);
    status(app, "disconnected", reason.as_deref());
    audio.resume_preview();
}
