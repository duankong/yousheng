mod audio;
mod devices;
mod diagnostics;
#[cfg(test)]
mod scenario_playback;

pub(crate) use diagnostics::VoiceEvent;
#[cfg(test)]
pub(crate) use scenario_playback::ScenarioPlayback;

use futures_util::StreamExt;
use livekit::{
    prelude::*,
    webrtc::{
        audio_source::{native::NativeAudioSource, AudioSourceOptions},
        audio_stream::native::{NativeAudioStream, NativeAudioStreamOptions},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Emitter};
use tokio::{
    sync::{mpsc, watch, Mutex as AsyncMutex},
    task::JoinHandle,
};

use crate::diagnostics::Diagnostics;
use audio::{AudioIo, CaptureControl, PreparedAudioIo, BLOCK, RATE};
pub use devices::AudioDevices;
use diagnostics::{CallDiagnostics, ConnectingCall, PcmStats, VoiceState};

// Decoded WebRTC sink frames are typically 10 ms each.
const SDK_PCM_QUEUE_FRAMES: usize = 3;
const NORMAL_PLAYOUT_SAMPLES: usize = 4 * BLOCK;
const MIN_PCM_QUEUE_SAMPLES: usize = 6 * BLOCK;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AudioSettings {
    pub mode: String,
    pub muted: bool,
    pub threshold: u16,
    pub input_gain: u16,
    pub output_gain: u16,
    pub output_muted: bool,
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
            output_muted: false,
            noise_suppression: true,
            echo_cancellation: true,
            input_device: String::new(),
            output_device: String::new(),
            shortcut: "F8".into(),
        }
    }
}

impl AudioSettings {
    fn playback_gain(&self) -> f32 {
        if self.output_muted {
            0.0
        } else {
            f32::from(self.output_gain) / 100.0
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
    callback_samples: usize,
    pcm_stats: PcmStats,
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

    fn playout_limit(&self) -> usize {
        // Keep at most 40 ms on typical devices. A longer device callback
        // needs enough PCM for its full request plus one incoming sink frame.
        NORMAL_PLAYOUT_SAMPLES.max(self.callback_samples.saturating_add(BLOCK))
    }

    fn pcm_queue_capacity(&self) -> usize {
        MIN_PCM_QUEUE_SAMPLES.max(self.playout_limit())
    }

    fn mix_block(&mut self, output: &mut [f32], muted: bool, callback_samples: Option<usize>) {
        let limit = if let Some(callback_samples) = callback_samples {
            self.callback_samples = callback_samples;
            self.playout_limit()
        } else {
            usize::MAX
        };
        output.fill(0.0);
        for track in self.tracks.values_mut() {
            let stale = track.samples.len().saturating_sub(limit);
            let count = (track.samples.len() - stale).min(output.len());
            let queued = track.samples.drain(..stale + count);
            self.pcm_stats.trimmed_samples_total += stale as u64;
            if !muted && track.gain != 0.0 {
                self.pcm_stats.unfilled_track_samples_total += (output.len() - count) as u64;
                for (mixed, sample) in output.iter_mut().zip(queued.skip(stale)) {
                    *mixed += f32::from(sample) * track.gain;
                }
            }
        }
    }

    fn push_remote_pcm(&mut self, sid: &TrackSid, samples: &[i16]) -> bool {
        let limit = self.pcm_queue_capacity();
        let Some(track) = self.tracks.get_mut(sid) else {
            return false;
        };
        let skipped_input = samples.len().saturating_sub(limit);
        let samples = &samples[skipped_input..];
        let stale = track.samples.len().saturating_sub(limit - samples.len());
        track.samples.drain(..stale);
        track.samples.extend(samples.iter().copied());
        self.pcm_stats.trimmed_samples_total += (skipped_input + stale) as u64;
        self.pcm_stats.peak_track_queue_samples = self
            .pcm_stats
            .peak_track_queue_samples
            .max(track.samples.len());
        true
    }

    fn sample_pcm(&mut self) -> PcmStats {
        let depth = self
            .tracks
            .values()
            .map(|track| track.samples.len())
            .max()
            .unwrap_or(0);
        let mut sample = self.pcm_stats;
        sample.peak_track_queue_samples = sample.peak_track_queue_samples.max(depth);
        self.pcm_stats.peak_track_queue_samples = depth;
        sample
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
    room: Arc<Room>,
    source: NativeAudioSource,
    io: Arc<AsyncMutex<Option<AudioIo>>>,
    events: Task,
    diagnostics: CallDiagnostics,
}

fn call_is_active(state: ConnectionState) -> bool {
    state != ConnectionState::Disconnected
}

fn call_owns_input(state: ConnectionState, has_io: bool) -> bool {
    call_is_active(state) || has_io
}

#[derive(Default)]
struct Preview {
    requested: bool,
    input: Option<AudioIo>,
}

impl Preview {
    fn needs_input_change(&self, in_call: bool, input_changed: bool) -> bool {
        self.requested
            && !in_call
            && (input_changed || self.input.as_ref().is_some_and(AudioIo::failed))
    }
}

#[derive(Clone)]
struct AudioControls {
    app: AppHandle,
    preview: Arc<Mutex<Preview>>,
    settings: watch::Sender<AudioSettings>,
    capture_control: Arc<CaptureControl>,
    mix: Arc<Mutex<Mixer>>,
}

impl AudioControls {
    fn prepare_io(
        &self,
        settings: &AudioSettings,
        source: Option<&NativeAudioSource>,
    ) -> Result<PreparedAudioIo, String> {
        PreparedAudioIo::open(
            &self.app,
            settings,
            self.settings.subscribe(),
            self.capture_control.clone(),
            self.mix.clone(),
            source.cloned(),
        )
    }

    async fn replace_call_io(
        &self,
        current: &mut Option<AudioIo>,
        settings: &AudioSettings,
        room: &Room,
        source: &NativeAudioSource,
    ) -> Result<(), String> {
        // Keep a working device if the replacement format or endpoint fails to open.
        let replacement = self.prepare_io(settings, Some(source))?;
        if let Some(old) = current.take() {
            old.stop().await;
        }
        let preview_input = self.preview.lock().unwrap().input.take();
        if let Some(preview_input) = preview_input {
            preview_input.stop().await;
        }
        if !call_is_active(room.connection_state()) {
            return Ok(());
        }
        match replacement.start() {
            Ok(started) => {
                *current = Some(started);
                Ok(())
            }
            Err(error) => {
                status(&self.app, "device-error", Some(&error));
                Err(error)
            }
        }
    }

    fn resume_preview(&self) {
        let mut preview = self.preview.lock().unwrap();
        if preview.requested && preview.input.is_none() {
            let prefs = self.settings.borrow().clone();
            match self
                .prepare_io(&prefs, None)
                .and_then(PreparedAudioIo::start)
            {
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
    speaker_preview: AsyncMutex<()>,
}

impl VoiceEngine {
    pub fn new(app: AppHandle) -> Self {
        Self {
            session: AsyncMutex::new(None),
            audio: AudioControls {
                app,
                preview: Default::default(),
                settings: watch::channel(AudioSettings::default()).0,
                capture_control: Arc::new(CaptureControl::default()),
                mix: Default::default(),
            },
            speaker_preview: AsyncMutex::new(()),
        }
    }

    pub(crate) async fn connect(&self, url: &str, token: &str, diagnostics: &Diagnostics) {
        let mut slot = self.session.lock().await;
        if let Some(old) = slot.take() {
            self.stop(old).await;
        }
        self.set_ptt(false);
        let connecting = ConnectingCall::begin(diagnostics);
        status(&self.audio.app, "connecting", None);
        let joined = join_microphone(url, token).await;
        let diagnostics = connecting.finish();
        let (room, events, source) = match joined {
            Ok(joined) => joined,
            Err(error) => {
                diagnostics.status(VoiceState::ConnectFailed);
                silence(&self.audio.app);
                status(&self.audio.app, "error", Some(&error));
                return;
            }
        };
        self.audio.mix.lock().unwrap().pcm_stats = PcmStats::default();
        let settings = self.audio.settings.borrow().clone();
        let audio = self
            .audio
            .prepare_io(&settings, Some(&source))
            .and_then(|io| {
                self.audio.preview.lock().unwrap().input.take();
                io.start()
            });
        let audio = match audio {
            Ok(audio) => audio,
            Err(e) => {
                diagnostics.status(VoiceState::DeviceError);
                tokio::spawn(async move {
                    let _ = room.close().await;
                });
                silence(&self.audio.app);
                status(&self.audio.app, "device-error", Some(&e));
                self.audio.resume_preview();
                return;
            }
        };
        let io = Arc::new(AsyncMutex::new(Some(audio)));
        let room = Arc::new(room);
        diagnostics.status(VoiceState::Connected);
        status(&self.audio.app, "connected", None);
        let events = Task(tokio::spawn(room_events(
            self.audio.clone(),
            events,
            io.clone(),
            room.clone(),
            source.clone(),
            diagnostics.clone(),
        )));
        *slot = Some(Session {
            room,
            source,
            io,
            events,
            diagnostics,
        });
    }

    async fn stop(&self, session: Session) {
        // Close signaling in parallel with local audio cleanup.
        let close = tokio::spawn(async move {
            let _ = session.room.close().await;
        });
        session.events.0.abort();
        // Join before clearing the shared mixer so a stale subscription cannot reinsert a track.
        let mut events = session.events;
        let _ = (&mut events.0).await;
        let audio = session.io.lock().await.take();
        if let Some(audio) = audio {
            audio.stop().await;
        }
        self.audio.mix.lock().unwrap().tracks.clear();
        session.diagnostics.status(VoiceState::Disconnected);
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
        let call = match slot.as_ref() {
            Some(session) => Some((session, session.io.lock().await)),
            None => None,
        };
        let in_call = call.as_ref().is_some_and(|(session, io)| {
            call_owns_input(session.room.connection_state(), io.is_some())
        });
        let mut preview = self.audio.preview.lock().unwrap();
        preview.requested = active;
        self.audio.capture_control.set_monitoring(active);
        if !active || in_call {
            if preview.input.take().is_some() {
                let _ = self
                    .audio
                    .app
                    .emit("voice-level", serde_json::json!({"level":0}));
            }
        } else if preview.input.as_ref().is_none_or(AudioIo::failed) {
            let prefs = self.audio.settings.borrow().clone();
            let input = self.audio.prepare_io(&prefs, None)?;
            preview.input.take();
            preview.input = Some(input.start()?);
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
        let mut call = match slot.as_ref() {
            Some(session) => Some((session, session.io.lock().await)),
            None => None,
        };
        let in_call = if let Some((session, io)) = &mut call {
            if call_is_active(session.room.connection_state())
                && io.as_ref().is_none_or(|current| {
                    current.failed()
                        || old.input_device != settings.input_device
                        || old.output_device != settings.output_device
                })
            {
                self.audio
                    .replace_call_io(io, &settings, &session.room, &session.source)
                    .await
                    .inspect_err(|_| session.diagnostics.status(VoiceState::DeviceError))?;
                if io.as_ref().is_some_and(|current| !current.failed()) {
                    session.diagnostics.status(VoiceState::DeviceConfigured);
                }
                if session.room.connection_state() == ConnectionState::Connected {
                    status(&self.audio.app, "connected", None);
                }
            }
            call_owns_input(session.room.connection_state(), io.is_some())
        } else {
            false
        };
        let mut preview = self.audio.preview.lock().unwrap();
        if preview.needs_input_change(in_call, old.input_device != settings.input_device) {
            let input = self.audio.prepare_io(&settings, None)?;
            preview.input.take();
            preview.input = Some(input.start()?);
        }
        self.audio.capture_control.configure(
            &settings,
            old.mode != settings.mode || old.shortcut != settings.shortcut,
        );
        self.audio.settings.send_replace(settings);
        Ok(())
    }

    pub async fn preview_speaker_volume(&self, expected_gain: u16) -> Result<(), String> {
        let _preview = self.speaker_preview.lock().await;
        let (device, gain) = {
            let slot = self.session.lock().await;
            let settings = self.audio.settings.borrow().clone();
            let gain = settings.playback_gain();
            if gain == 0.0 || settings.output_gain != expected_gain {
                return Ok(());
            }
            if let Some(session) = slot.as_ref() {
                let io = session.io.lock().await;
                if io.as_ref().is_some_and(|io| io.preview_speaker(gain)) {
                    return Ok(());
                }
            }
            (settings.output_device, gain)
        };
        audio::speaker_preview::play_on_device(&device, self.audio.capture_control.clone(), gain)
            .await
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
        self.audio.capture_control.set_ptt(pressed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_call_cannot_recover_audio() {
        assert!(!call_is_active(ConnectionState::Disconnected));
        assert!(call_is_active(ConnectionState::Connected));
        assert!(call_is_active(ConnectionState::Reconnecting));
    }

    #[test]
    fn recoverable_call_reserves_input_until_call_audio_is_stopped() {
        assert!(call_owns_input(ConnectionState::Connected, false));
        assert!(call_owns_input(ConnectionState::Reconnecting, false));
        assert!(call_owns_input(ConnectionState::Disconnected, true));
        assert!(!call_owns_input(ConnectionState::Disconnected, false));
    }

    fn add_track(mix: &mut Mixer, id: &str, samples: VecDeque<i16>) -> TrackSid {
        let sid: TrackSid = format!("TR_{id}").try_into().unwrap();
        mix.tracks.insert(
            sid.clone(),
            VoiceTrack {
                player: id.into(),
                gain: 1.0,
                samples,
                _pump: Task(tokio::spawn(std::future::pending())),
            },
        );
        sid
    }

    fn played_sample(mix: &Mutex<Mixer>, master: f32) -> f32 {
        let mut sample = 0.0;
        audio::Playback::new(48_000).render(0..2, master, mix, |_, value| sample = value);
        sample
    }

    #[test]
    fn selecting_a_device_retries_a_missing_preview_only_while_it_is_requested() {
        let mut preview = Preview::default();
        assert!(!preview.needs_input_change(false, true));
        preview.requested = true;
        assert!(!preview.needs_input_change(true, true));
        assert!(!preview.needs_input_change(false, false));
        assert!(preview.needs_input_change(false, true));
    }

    #[tokio::test]
    async fn playout_discards_stale_pcm_before_reaching_the_speaker() {
        let mut mix = Mixer::default();
        let sid = add_track(&mut mix, "one", VecDeque::new());
        mix.push_remote_pcm(&sid, &[2048; 2880]);
        mix.push_remote_pcm(&sid, &[4096; 1920]);
        let mix = Mutex::new(mix);
        assert_eq!(played_sample(&mix, 1.0), 0.125);
        let mut mix = mix.lock().unwrap();
        let pcm = mix.sample_pcm();
        assert_eq!(pcm.peak_track_queue_samples, 2880);
        assert_eq!(pcm.trimmed_samples_total, 2880);
        assert_eq!(pcm.unfilled_track_samples_total, 0);
        assert!(mix.sample_pcm().peak_track_queue_samples < pcm.peak_track_queue_samples);
    }

    #[tokio::test]
    async fn playout_queue_adapts_to_the_device_callback_length() {
        let mix = Mutex::new(Mixer::default());
        let sid = add_track(&mut mix.lock().unwrap(), "one", VecDeque::new());
        let mut playback = audio::Playback::new(44_100);
        playback.render(0..2205, 1.0, &mix, |_, _| {});
        mix.lock().unwrap().push_remote_pcm(&sid, &[4096; 2400]);
        let mut played = 0.0;
        playback.render(0..2205, 1.0, &mix, |_, sample| played = sample);
        assert_eq!(played, 0.125);

        mix.lock().unwrap().push_remote_pcm(&sid, &[4096; 480]);
        mix.lock().unwrap().push_remote_pcm(&sid, &[8192; 1920]);
        assert_eq!(played_sample(&mix, 1.0), 0.25);
    }

    #[tokio::test]
    async fn playout_keeps_all_pcm_when_the_device_callback_gets_longer() {
        let mix = Mutex::new(Mixer::default());
        let sid = add_track(&mut mix.lock().unwrap(), "one", VecDeque::new());
        let mut playback = audio::Playback::new(44_100);
        playback.render(0..441, 1.0, &mix, |_, _| {});

        let levels = [2048, 4096, 6144, 8192, 10240];
        for level in levels {
            mix.lock().unwrap().push_remote_pcm(&sid, &[level; BLOCK]);
        }

        let mut played = [0.0; 2205];
        playback.render(0..played.len(), 1.0, &mix, |frame, sample| {
            played[frame] = sample;
        });
        for (block, level) in played.chunks_exact(441).zip(levels) {
            let expected = f32::from(level) / 32768.0;
            assert!(
                block.iter().all(|&sample| sample == expected),
                "expected a full block at {expected}, got {} to {}",
                block[0],
                block[440]
            );
        }
    }

    #[tokio::test]
    async fn resampled_playout_is_continuous_across_mix_blocks_and_callbacks() {
        let queued: VecDeque<i16> = (0..1600).map(|index| 2048 + index * 8).collect();
        let render = |callbacks: &[usize]| {
            let mix = Mutex::new(Mixer::default());
            add_track(&mut mix.lock().unwrap(), "one", queued.clone());
            let mut playback = audio::Playback::new(44_100);
            let mut played = Vec::new();
            for &frames in callbacks {
                playback.render(0..frames, 1.0, &mix, |_, sample| played.push(sample));
            }
            played
        };
        let played = render(&[143, 955, 233]);
        assert_eq!(played, render(&[1331]));
        assert_eq!(played[0], 0.0);
        assert_eq!(played[441], 5880.0 / 32768.0);
        assert_eq!(played[882], 9720.0 / 32768.0);
    }

    #[tokio::test]
    async fn mix_scales_and_mutes_each_listener_track_before_master_gain() {
        let mut mix = Mixer::default();
        for (id, sample) in [("one", 2048), ("two", 4096)] {
            add_track(&mut mix, id, VecDeque::from(vec![sample; 10]));
        }
        let mix = Mutex::new(mix);
        assert_eq!(played_sample(&mix, 1.0), 6144.0 / 32768.0);
        mix.lock().unwrap().set_peer_gain(
            "one".into(),
            PeerGain {
                volume: 200,
                muted: false,
            },
        );
        assert_eq!(played_sample(&mix, 1.0), 8192.0 / 32768.0);
        mix.lock().unwrap().set_peer_gain(
            "one".into(),
            PeerGain {
                volume: 200,
                muted: true,
            },
        );
        assert_eq!(played_sample(&mix, 1.0), 4096.0 / 32768.0);
        mix.lock().unwrap().set_peer_gain(
            "one".into(),
            PeerGain {
                volume: 200,
                muted: false,
            },
        );
        let mut prefs: AudioSettings = serde_json::from_value(serde_json::json!({
            "outputGain": 50
        }))
        .unwrap();
        assert!(!prefs.output_muted);
        prefs.output_muted = true;
        assert_eq!(played_sample(&mix, prefs.playback_gain()), 0.0);
        assert!(mix
            .lock()
            .unwrap()
            .tracks
            .values()
            .all(|track| track.samples.len() == 2));
        assert_eq!(serde_json::to_value(&prefs).unwrap()["outputMuted"], true);
        prefs.output_muted = false;
        assert_eq!(played_sample(&mix, prefs.playback_gain()), 4096.0 / 32768.0);
    }
}

fn attach_remote_track(mix: &Arc<Mutex<Mixer>>, track: RemoteAudioTrack, player: String) {
    let sid = track.sid();
    let mut stream = NativeAudioStream::with_options(
        track.rtc_track(),
        RATE as i32,
        1,
        NativeAudioStreamOptions {
            queue_size_frames: Some(SDK_PCM_QUEUE_FRAMES),
        },
    );
    let weak = Arc::downgrade(mix);
    let key = sid.clone();
    let capacity = mix.lock().unwrap().pcm_queue_capacity();
    let samples = VecDeque::with_capacity(capacity);
    let mut mix = mix.lock().unwrap();
    let gain = mix.gain(&player);
    let pump = Task(tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            let Some(mix) = weak.upgrade() else { break };
            if !mix.lock().unwrap().push_remote_pcm(&key, &frame.data) {
                break;
            }
        }
    }));
    mix.tracks.insert(
        sid,
        VoiceTrack {
            player,
            gain,
            samples,
            _pump: pump,
        },
    );
}

async fn room_events(
    audio: AudioControls,
    mut events: mpsc::UnboundedReceiver<RoomEvent>,
    io: Arc<AsyncMutex<Option<AudioIo>>>,
    room: Arc<Room>,
    source: NativeAudioSource,
    diagnostics: CallDiagnostics,
) {
    let mut reason = None;
    let app = &audio.app;
    let mix = &audio.mix;
    let sampler = Task(tokio::spawn(
        diagnostics.clone().sample(room.clone(), mix.clone()),
    ));
    let mut network_ready = true;
    let mut device_failed = false;
    let mut retry = tokio::time::interval(Duration::from_secs(2));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let event = tokio::select! {
            event = events.recv() => match event { Some(event) => event, None => break },
            _ = retry.tick() => {
                let mut current = io.lock().await;
                if call_is_active(room.connection_state()) && current.as_ref().is_none_or(AudioIo::failed) {
                    if !device_failed {
                        diagnostics.status(VoiceState::DeviceError);
                    }
                    device_failed = true;
                    let settings = audio.settings.borrow().clone();
                    if audio.replace_call_io(&mut current, &settings, &room, &source).await.is_ok() {
                        if network_ready && current.is_some() { status(app, "connected", None); }
                    }
                }
                if device_failed && current.as_ref().is_some_and(|audio| !audio.failed()) {
                    diagnostics.status(VoiceState::DeviceRecovered);
                    device_failed = false;
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
                attach_remote_track(mix, track, participant.identity().to_string());
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
                diagnostics.status(VoiceState::Reconnecting);
                silence(app);
                status(app, "reconnecting", None);
            }
            RoomEvent::Reconnected => {
                network_ready = true;
                diagnostics.status(VoiceState::Reconnected);
                if io
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(|audio| !audio.failed())
                {
                    status(app, "connected", None);
                } else {
                    if !device_failed {
                        diagnostics.status(VoiceState::DeviceError);
                    }
                    device_failed = true;
                    status(
                        app,
                        "device-error",
                        Some("请检查声音设置中的麦克风和播放设备"),
                    );
                }
            }
            RoomEvent::Disconnected { reason: why } => {
                reason = Some(format!("{why:?}"));
                break;
            }
            _ => (),
        }
    }
    drop(sampler);
    let mut current = io.lock().await;
    if let Some(current) = current.take() {
        current.stop().await;
    }
    mix.lock().unwrap().tracks.clear();
    diagnostics.status(VoiceState::Disconnected);
    silence(app);
    status(app, "disconnected", reason.as_deref());
    audio.resume_preview();
}
