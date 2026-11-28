use std::{
    borrow::Cow,
    future::poll_fn,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::Poll,
    time::{Duration, Instant},
};

use super::{devices, status, AudioSettings, Mixer, Task};
use cpal::{
    traits::{DeviceTrait, StreamTrait},
    FromSample, Sample, SampleFormat, SizedSample,
};
use crossbeam_queue::ArrayQueue;
use futures_util::task::AtomicWaker;
use livekit::webrtc::{
    audio_frame::AudioFrame, audio_source::native::NativeAudioSource,
    native::apm::AudioProcessingModule,
};
use tauri::{AppHandle, Emitter};
use tokio::sync::watch;

use super::diagnostics::{MixerTiming, PlaybackTiming};
use speaker_preview::SpeakerTone;

pub(super) const RATE: u32 = 48_000;
pub(super) const BLOCK: usize = 480;

const CAPTURE_QUEUE_BLOCKS: usize = 5;
const CAPTURE_MAX_BACKLOG: Duration = Duration::from_millis(50);
const REFERENCE_QUEUE_BLOCKS: usize = 8;

const MUTE: u64 = 1;
const PTT_MODE: u64 = 2;
const PTT_PRESSED: u64 = 4;
const FLAG_BITS: u32 = 3;
const FLAGS: u64 = MUTE | PTT_MODE | PTT_PRESSED;

pub(super) struct CaptureControl {
    started_at: Instant,
    // The low bits hold the current controls; the high bits hold the last open edge in ns.
    state: AtomicU64,
    monitor: AtomicBool,
    echo: AtomicBool,
    playback_gain: AtomicU32,
}

impl Default for CaptureControl {
    fn default() -> Self {
        let prefs = AudioSettings::default();
        Self {
            started_at: Instant::now(),
            state: AtomicU64::new(0),
            monitor: AtomicBool::new(false),
            echo: AtomicBool::new(prefs.echo_cancellation),
            playback_gain: AtomicU32::new(prefs.playback_gain().to_bits()),
        }
    }
}

impl CaptureControl {
    fn closed(state: u64) -> bool {
        state & MUTE != 0 || state & (PTT_MODE | PTT_PRESSED) == PTT_MODE
    }

    fn update(&self, mask: u64, flags: u64, at: Instant) {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            let next = (state & !mask) | flags;
            let next = if Self::closed(state) && !Self::closed(next) {
                let nanos = at.saturating_duration_since(self.started_at).as_nanos() as u64;
                let opened_at = (state >> FLAG_BITS).max(nanos);
                (opened_at << FLAG_BITS) | (next & FLAGS)
            } else {
                next
            };
            if next == state {
                return;
            }
            match self.state.compare_exchange_weak(
                state,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(current) => state = current,
            }
        }
    }

    pub(super) fn configure(&self, prefs: &AudioSettings, reset_ptt: bool) {
        let mut mask = MUTE | PTT_MODE;
        if reset_ptt {
            mask |= PTT_PRESSED;
        }
        let flags = (u64::from(prefs.muted) * MUTE) | (u64::from(prefs.mode == "ptt") * PTT_MODE);
        self.update(mask, flags, Instant::now());
        self.echo.store(prefs.echo_cancellation, Ordering::Relaxed);
        self.playback_gain
            .store(prefs.playback_gain().to_bits(), Ordering::Relaxed);
    }

    pub(super) fn set_ptt(&self, pressed: bool) {
        self.update(
            PTT_PRESSED,
            u64::from(pressed) * PTT_PRESSED,
            Instant::now(),
        );
    }

    pub(super) fn set_monitoring(&self, active: bool) {
        self.monitor.store(active, Ordering::Relaxed);
    }

    fn playback_gain(&self) -> f32 {
        f32::from_bits(self.playback_gain.load(Ordering::Relaxed))
    }

    fn blocked_prefix(&self, block: &CaptureBlock, now: Instant) -> usize {
        let state = self.state.load(Ordering::Relaxed);
        if Self::closed(state)
            || now.saturating_duration_since(block.ready_at) > CAPTURE_MAX_BACKLOG
        {
            return BLOCK;
        }
        let opened_at = self.started_at + Duration::from_nanos(state >> FLAG_BITS);
        let before_open = opened_at.saturating_duration_since(block.captured_at);
        (before_open.as_nanos() * u128::from(RATE))
            .div_ceil(1_000_000_000)
            .min(BLOCK as u128) as usize
    }
}

struct CaptureBlock {
    samples: [i16; BLOCK],
    captured_at: Instant,
    ready_at: Instant,
    #[cfg(test)]
    timing: (Instant, Instant),
}

struct CaptureQueue {
    blocks: ArrayQueue<CaptureBlock>,
    ready: AtomicWaker,
}

impl CaptureQueue {
    fn new() -> Self {
        Self {
            blocks: ArrayQueue::new(CAPTURE_QUEUE_BLOCKS),
            ready: AtomicWaker::new(),
        }
    }

    fn push(&self, block: CaptureBlock) {
        self.blocks.force_push(block);
        self.ready.wake();
    }

    async fn recv(&self) -> CaptureBlock {
        tokio::task::consume_budget().await;
        poll_fn(|context| {
            if let Some(block) = self.blocks.pop() {
                return Poll::Ready(block);
            }
            self.ready.register(context.waker());
            // A push between the first pop and registration had no waker to notify.
            self.blocks.pop().map_or(Poll::Pending, Poll::Ready)
        })
        .await
    }
}

struct ReferenceBlock {
    samples: [i16; BLOCK],
    played_at: Instant,
}

// Both hardware callbacks deliver actual samples at the device's native rate.
// The phase is carried across callbacks, including for 44.1 kHz devices.
struct Framer {
    rate: u32,
    phase: u32,
    previous: f32,
    filled: usize,
    samples: [i16; BLOCK],
    first_sample: Instant,
}

impl Framer {
    fn new(rate: u32) -> Self {
        Self {
            rate,
            phase: 0,
            previous: 0.0,
            filled: 0,
            samples: [0; BLOCK],
            first_sample: Instant::now(),
        }
    }

    fn reset(&mut self) {
        self.phase = 0;
        self.filled = 0;
        self.previous = 0.0;
    }

    fn push(
        &mut self,
        sample: f32,
        stamp: impl Fn(u32) -> Instant,
        mut emit: impl FnMut(&[i16; BLOCK], Instant, u32),
    ) {
        self.phase += RATE;
        while self.phase >= self.rate {
            self.phase -= self.rate;
            let value = sample + (self.previous - sample) * (self.phase as f32 / RATE as f32);
            if self.filled == 0 {
                self.first_sample = stamp(self.phase);
            }
            self.samples[self.filled] = (value * 32768.0) as i16;
            self.filled += 1;
            if self.filled == BLOCK {
                emit(&self.samples, self.first_sample, self.phase);
                self.filled = 0;
            }
        }
        self.previous = sample;
    }
}

fn sample_time(packet_start: Instant, index: usize, phase: u32, device_rate: u64) -> Instant {
    let offset = Duration::from_nanos(index as u64 * 1_000_000_000 / device_rate);
    let interpolation =
        Duration::from_nanos(u64::from(phase) * 1_000_000_000 / (u64::from(RATE) * device_rate));
    (packet_start + offset)
        .checked_sub(interpolation)
        .unwrap_or(packet_start)
}

fn capture_packet_start(
    callback_at: Instant,
    timestamp: cpal::InputStreamTimestamp,
    packet_frames: usize,
    device_rate: u64,
) -> (Instant, bool) {
    let packet_duration =
        Duration::from_nanos((packet_frames as u64 * 1_000_000_000).div_ceil(device_rate));
    // WASAPI uses QPC time. A zero capture stamp can be left by a failed timestamp read.
    let reported_delay = if cfg!(windows) && timestamp.capture == cpal::StreamInstant::new(0, 0) {
        None
    } else {
        timestamp.callback.duration_since(&timestamp.capture)
    };
    let measured_start = reported_delay
        .filter(|&delay| delay >= packet_duration)
        .and_then(|delay| callback_at.checked_sub(delay));
    (
        measured_start.unwrap_or_else(|| callback_at - packet_duration),
        measured_start.is_some(),
    )
}

fn capture_ready_at(
    lowest_device_delay: &mut Option<Duration>,
    callback_at: Instant,
    last_sample_at: Option<Instant>,
) -> Instant {
    let Some(last_sample_at) = last_sample_at else {
        return callback_at;
    };
    // Count extra device backlog and time spent in our queue, excluding the device's fixed delay.
    let delay = callback_at.saturating_duration_since(last_sample_at);
    let baseline = lowest_device_delay.get_or_insert(delay);
    *baseline = (*baseline).min(delay);
    callback_at - (delay - *baseline)
}

type ErrorCallback = Box<dyn FnMut(cpal::StreamError) + Send>;

fn error_handler(app: &AppHandle, failed: &Arc<AtomicBool>, device: &'static str) -> ErrorCallback {
    let app = app.clone();
    let failed = failed.clone();
    Box::new(move |e| {
        if !failed.swap(true, Ordering::Relaxed) {
            status(
                &app,
                "device-error",
                Some(&format!("{device}已停止：{e}，请在声音设置中重新选择设备")),
            );
        }
    })
}

fn capture<T: SizedSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    input: Arc<CaptureQueue>,
    err: ErrorCallback,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let device_rate = u64::from(config.sample_rate.0);
    let mut frames = Framer::new(config.sample_rate.0);
    let mut lowest_device_delay = None;
    let mut priority = devices::CallbackPriority::default();
    device.build_input_stream(
        config,
        move |samples: &[T], info| {
            priority.enter();
            let callback_at = Instant::now();
            let frames_in_packet = samples.chunks_exact(channels);
            let (packet_start, measured_timing) = capture_packet_start(
                callback_at,
                info.timestamp(),
                frames_in_packet.len(),
                device_rate,
            );
            for (index, frame) in frames_in_packet.enumerate() {
                let sum = frame
                    .iter()
                    .map(|sample| f32::from_sample(*sample))
                    .sum::<f32>();
                frames.push(
                    sum / channels as f32,
                    |phase| sample_time(packet_start, index, phase, device_rate),
                    |samples, captured_at, last_phase| {
                        let last_sample_at =
                            sample_time(packet_start, index, last_phase, device_rate);
                        input.push(CaptureBlock {
                            samples: *samples,
                            captured_at,
                            ready_at: capture_ready_at(
                                &mut lowest_device_delay,
                                callback_at,
                                measured_timing.then_some(last_sample_at),
                            ),
                            #[cfg(test)]
                            timing: (callback_at, last_sample_at),
                        });
                    },
                );
            }
        },
        err,
        None,
    )
}

pub(super) struct Playback {
    rate: u32,
    prev: f32,
    next: f32,
    phase: u32,
    mixed: [f32; BLOCK],
}

impl Playback {
    pub(super) fn new(rate: u32) -> Self {
        Self {
            rate,
            prev: 0.0,
            next: 0.0,
            phase: rate,
            mixed: [0.0; BLOCK],
        }
    }

    pub(super) fn render<Frame>(
        &mut self,
        frames: impl ExactSizeIterator<Item = Frame>,
        master: f32,
        mix: &Mutex<Mixer>,
        mut write: impl FnMut(Frame, f32),
    ) -> MixerTiming {
        let mut timing = MixerTiming::default();
        let muted = master == 0.0;
        if muted {
            // Drop the two interpolator samples from the previous, audible callback.
            self.prev = 0.0;
            self.next = 0.0;
        }
        let Some(last_frame) = frames.len().checked_sub(1) else {
            return timing;
        };
        // Consume exactly the source samples this callback needs, including
        // any fractional phase carried over from the previous callback.
        let callback_samples = ((u64::from(self.phase) + last_frame as u64 * u64::from(RATE))
            / u64::from(self.rate)) as usize;
        let mut new_callback = Some(callback_samples);
        let mut remaining = callback_samples;
        let (mut offset, mut end) = (0, 0);
        for frame in frames {
            while self.phase >= self.rate {
                if offset == end {
                    end = remaining.min(BLOCK);
                    let waited = {
                        let started = Instant::now();
                        let mut mix = mix.lock().unwrap();
                        let waited = started.elapsed();
                        mix.mix_block(&mut self.mixed[..end], muted, new_callback.take());
                        waited
                    };
                    timing.record(waited);
                    remaining -= end;
                    offset = 0;
                }
                self.phase -= self.rate;
                self.prev = self.next;
                self.next = (self.mixed[offset] * master / 32768.0).clamp(-1.0, 1.0);
                offset += 1;
            }
            let sample =
                self.prev + (self.next - self.prev) * (self.phase as f32 / self.rate as f32);
            self.phase += RATE;
            write(frame, sample);
        }
        timing
    }
}

fn play<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mix: Arc<Mutex<Mixer>>,
    control: Arc<CaptureControl>,
    speaker: Arc<SpeakerControl>,
    reference: Arc<ArrayQueue<ReferenceBlock>>,
    timing: Arc<PlaybackTiming>,
    err: ErrorCallback,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let device_rate = config.sample_rate.0;
    let mut reference_framer = Framer::new(device_rate);
    let mut playback = Playback::new(device_rate);
    let mut tone = SpeakerTone::new(device_rate);
    let mut priority = devices::CallbackPriority::default();
    let mut previous_callback = None;
    device.build_output_stream(
        config,
        move |buffer: &mut [T], info| {
            let callback_at = Instant::now();
            priority.enter();
            let packet_start = if control.echo.load(Ordering::Relaxed) {
                let time = info.timestamp();
                Some(
                    callback_at
                        + time
                            .playback
                            .duration_since(&time.callback)
                            .unwrap_or_default(),
                )
            } else {
                reference_framer.reset();
                None
            };
            let master = control.playback_gain();
            let requested = speaker.preview.swap(0, Ordering::Relaxed);
            if requested != 0 && requested == master.to_bits() {
                tone.start(master);
            }
            let frames = buffer.chunks_exact_mut(channels);
            let frame_count = frames.len();
            let mut index = 0;
            let mut write_frame = |frame: &mut [T], sample: f32| {
                let output = T::from_sample(sample.clamp(-1.0, 0.999_969_5));
                frame.fill(output);
                if let Some(packet_start) = packet_start {
                    reference_framer.push(
                        f32::from_sample(output),
                        |phase| sample_time(packet_start, index, phase, u64::from(device_rate)),
                        |samples, played_at, _| {
                            reference.force_push(ReferenceBlock {
                                samples: *samples,
                                played_at,
                            });
                        },
                    );
                    index += 1;
                }
            };
            let mixer_timing = if tone.keep_playing_at(master) {
                // Include the local cue in the echo-canceller's playback reference.
                playback.render(frames, master, &mix, |frame, sample| {
                    write_frame(frame, sample + tone.next_sample());
                })
            } else {
                playback.render(frames, master, &mix, write_frame)
            };
            let interval = previous_callback
                .replace(callback_at)
                .map(|previous| callback_at - previous);
            timing.record(
                interval,
                callback_at.elapsed(),
                frame_count,
                device_rate,
                mixer_timing,
            );
        },
        err,
        None,
    )
}

struct SpeakerOutput {
    stream: cpal::Stream,
    control: Arc<SpeakerControl>,
}

#[derive(Clone)]
pub(super) struct CallAudio {
    pub(super) source: NativeAudioSource,
    pub(super) playback_timing: Arc<PlaybackTiming>,
}

#[derive(Default)]
struct SpeakerControl {
    failed: Arc<AtomicBool>,
    preview: AtomicU32,
}

impl SpeakerControl {
    fn request(&self, gain: f32) -> bool {
        if self.failed.load(Ordering::Relaxed) {
            return false;
        }
        self.preview.store(gain.to_bits(), Ordering::Relaxed);
        true
    }
}

pub(super) struct AudioIo {
    worker: Task,
    input: cpal::Stream,
    output: Option<SpeakerOutput>,
    failed: Arc<AtomicBool>,
}

pub(super) struct PreparedAudioIo {
    app: AppHandle,
    settings: watch::Receiver<AudioSettings>,
    control: Arc<CaptureControl>,
    source: Option<NativeAudioSource>,
    input: cpal::Stream,
    output: Option<SpeakerOutput>,
    capture: Arc<CaptureQueue>,
    reference: Arc<ArrayQueue<ReferenceBlock>>,
    failed: Arc<AtomicBool>,
}

// Types returned by WASAPI's shared-mode GetMixFormat.
macro_rules! build_stream {
    ($format:expr, $build:ident, $($arg:expr),+) => {
        match $format {
            SampleFormat::F32 => $build::<f32>($($arg),+),
            SampleFormat::I16 => $build::<i16>($($arg),+),
            SampleFormat::I32 => $build::<i32>($($arg),+),
            SampleFormat::I64 => $build::<i64>($($arg),+),
            SampleFormat::U8 => $build::<u8>($($arg),+),
            _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
        }
    };
}

pub(super) mod speaker_preview;

impl AudioIo {
    pub(super) fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
            || self
                .output
                .as_ref()
                .is_some_and(|output| output.control.failed.load(Ordering::Relaxed))
    }

    pub(super) fn preview_speaker(&self, gain: f32) -> bool {
        self.output
            .as_ref()
            .is_some_and(|output| output.control.request(gain))
    }

    pub(super) async fn stop(self) {
        let Self {
            mut worker,
            input,
            output,
            ..
        } = self;
        worker.0.abort();
        drop(input);
        drop(output);
        let _ = (&mut worker.0).await;
    }
}

impl PreparedAudioIo {
    pub(super) fn open(
        app: &AppHandle,
        prefs: &AudioSettings,
        settings: watch::Receiver<AudioSettings>,
        control: Arc<CaptureControl>,
        mix: Arc<Mutex<Mixer>>,
        call: Option<CallAudio>,
    ) -> Result<Self, String> {
        let input = devices::select(&prefs.input_device, true)?;
        let failed = Arc::new(AtomicBool::new(false));
        let input_queue = Arc::new(CaptureQueue::new());
        let reference = Arc::new(ArrayQueue::new(REFERENCE_QUEUE_BLOCKS));
        let input = devices::open_stream(&input, true, |format| {
            build_stream!(
                format.sample_format(),
                capture,
                &input,
                &format.config(),
                input_queue.clone(),
                error_handler(app, &failed, "麦克风")
            )
        })?;
        let output = if let Some(call) = &call {
            let output = devices::select(&prefs.output_device, false)?;
            let speaker = Arc::new(SpeakerControl::default());
            let stream = devices::open_stream(&output, false, |format| {
                build_stream!(
                    format.sample_format(),
                    play,
                    &output,
                    &format.config(),
                    mix.clone(),
                    control.clone(),
                    speaker.clone(),
                    reference.clone(),
                    call.playback_timing.clone(),
                    error_handler(app, &speaker.failed, "扬声器")
                )
            })?;
            Some(SpeakerOutput {
                stream,
                control: speaker,
            })
        } else {
            None
        };
        Ok(Self {
            app: app.clone(),
            settings,
            control,
            source: call.map(|call| call.source),
            input,
            output,
            capture: input_queue,
            reference,
            failed,
        })
    }

    pub(super) fn start(self) -> Result<AudioIo, String> {
        let Self {
            app,
            settings,
            control,
            source,
            input,
            output,
            capture,
            reference,
            failed,
        } = self;
        if let Some(output) = &output {
            output
                .stream
                .play()
                .map_err(|e| format!("无法启动播放设备：{e}"))?;
        }
        input.play().map_err(|e| format!("无法启动麦克风：{e}"))?;
        let fault = failed.clone();
        let worker = Task(tokio::spawn(async move {
            if let Err(error) = process(&app, settings, control, source, capture, reference).await {
                fault.store(true, Ordering::Relaxed);
                status(&app, "device-error", Some(&error));
            }
        }));
        Ok(AudioIo {
            input,
            output,
            worker,
            failed,
        })
    }
}

// The meter maps RMS between -60 dBFS and full scale onto the slider's 0..100.
fn input_level(samples: &[i16]) -> u16 {
    let power = samples
        .iter()
        .map(|&s| (f32::from(s) / 32768.0).powi(2))
        .sum::<f32>()
        / samples.len() as f32;
    ((1.0 + power.max(1e-6).log10() / 6.0) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u16
}

#[derive(Default)]
struct Gate {
    hangover: u8,
}
impl Gate {
    fn open(&mut self, threshold: Option<u16>, level: u16) -> bool {
        let Some(threshold) = threshold else {
            self.hangover = 0;
            return true;
        };
        if level >= threshold {
            self.hangover = 22;
            true
        } else if self.hangover > 0 {
            self.hangover -= 1;
            true
        } else {
            false
        }
    }
}

#[derive(Default)]
struct Meter {
    ticks: u8,
    peak: u16,
}

impl Meter {
    fn update(&mut self, level: Option<u16>) -> Option<u16> {
        let Some(level) = level else {
            *self = Self::default();
            return None;
        };
        self.peak = self.peak.max(level);
        self.ticks += 1;
        if self.ticks == 6 {
            return Some(std::mem::take(self).peak);
        }
        None
    }
}

fn apply_input_gain(samples: &mut [i16], input_gain: u16) {
    if input_gain != 100 {
        let gain = f32::from(input_gain) / 100.0;
        for sample in samples {
            *sample = (f32::from(*sample) * gain) as i16;
        }
    }
}

fn process_capture_samples(
    apm: &mut AudioProcessingModule,
    samples: &mut [i16; BLOCK],
    blocked: usize,
    input_gain: u16,
    monitoring: bool,
) -> Result<Option<u16>, String> {
    let raw_level = (monitoring && blocked != 0).then(|| {
        let mut raw = *samples;
        apply_input_gain(&mut raw, input_gain);
        input_level(&raw)
    });
    samples[..blocked].fill(0);
    apm.process_stream(samples, RATE as i32, 1)
        .map_err(|e| e.to_string())?;
    if blocked < BLOCK {
        apply_input_gain(samples, input_gain);
    }
    Ok(raw_level.or_else(|| monitoring.then(|| input_level(samples))))
}

async fn process(
    app: &AppHandle,
    settings: watch::Receiver<AudioSettings>,
    control: Arc<CaptureControl>,
    source: Option<NativeAudioSource>,
    input: Arc<CaptureQueue>,
    reference: Arc<ArrayQueue<ReferenceBlock>>,
) -> Result<(), String> {
    let mut effects: Option<(bool, bool, AudioProcessingModule)> = None;
    let mut gate = Gate::default();
    let mut meter = Meter::default();
    let mut render_delay = 0_i128;
    loop {
        let mut block = input.recv().await;
        let echo = source.is_some() && control.echo.load(Ordering::Relaxed);
        let (noise, activation, input_gain, threshold) = {
            let prefs = settings.borrow();
            (
                prefs.noise_suppression,
                prefs.mode == "activation",
                prefs.input_gain,
                prefs.threshold,
            )
        };
        if effects
            .as_ref()
            .is_none_or(|current| (current.0, current.1) != (echo, noise))
        {
            render_delay = 0;
            effects = Some((
                echo,
                noise,
                AudioProcessingModule::new(echo, false, true, noise),
            ));
        }
        let apm = &mut effects.as_mut().unwrap().2;
        for _ in 0..REFERENCE_QUEUE_BLOCKS {
            let Some(mut played) = reference.pop() else {
                break;
            };
            if echo {
                let analyzed_at = Instant::now();
                render_delay = if played.played_at >= analyzed_at {
                    (played.played_at - analyzed_at).as_nanos() as i128
                } else {
                    -((analyzed_at - played.played_at).as_nanos() as i128)
                };
                apm.process_reverse_stream(&mut played.samples, RATE as i32, 1)
                    .map_err(|e| e.to_string())?;
            }
        }
        if echo {
            let capture_delay = block.captured_at.elapsed().as_nanos() as i128;
            let delay_ms = ((render_delay + capture_delay) / 1_000_000).clamp(0, 500) as i32;
            apm.set_stream_delay_ms(delay_ms)
                .map_err(|e| e.to_string())?;
        }
        let monitoring = control.monitor.load(Ordering::Relaxed);
        let blocked = if source.is_some() {
            control.blocked_prefix(&block, Instant::now())
        } else {
            0
        };
        let meter_level =
            process_capture_samples(apm, &mut block.samples, blocked, input_gain, monitoring)?;
        if let Some(source) = &source {
            let activation_threshold = (activation && blocked < BLOCK).then_some(threshold);
            let level = activation_threshold.map_or(0, |_| {
                if blocked == 0 {
                    meter_level.unwrap_or_else(|| input_level(&block.samples))
                } else {
                    input_level(&block.samples[blocked..])
                }
            });
            let blocked = if gate.open(activation_threshold, level) {
                blocked.max(control.blocked_prefix(&block, Instant::now()))
            } else {
                BLOCK
            };
            block.samples[..blocked].fill(0);
            let frame = AudioFrame {
                data: Cow::Borrowed(&block.samples),
                sample_rate: RATE,
                num_channels: 1,
                samples_per_channel: BLOCK as u32,
            };
            source
                .capture_frame(&frame)
                .await
                .map_err(|e| e.to_string())?;
        }
        if let Some(peak) =
            meter.update(meter_level.filter(|_| control.monitor.load(Ordering::Relaxed)))
        {
            let _ = app.emit("voice-level", serde_json::json!({"level":peak}));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_tone_is_brief_and_scales_with_the_selected_gain() {
        for rate in [44_100, 48_000] {
            let render = |gain| {
                let mut tone = SpeakerTone::new(rate);
                assert_eq!(tone.next_sample(), 0.0);
                tone.start(gain);
                let samples: Vec<_> = (0..rate / 10).map(|_| tone.next_sample()).collect();
                assert_eq!(samples[0], 0.0);
                assert_eq!(samples.last(), Some(&0.0));
                assert_eq!(tone.next_sample(), 0.0);
                samples.into_iter().map(f32::abs).fold(0.0, f32::max)
            };
            let normal = render(1.0);
            assert!((0.05..0.2).contains(&normal));
            assert!((render(0.25) / normal - 0.25).abs() < 1e-6);
        }
    }

    #[test]
    fn speaker_tone_cancels_when_output_is_muted() {
        let mut tone = SpeakerTone::new(48_000);
        tone.start(1.0);
        assert!(tone.keep_playing_at(1.0));
        assert!(!tone.keep_playing_at(0.0));
        assert!(!tone.keep_playing_at(1.0));
    }

    #[test]
    fn failed_output_rejects_speaker_preview_requests() {
        let output = SpeakerControl::default();
        assert!(output.request(0.5));
        assert_eq!(output.preview.swap(0, Ordering::Relaxed), 0.5f32.to_bits());

        output.failed.store(true, Ordering::Relaxed);
        assert!(!output.request(0.5));
        assert_eq!(output.preview.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    #[ignore = "measures the local speaker callback with silent PCM"]
    async fn inspect_playback_cadence() {
        if std::env::var("DUIPIN_TEST_PLAYBACK_CADENCE").as_deref() != Ok("1") {
            return;
        }
        let device = devices::select("", false).unwrap();
        let mix = Arc::new(Mutex::new(Mixer::default()));
        let control = Arc::new(CaptureControl::default());
        let speaker = Arc::new(SpeakerControl::default());
        let reference = Arc::new(ArrayQueue::new(REFERENCE_QUEUE_BLOCKS));
        let timing = Arc::new(PlaybackTiming::default());
        let stream = devices::open_stream(&device, false, |format| {
            build_stream!(
                format.sample_format(),
                play,
                &device,
                &format.config(),
                mix.clone(),
                control.clone(),
                speaker.clone(),
                reference.clone(),
                timing.clone(),
                Box::new(|error| eprintln!("playback probe: {error}"))
            )
        })
        .unwrap();
        stream.play().unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(stream);
        let stats = timing.sample().expect("no playback callback was observed");
        let stats = serde_json::to_value(stats).unwrap();
        assert!(stats["callback_count"].as_u64().unwrap() > 1);
        assert!(stats["mixer_lock_count"].as_u64().unwrap() > 0);
        assert!(stats["max_callback_frames"].as_u64().unwrap() > 0);
        println!("WASAPI playback: {stats}");
    }

    #[tokio::test]
    #[ignore = "measures the local microphone without publishing audio"]
    async fn inspect_capture_cadence() {
        if std::env::var("DUIPIN_TEST_CAPTURE_CADENCE").as_deref() != Ok("1") {
            return;
        }
        let device = devices::select("", true).unwrap();
        let queue = Arc::new(CaptureQueue::new());
        let stream = devices::open_stream(&device, true, |format| {
            build_stream!(
                format.sample_format(),
                capture,
                &device,
                &format.config(),
                queue.clone(),
                Box::new(|err| eprintln!("capture probe: {err}"))
            )
        })
        .unwrap();
        stream.play().unwrap();
        let mut previous = tokio::time::timeout(Duration::from_secs(2), queue.recv())
            .await
            .expect("microphone did not deliver the first block");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut gaps = Vec::new();
        let mut sample_gaps = Vec::new();
        let mut capture_ages = Vec::new();
        while let Ok(block) = tokio::time::timeout_at(deadline, queue.recv()).await {
            let (callback_at, last_sample_at) = block.timing;
            capture_ages.push(
                callback_at
                    .saturating_duration_since(last_sample_at)
                    .as_micros(),
            );
            gaps.push(
                callback_at
                    .saturating_duration_since(previous.timing.0)
                    .as_micros(),
            );
            sample_gaps.push(
                block
                    .captured_at
                    .saturating_duration_since(previous.captured_at)
                    .as_micros(),
            );
            previous = block;
            if gaps.len() >= 800 {
                break;
            }
        }
        drop(stream);
        assert_eq!(
            gaps.len(),
            800,
            "microphone did not deliver the sample window"
        );
        for (label, mut intervals) in [
            ("callback cadence", gaps),
            ("capture cadence", sample_gaps),
            ("last sample age at callback", capture_ages),
        ] {
            intervals.sort_unstable();
            let short = intervals.iter().filter(|&&gap| gap < 3_000).count();
            let long = intervals.iter().filter(|&&gap| gap > 16_000).count();
            println!(
                "{label}: {} intervals, min={} us p50={} us p99={} us max={} us, <3ms={short}, >16ms={long}",
                intervals.len(),
                intervals[0],
                intervals[399],
                intervals[791],
                intervals[799],
            );
        }
    }

    #[test]
    fn gated_samples_do_not_reach_the_send_apm_or_hide_the_local_meter() {
        let mut apm = AudioProcessingModule::new(false, false, true, false);
        let mut muted = [12_000; BLOCK];
        let meter = process_capture_samples(&mut apm, &mut muted, BLOCK, 50, true).unwrap();
        assert_eq!(meter, Some(input_level(&[6_000; BLOCK])));
        assert_eq!(muted, [0; BLOCK]);

        let mut reopened = [0; BLOCK];
        assert_eq!(
            process_capture_samples(&mut apm, &mut reopened, 0, 100, false).unwrap(),
            None
        );
        assert_eq!(reopened, [0; BLOCK]);
    }

    #[test]
    fn partially_blocked_pcm_keeps_the_new_samples() {
        let mut apm = AudioProcessingModule::new(false, false, false, false);
        let mut split = [12_000; BLOCK];
        let meter = process_capture_samples(&mut apm, &mut split, BLOCK / 2, 50, true).unwrap();
        assert_eq!(meter, Some(input_level(&[6_000; BLOCK])));
        assert_eq!(&split[..BLOCK / 2], &[0; BLOCK / 2]);
        assert_eq!(&split[BLOCK / 2..], &[6_000; BLOCK / 2]);
    }

    #[test]
    fn invalid_capture_timing_places_the_whole_packet_before_the_callback() {
        let callback = Instant::now();
        let duration = Duration::from_millis(20);
        let device_callback = cpal::StreamInstant::new(100, 0);
        let capture_times = [
            cpal::StreamInstant::new(100, 1),
            device_callback,
            device_callback.sub(Duration::from_millis(1)).unwrap(),
            #[cfg(windows)]
            cpal::StreamInstant::new(0, 0),
        ];
        for capture in capture_times {
            let timestamp = cpal::InputStreamTimestamp {
                callback: device_callback,
                capture,
            };
            let (start, measured) = capture_packet_start(callback, timestamp, 882, 44_100);
            assert!(!measured);
            assert_eq!(callback.duration_since(start), duration);
            assert!(sample_time(start, 881, 0, 44_100) < callback);
        }
        let reported = Duration::from_millis(35);
        let timestamp = cpal::InputStreamTimestamp {
            callback: device_callback,
            capture: device_callback.sub(reported).unwrap(),
        };
        assert_eq!(
            capture_packet_start(callback, timestamp, 882, 44_100),
            (callback - reported, true)
        );
    }

    #[tokio::test]
    async fn capture_queue_wakes_a_waiting_consumer() {
        use std::{
            future::Future,
            sync::atomic::AtomicUsize,
            task::{Context, Poll},
        };

        #[derive(Default)]
        struct WakeCount(AtomicUsize);

        impl futures_util::task::ArcWake for WakeCount {
            fn wake_by_ref(count: &Arc<Self>) {
                count.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let queue = Arc::new(CaptureQueue::new());
        let now = Instant::now();
        let mut receive = Box::pin(queue.recv());
        let count = Arc::new(WakeCount::default());
        let waker = futures_util::task::waker(count.clone());
        let mut context = Context::from_waker(&waker);
        assert!(receive.as_mut().poll(&mut context).is_pending());
        let producer = queue.clone();
        std::thread::spawn(move || {
            producer.push(CaptureBlock {
                samples: [123; BLOCK],
                captured_at: now,
                ready_at: now,
                timing: (now, now),
            });
        })
        .join()
        .unwrap();
        assert_eq!(count.0.load(Ordering::Relaxed), 1);
        let Poll::Ready(block) = receive.as_mut().poll(&mut context) else {
            panic!("producer wake left the capture queue pending");
        };
        assert_eq!(block.samples, [123; BLOCK]);
    }

    #[test]
    fn capture_queue_keeps_the_most_recent_blocks() {
        let queue = CaptureQueue::new();
        let now = Instant::now();
        for sample in 0..=CAPTURE_QUEUE_BLOCKS as i16 {
            queue.push(CaptureBlock {
                samples: [sample; BLOCK],
                captured_at: now,
                ready_at: now,
                timing: (now, now),
            });
        }
        for sample in 1..=CAPTURE_QUEUE_BLOCKS as i16 {
            assert_eq!(queue.blocks.pop().unwrap().samples, [sample; BLOCK]);
        }
        assert!(queue.blocks.is_empty());
    }

    #[test]
    fn stale_capture_is_silenced_after_a_worker_stall() {
        let control = CaptureControl::default();
        let now = control.started_at + Duration::from_millis(10);
        let block = CaptureBlock {
            samples: [123; BLOCK],
            captured_at: now - Duration::from_millis(10),
            ready_at: now,
            timing: (now, now),
        };
        assert_eq!(control.blocked_prefix(&block, now + CAPTURE_MAX_BACKLOG), 0);
        assert_eq!(
            control.blocked_prefix(&block, now + CAPTURE_MAX_BACKLOG + Duration::from_micros(1)),
            BLOCK
        );
    }

    #[test]
    fn capture_limits_new_backlog_on_devices_with_high_fixed_delay() {
        let control = CaptureControl::default();
        let mut baseline = None;
        for (offset, device_delay, blocked) in [
            (200, Some(70), 0),
            (210, None, 0),
            (220, Some(70), 0),
            (290, Some(130), BLOCK),
            (300, Some(119), 0),
            (310, Some(70), 0),
        ] {
            let callback = control.started_at + Duration::from_millis(offset);
            let last_sample_at = device_delay.map(|ms| callback - Duration::from_millis(ms));
            let timing = last_sample_at.unwrap_or(callback);
            let block = CaptureBlock {
                samples: [123; BLOCK],
                captured_at: timing - Duration::from_millis(10),
                ready_at: capture_ready_at(&mut baseline, callback, last_sample_at),
                timing: (callback, timing),
            };
            assert_eq!(control.blocked_prefix(&block, callback), blocked);
        }
    }

    #[test]
    fn reopening_excludes_old_capture_and_keeps_new_samples_in_the_first_block() {
        let control = CaptureControl::default();
        let start = control.started_at;
        let block = CaptureBlock {
            samples: [123; BLOCK],
            captured_at: start,
            ready_at: start + Duration::from_millis(10),
            timing: (
                start + Duration::from_millis(10),
                start + Duration::from_millis(10),
            ),
        };
        let closed = start + Duration::from_millis(2);
        let opened = start + Duration::from_millis(5);
        control.update(MUTE, MUTE, closed);
        assert_eq!(control.blocked_prefix(&block, closed), BLOCK);
        control.update(MUTE, 0, opened);
        assert_eq!(control.blocked_prefix(&block, block.ready_at), BLOCK / 2);

        let older = CaptureBlock {
            captured_at: block.captured_at - Duration::from_millis(10),
            ..block
        };
        assert_eq!(control.blocked_prefix(&older, block.ready_at), BLOCK);

        control.update(PTT_MODE | PTT_PRESSED, PTT_MODE, closed);
        control.update(PTT_PRESSED, PTT_PRESSED, opened);
        control.update(PTT_PRESSED, 0, start + Duration::from_millis(8));
        control.update(PTT_PRESSED, PTT_PRESSED, start + Duration::from_millis(10));
        assert_eq!(control.blocked_prefix(&block, block.ready_at), BLOCK);
        let first_pressed = CaptureBlock {
            captured_at: block.ready_at,
            ..block
        };
        assert_eq!(control.blocked_prefix(&first_pressed, block.ready_at), 0);
    }

    #[test]
    fn activation_holds_the_gate_for_twenty_two_quiet_blocks() {
        let mut gate = Gate::default();
        assert!(!gate.open(Some(40), 5));
        assert!(gate.open(Some(40), 50));
        for _ in 0..22 {
            assert!(gate.open(Some(40), 5));
        }
        assert!(!gate.open(Some(40), 5));
        assert!(gate.open(Some(40), 50));
        assert!(gate.open(None, 0));
        assert!(!gate.open(Some(40), 5));
        assert_eq!(input_level(&[0; BLOCK]), 0);
    }

    #[test]
    fn a_closed_meter_resets_its_peak_and_reporting_interval() {
        let mut meter = Meter::default();
        assert_eq!(meter.update(Some(100)), None);
        assert_eq!(meter.update(None), None);
        for _ in 0..5 {
            assert_eq!(meter.update(Some(1)), None);
        }
        assert_eq!(meter.update(Some(15)), Some(15));
    }

    #[test]
    fn resampling_keeps_exact_ten_millisecond_frames() {
        for rate in [16_000, 44_100, 48_000, 96_000] {
            let mut block_count = 0;
            let mut framer = Framer::new(rate);
            let now = Instant::now();
            for _ in 0..rate / 10 {
                framer.push(
                    0.5,
                    |_| now,
                    |samples, _, _| {
                        assert_eq!(samples[BLOCK - 1], 16384);
                        block_count += 1;
                    },
                );
            }
            assert_eq!(block_count, 10);
        }
    }
}
