mod audio;
mod devices;
mod diagnostics;
#[cfg(test)]
mod scenario_playback;

pub(crate) use diagnostics::VoiceEvent;
#[cfg(test)]
pub(crate) use scenario_playback::ScenarioPlayback;

use cpal::traits::StreamTrait;
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
    sync::{mpsc, oneshot, watch, Mutex as AsyncMutex, Notify},
    task::JoinHandle,
};

use crate::diagnostics::Diagnostics;
use audio::{AudioIo, CallAudio, CaptureControl, PreparedAudioIo, BLOCK, RATE};
pub use devices::AudioDevices;
use diagnostics::{CallDiagnostics, ConnectingCall, PcmStats, PlaybackTiming, VoiceState};

// Decoded WebRTC sink frames are typically 10 ms each.
const SDK_PCM_QUEUE_FRAMES: usize = 3;
const NORMAL_PLAYOUT_SAMPLES: usize = 4 * BLOCK;
const MIN_PCM_QUEUE_SAMPLES: usize = 6 * BLOCK;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    fn same_devices(&self, other: &Self) -> bool {
        self.input_device == other.input_device && self.output_device == other.output_device
    }

    fn playback_gain(&self) -> f32 {
        if self.output_muted {
            0.0
        } else {
            f32::from(self.output_gain) / 100.0
        }
    }

    fn speaker_preview_target(&self, expected_gain: u16) -> Option<(&str, f32)> {
        let gain = self.playback_gain();
        (gain != 0.0 && self.output_gain == expected_gain)
            .then_some((self.output_device.as_str(), gain))
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

fn preview_status(app: &AppHandle, error: &str) {
    let _ = app.emit("voice-preview-error", error);
}

fn silence(app: &AppHandle) {
    let _ = app.emit("speakers", Vec::<String>::new());
    let _ = app.emit("voice-level", serde_json::json!({"level":0}));
}

struct Session {
    room: Arc<Room>,
    devices: Arc<CallDevices>,
    configure: mpsc::Sender<ConfigureDevices>,
    stop_devices: watch::Sender<Option<StopDevices>>,
    recovery: JoinHandle<()>,
    events: Task,
    diagnostics: CallDiagnostics,
}

#[derive(Clone, Copy)]
enum StopDevices {
    Stop,
    ResumePreview,
}

struct ConfigureDevices {
    revision: u64,
    previous: AudioSettings,
    requested: AudioSettings,
    complete: oneshot::Sender<Result<(), String>>,
}

type ConfigureResult = oneshot::Receiver<Result<(), String>>;

#[derive(Default)]
struct CallDevices {
    current: Mutex<CurrentDevices>,
    changed: Arc<Notify>,
}

#[derive(Default)]
struct CurrentDevices {
    io: Option<AudioIo>,
    error: Option<String>,
    stopping: bool,
    pending: Option<u64>,
}

impl CurrentDevices {
    fn owns_input(&self) -> bool {
        self.io.is_some() || self.stopping
    }

    fn is_requested(&self, revision: Option<u64>) -> bool {
        self.pending == revision
    }

    fn take_io(&mut self) -> Option<AudioIo> {
        let old = self.io.take();
        self.stopping = old.is_some();
        old
    }

    fn queue_switch(
        &mut self,
        configure: &mpsc::Sender<ConfigureDevices>,
        previous: AudioSettings,
        requested: AudioSettings,
    ) -> Result<ConfigureResult, String> {
        let (complete, response) = oneshot::channel();
        let revision = self.pending.unwrap_or(0).wrapping_add(1);
        configure
            .try_send(ConfigureDevices {
                revision,
                previous,
                requested,
                complete,
            })
            .map_err(|_| "声音设备正在切换，请稍后重试".to_owned())?;
        self.pending = Some(revision);
        Ok(response)
    }
}

#[derive(PartialEq)]
enum DeviceReadiness {
    Starting,
    Ready,
    Failed(String),
}

impl CallDevices {
    fn readiness(&self) -> DeviceReadiness {
        let current = self.current.lock().unwrap();
        if let Some(error) = current.io.as_ref().and_then(AudioIo::failure) {
            DeviceReadiness::Failed(error)
        } else if current.io.as_ref().is_some_and(AudioIo::active) {
            DeviceReadiness::Ready
        } else if let Some(error) = &current.error {
            DeviceReadiness::Failed(error.clone())
        } else {
            DeviceReadiness::Starting
        }
    }

    fn stop(&self, stopped: &watch::Sender<Option<StopDevices>>, end: StopDevices) {
        let current = self.current.lock().unwrap();
        stopped.send_if_modified(|stop| {
            if stop.is_none()
                || matches!(
                    (*stop, end),
                    (Some(StopDevices::ResumePreview), StopDevices::Stop)
                )
            {
                *stop = Some(end);
                true
            } else {
                false
            }
        });
        if let Some(io) = current.io.as_ref() {
            io.deactivate();
        }
    }
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

struct SpeakerPreviewGuard<'a>(&'a Mutex<Option<cpal::Stream>>);

impl Drop for SpeakerPreviewGuard<'_> {
    fn drop(&mut self) {
        stop_speaker_preview(self.0);
    }
}

fn stop_speaker_preview(stream: &Mutex<Option<cpal::Stream>>) {
    let stopped = stream.lock().unwrap().take();
    drop(stopped);
}

struct UninstalledRoom<'a> {
    room: Option<Arc<Room>>,
    closing: &'a mut Option<ClosingRoom>,
}

impl Drop for UninstalledRoom<'_> {
    fn drop(&mut self) {
        if let Some(room) = self.room.take() {
            *self.closing = Some(ClosingRoom::new(room));
        }
    }
}

struct ClosingRoom {
    audio: Option<JoinHandle<()>>,
    signaling: JoinHandle<()>,
}

impl ClosingRoom {
    fn new(room: Arc<Room>) -> Self {
        Self {
            audio: None,
            signaling: tokio::spawn(async move {
                let _ = room.close().await;
            }),
        }
    }
}

async fn wait_for_audio_close(closing: &mut Option<ClosingRoom>) {
    if let Some(closing) = closing.as_mut() {
        if let Some(audio) = closing.audio.as_mut() {
            let _ = audio.await;
            closing.audio.take();
        }
    }
}

async fn wait_for_room_close(closing: &mut Option<ClosingRoom>) {
    if let Some(room) = closing.as_mut() {
        let _ = tokio::time::timeout(Duration::from_secs(4), &mut room.signaling).await;
        closing.take();
    }
}

#[derive(Clone)]
struct AudioControls {
    app: AppHandle,
    preview: Arc<Mutex<Preview>>,
    speaker_stream: Arc<Mutex<Option<cpal::Stream>>>,
    settings: watch::Sender<AudioSettings>,
    capture_control: Arc<CaptureControl>,
    mix: Arc<Mutex<Mixer>>,
}

impl AudioControls {
    fn prepare_io(
        &self,
        settings: &AudioSettings,
        call: Option<&CallAudio>,
        call_changed: Option<Arc<Notify>>,
    ) -> Result<PreparedAudioIo, String> {
        PreparedAudioIo::open(
            &self.app,
            settings,
            self.settings.subscribe(),
            self.capture_control.clone(),
            self.mix.clone(),
            call.cloned(),
            call_changed,
        )
    }

    fn apply_settings(&self, previous: &AudioSettings, settings: AudioSettings) {
        self.capture_control.configure(
            &settings,
            previous.mode != settings.mode || previous.shortcut != settings.shortcut,
        );
        self.settings.send_replace(settings);
    }

    fn take_preview_for_call(&self) -> Option<AudioIo> {
        stop_speaker_preview(&self.speaker_stream);
        self.preview.lock().unwrap().input.take()
    }

    fn resume_preview(&self) {
        let mut preview = self.preview.lock().unwrap();
        if preview.requested && preview.input.is_none() {
            let prefs = self.settings.borrow().clone();
            match self.prepare_preview(&prefs) {
                Ok(input) => self.install_preview(&mut preview, input),
                Err(e) => preview_status(&self.app, &e),
            }
        }
    }

    fn prepare_preview(&self, prefs: &AudioSettings) -> Result<PreparedAudioIo, String> {
        let input = self.prepare_io(prefs, None, None)?;
        input.start()?;
        Ok(input)
    }

    fn install_preview(&self, preview: &mut Preview, input: PreparedAudioIo) {
        drop(preview.input.take());
        preview_status(&self.app, "");
        preview.input = Some(input.activate_preview());
    }
}

struct DeviceRecovery {
    audio: AudioControls,
    devices: Arc<CallDevices>,
    room: Arc<Room>,
    call: CallAudio,
    diagnostics: CallDiagnostics,
}

impl DeviceRecovery {
    async fn run(
        self,
        mut requests: mpsc::Receiver<ConfigureDevices>,
        mut stopped: watch::Receiver<Option<StopDevices>>,
    ) {
        let mut retry = tokio::time::interval(Duration::from_secs(2));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while stopped.borrow().is_none() {
            let request = tokio::select! {
                biased;
                _ = stopped.changed() => break,
                request = requests.recv() => match request {
                    Some(request) => Some(request),
                    None => break,
                },
                _ = retry.tick() => None,
            };
            if request.is_none() && self.devices.readiness() == DeviceReadiness::Ready {
                continue;
            }
            let result = self.replace(request.as_ref(), &mut stopped).await;
            match (&result, &request) {
                (Ok(true), request) => {
                    preview_status(&self.audio.app, "");
                    if request.is_some() {
                        self.diagnostics.status(VoiceState::DeviceConfigured);
                    }
                }
                (Err(error), _) => {
                    let mut current = self.devices.current.lock().unwrap();
                    if current.io.as_ref().is_none_or(AudioIo::failed) {
                        current.error = Some(error.clone());
                    }
                }
                _ => {}
            }
            if let Some(request) = request {
                let mut current = self.devices.current.lock().unwrap();
                if current.is_requested(Some(request.revision)) {
                    current.pending = None;
                }
                let _ = request.complete.send(result.and_then(|installed| {
                    installed
                        .then_some(())
                        .ok_or_else(|| "声音设置变更已取消，请重试".to_owned())
                }));
            }
            self.devices.changed.notify_one();
        }
        let old = self.devices.current.lock().unwrap().take_io();
        self.stop_current(old).await;
        let resume_preview = matches!(*stopped.borrow(), Some(StopDevices::ResumePreview));
        if resume_preview {
            let _ = tokio::task::spawn_blocking(move || self.audio.resume_preview()).await;
        }
    }

    fn is_current(
        &self,
        request: Option<&ConfigureDevices>,
        opened_for: &AudioSettings,
        stopped: &watch::Receiver<Option<StopDevices>>,
        current: &CurrentDevices,
    ) -> bool {
        let settings = self.audio.settings.borrow();
        stopped.borrow().is_none()
            && call_is_active(self.room.connection_state())
            && current.is_requested(request.map(|request| request.revision))
            && request.map_or_else(
                || settings.same_devices(opened_for),
                |request| !request.complete.is_closed() && *settings == request.previous,
            )
    }

    fn can_replace(
        &self,
        request: Option<&ConfigureDevices>,
        settings: &AudioSettings,
        stopped: &watch::Receiver<Option<StopDevices>>,
    ) -> bool {
        self.is_current(
            request,
            settings,
            stopped,
            &self.devices.current.lock().unwrap(),
        )
    }

    async fn replace(
        &self,
        request: Option<&ConfigureDevices>,
        stopped: &mut watch::Receiver<Option<StopDevices>>,
    ) -> Result<bool, String> {
        let settings = request.map_or_else(
            || self.audio.settings.borrow().clone(),
            |request| request.requested.clone(),
        );
        if !self.can_replace(request, &settings, stopped) {
            return Ok(false);
        }
        let audio = self.audio.clone();
        let prefs = settings.clone();
        let call = self.call.clone();
        let changed = self.devices.changed.clone();
        let replacement = tokio::task::spawn_blocking(move || {
            audio.prepare_io(&prefs, Some(&call), Some(changed))
        })
        .await
        .map_err(|error| format!("声音设备准备失败：{error}"))?;
        let replacement = match replacement {
            Ok(replacement) => replacement,
            Err(error) if self.can_replace(request, &settings, stopped) => return Err(error),
            Err(_) => return Ok(false),
        };
        self.activate_if_current(request, &settings, stopped, replacement)
            .await
    }

    async fn activate_if_current(
        &self,
        request: Option<&ConfigureDevices>,
        settings: &AudioSettings,
        stopped: &mut watch::Receiver<Option<StopDevices>>,
        replacement: PreparedAudioIo,
    ) -> Result<bool, String> {
        let mut replacement = Some(replacement);
        let result = async {
            let opened = replacement.as_ref().unwrap();
            {
                let current = self.devices.current.lock().unwrap();
                if !self.is_current(request, settings, stopped, &current) {
                    return Ok(false);
                }
                opened.start()?;
            }
            tokio::select! {
                biased;
                _ = stopped.changed() => return Ok(false),
                ready = opened.ready() => ready?,
            }
            let old = {
                let mut current = self.devices.current.lock().unwrap();
                if !self.is_current(request, settings, stopped, &current) {
                    return Ok(false);
                }
                if let Some(error) = opened.failure() {
                    return Err(error);
                }
                current.take_io()
            };
            self.stop_current(old).await;
            if !self.can_replace(request, settings, stopped) {
                return Ok(false);
            }
            let audio = self.audio.clone();
            let preview = tokio::task::spawn_blocking(move || audio.take_preview_for_call()).await;
            if let Ok(Some(preview)) = preview {
                preview.stop().await;
            }
            let mut current = self.devices.current.lock().unwrap();
            if !self.is_current(request, settings, stopped, &current) {
                return Ok(false);
            }
            opened.activate_call(|| {
                if let Some(request) = request {
                    self.audio
                        .apply_settings(&request.previous, request.requested.clone());
                }
            })?;
            current.error = None;
            current.io = Some(replacement.take().unwrap().into_io());
            Ok(true)
        }
        .await;
        if let Some(replacement) = replacement {
            let _ = tokio::task::spawn_blocking(move || drop(replacement)).await;
        }
        result
    }

    async fn stop_current(&self, old: Option<AudioIo>) {
        if let Some(old) = old {
            old.stop().await;
            self.devices.current.lock().unwrap().stopping = false;
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
    transition: AsyncMutex<Option<ClosingRoom>>,
    audio: AudioControls,
    speaker_preview: AsyncMutex<()>,
}

impl VoiceEngine {
    pub fn new(app: AppHandle) -> Self {
        Self {
            session: AsyncMutex::new(None),
            transition: AsyncMutex::new(None),
            audio: AudioControls {
                app,
                preview: Default::default(),
                speaker_stream: Default::default(),
                settings: watch::channel(AudioSettings::default()).0,
                capture_control: Arc::new(CaptureControl::default()),
                mix: Default::default(),
            },
            speaker_preview: AsyncMutex::new(()),
        }
    }

    pub(crate) async fn connect(&self, url: &str, token: &str, diagnostics: &Diagnostics) {
        let mut closing = self.transition.lock().await;
        {
            let mut slot = self.session.lock().await;
            if let Some(old) = slot.take() {
                *closing = Some(self.stop(old));
            }
            wait_for_audio_close(&mut closing).await;
        }
        wait_for_room_close(&mut closing).await;
        self.set_ptt(false);
        let connecting = ConnectingCall::begin(diagnostics);
        status(&self.audio.app, "connecting", None);
        let (room, events, source) = match join_microphone(url, token).await {
            Ok(joined) => joined,
            Err(error) => {
                connecting.finish().status(VoiceState::ConnectFailed);
                silence(&self.audio.app);
                status(&self.audio.app, "error", Some(&error));
                return;
            }
        };
        let mut pending = UninstalledRoom {
            room: Some(Arc::new(room)),
            closing: &mut closing,
        };
        let mut slot = self.session.lock().await;
        let room = pending.room.take().unwrap();
        drop(pending);
        let diagnostics = connecting.finish();
        if !call_is_active(room.connection_state()) {
            diagnostics.status(VoiceState::Disconnected);
            silence(&self.audio.app);
            status(&self.audio.app, "disconnected", None);
            *closing = Some(ClosingRoom::new(room));
            return;
        }
        self.audio.mix.lock().unwrap().pcm_stats = PcmStats::default();
        let call = CallAudio {
            source,
            playback_timing: Arc::new(PlaybackTiming::default()),
        };
        let devices = Arc::new(CallDevices::default());
        let (configure, requests) = mpsc::channel(1);
        let (stop_devices, stopped) = watch::channel(None);
        let events = Task(tokio::spawn(room_events(
            self.audio.clone(),
            events,
            devices.clone(),
            stop_devices.clone(),
            room.clone(),
            call.clone(),
            diagnostics.clone(),
        )));
        let recovery = tokio::spawn(
            DeviceRecovery {
                audio: self.audio.clone(),
                devices: devices.clone(),
                room: room.clone(),
                call,
                diagnostics: diagnostics.clone(),
            }
            .run(requests, stopped),
        );
        *slot = Some(Session {
            room,
            devices,
            configure,
            stop_devices,
            recovery,
            events,
            diagnostics,
        });
    }

    fn stop(&self, session: Session) -> ClosingRoom {
        session
            .devices
            .stop(&session.stop_devices, StopDevices::Stop);
        let mut closing = ClosingRoom::new(session.room);
        session.events.0.abort();
        let audio = self.audio.clone();
        closing.audio = Some(tokio::spawn(async move {
            let mut events = session.events;
            let _ = (&mut events.0).await;
            audio.mix.lock().unwrap().tracks.clear();
            let _ = session.recovery.await;
            session.diagnostics.status(VoiceState::Disconnected);
            silence(&audio.app);
        }));
        closing
    }

    pub async fn disconnect(&self) {
        let mut closing = self.transition.lock().await;
        self.set_ptt(false);
        {
            let mut slot = self.session.lock().await;
            if let Some(session) = slot.take() {
                *closing = Some(self.stop(session));
            }
            wait_for_audio_close(&mut closing).await;
            status(&self.audio.app, "disconnected", None);
            self.audio.resume_preview();
        }
        wait_for_room_close(&mut closing).await;
    }

    pub async fn monitor(&self, active: bool) -> Result<(), String> {
        let slot = self.session.lock().await;
        let in_call = slot.as_ref().is_some_and(|session| {
            let current = session.devices.current.lock().unwrap();
            call_owns_input(session.room.connection_state(), current.owns_input())
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
            preview_status(&self.audio.app, "");
        } else if preview.input.as_ref().is_none_or(AudioIo::failed) {
            let prefs = self.audio.settings.borrow().clone();
            let input = self.audio.prepare_preview(&prefs)?;
            self.audio.install_preview(&mut preview, input);
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
        let response = if let Some(session) = slot.as_ref() {
            let mut current = session.devices.current.lock().unwrap();
            let old = self.audio.settings.borrow().clone();
            let network = session.room.connection_state();
            if call_is_active(network) && !old.same_devices(&settings) {
                Some(current.queue_switch(&session.configure, old, settings.clone())?)
            } else if call_owns_input(network, current.owns_input()) {
                self.audio.apply_settings(&old, settings);
                return Ok(());
            } else {
                None
            }
        } else {
            None
        };
        if let Some(response) = response {
            drop(slot);
            return response
                .await
                .map_err(|_| "语音已断开，请重试声音设置".to_owned())?;
        }
        let old = self.audio.settings.borrow().clone();
        let mut preview = self.audio.preview.lock().unwrap();
        let input = if preview.needs_input_change(false, old.input_device != settings.input_device)
        {
            Some(self.audio.prepare_preview(&settings)?)
        } else {
            None
        };
        if old.output_device != settings.output_device {
            stop_speaker_preview(&self.audio.speaker_stream);
        }
        self.audio.apply_settings(&old, settings);
        if let Some(input) = input {
            self.audio.install_preview(&mut preview, input);
        }
        Ok(())
    }

    pub async fn preview_speaker_volume(&self, expected_gain: u16) -> Result<(), String> {
        let Some((device, gain)) = self
            .audio
            .settings
            .borrow()
            .speaker_preview_target(expected_gain)
            .map(|(device, gain)| (device.to_owned(), gain))
        else {
            return Ok(());
        };
        let _preview = self.speaker_preview.lock().await;
        let mut prepared = None;
        let finished = loop {
            {
                let slot = self.session.lock().await;
                if self
                    .audio
                    .settings
                    .borrow()
                    .speaker_preview_target(expected_gain)
                    != Some((device.as_str(), gain))
                {
                    return Ok(());
                }
                let io = slot
                    .as_ref()
                    .map(|session| session.devices.current.lock().unwrap());
                if io.as_ref().is_some_and(|current| {
                    current
                        .io
                        .as_ref()
                        .is_some_and(|io| io.preview_speaker(gain))
                }) {
                    return Ok(());
                }
                if let Some(opened) = prepared.take() {
                    let (speaker, finished): (cpal::Stream, _) = opened?;
                    if let Err(error) = speaker.play() {
                        drop(io);
                        drop(slot);
                        return Err(format!("无法启动播放设备：{error}"));
                    }
                    *self.audio.speaker_stream.lock().unwrap() = Some(speaker);
                    break finished;
                }
            }
            let output_device = device.clone();
            let control = self.audio.capture_control.clone();
            prepared = Some(
                tokio::task::spawn_blocking(move || {
                    audio::speaker_preview::prepare_on_device(&output_device, control, gain)
                })
                .await
                .unwrap_or_else(|error| Err(format!("扬声器试听准备失败：{error}"))),
            );
        };
        let _speaker = SpeakerPreviewGuard(&self.audio.speaker_stream);
        let result = audio::speaker_preview::wait_until_played(finished).await;
        if self.audio.speaker_stream.lock().unwrap().is_none() {
            Ok(())
        } else {
            result
        }
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
    fn speaker_preview_requires_the_requested_output_and_live_gain() {
        let mut settings = AudioSettings {
            output_device: "headset".to_owned(),
            ..AudioSettings::default()
        };
        let (device, gain) = settings
            .speaker_preview_target(100)
            .map(|(device, gain)| (device.to_owned(), gain))
            .unwrap();
        assert_eq!((device.as_str(), gain), ("headset", 1.0));
        assert_eq!(settings.speaker_preview_target(120), None);
        settings.output_device = "speakers".to_owned();
        assert_ne!(
            settings.speaker_preview_target(100),
            Some((device.as_str(), gain))
        );
        settings.output_muted = true;
        assert_eq!(settings.speaker_preview_target(100), None);
        settings.output_muted = false;
        settings.output_gain = 0;
        assert_eq!(settings.speaker_preview_target(0), None);
    }

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

    #[tokio::test]
    async fn queued_device_switch_supersedes_a_delayed_retry() {
        let devices = Arc::new(CallDevices::default());
        let opening = devices.clone();
        let (started, wait_started) = oneshot::channel();
        let (opened, wait_opened) = oneshot::channel();
        let retry = tokio::spawn(async move {
            assert!(opening.current.lock().unwrap().is_requested(None));
            started.send(()).unwrap();
            wait_opened.await.unwrap();
            opening.current.lock().unwrap().is_requested(None)
        });
        wait_started.await.unwrap();

        let (configure, mut requests) = mpsc::channel(1);
        let requested = AudioSettings {
            input_device: "headset".into(),
            ..AudioSettings::default()
        };
        let response = devices
            .current
            .lock()
            .unwrap()
            .queue_switch(&configure, AudioSettings::default(), requested)
            .unwrap();
        opened.send(()).unwrap();
        assert!(!retry.await.unwrap());

        let request = requests.recv().await.unwrap();
        assert_eq!(request.requested.input_device, "headset");
        assert!(devices
            .current
            .lock()
            .unwrap()
            .is_requested(Some(request.revision)));
        request.complete.send(Ok(())).unwrap();
        assert_eq!(response.await.unwrap(), Ok(()));
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
    devices: Arc<CallDevices>,
    stop_devices: watch::Sender<Option<StopDevices>>,
    room: Arc<Room>,
    call: CallAudio,
    diagnostics: CallDiagnostics,
) {
    let mut reason = None;
    let app = &audio.app;
    let mix = &audio.mix;
    let sampler = Task(tokio::spawn(diagnostics.clone().sample(
        room.clone(),
        mix.clone(),
        call.playback_timing.clone(),
    )));
    let mut network_ready = true;
    let mut connected_once = false;
    let mut device_failed = false;
    let mut shown = Some(DeviceReadiness::Starting);
    loop {
        let event = tokio::select! {
            biased;
            event = events.recv() => match event { Some(event) => Some(event), None => break },
            _ = devices.changed.notified() => None,
        };
        match event {
            Some(RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                participant,
                ..
            }) => {
                attach_remote_track(mix, track, participant.identity().to_string());
                continue;
            }
            Some(RoomEvent::TrackUnsubscribed {
                track: RemoteTrack::Audio(track),
                ..
            }) => {
                mix.lock().unwrap().tracks.remove(&track.sid());
                continue;
            }
            Some(RoomEvent::ActiveSpeakersChanged { speakers }) => {
                let ids: Vec<_> = speakers
                    .iter()
                    .map(|speaker| speaker.identity().to_string())
                    .collect();
                let _ = app.emit("speakers", ids);
                continue;
            }
            Some(RoomEvent::Reconnecting) => {
                network_ready = false;
                diagnostics.status(VoiceState::Reconnecting);
                silence(app);
                status(app, "reconnecting", None);
                continue;
            }
            Some(RoomEvent::Reconnected) => {
                network_ready = true;
                diagnostics.status(VoiceState::Reconnected);
                shown = None;
            }
            Some(RoomEvent::Disconnected { reason: why }) => {
                reason = Some(format!("{why:?}"));
                break;
            }
            None => (),
            Some(_) => continue,
        }
        if !network_ready
            || stop_devices.borrow().is_some()
            || room.connection_state() != ConnectionState::Connected
        {
            continue;
        }
        let readiness = devices.readiness();
        if shown.as_ref() == Some(&readiness) {
            continue;
        }
        match &readiness {
            DeviceReadiness::Starting => {}
            DeviceReadiness::Ready => {
                if device_failed {
                    diagnostics.status(VoiceState::DeviceRecovered);
                }
                if !connected_once {
                    diagnostics.status(VoiceState::Connected);
                }
                connected_once = true;
                device_failed = false;
                status(app, "connected", None);
            }
            DeviceReadiness::Failed(error) => {
                if !device_failed {
                    diagnostics.status(VoiceState::DeviceError);
                }
                device_failed = true;
                status(app, "device-error", Some(error));
            }
        }
        shown = Some(readiness);
    }
    drop(sampler);
    devices.stop(&stop_devices, StopDevices::ResumePreview);
    mix.lock().unwrap().tracks.clear();
    diagnostics.status(VoiceState::Disconnected);
    silence(app);
    status(app, "disconnected", reason.as_deref());
}
