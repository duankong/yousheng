use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use super::{devices, status, AudioSettings, Mixer, Task};
use cpal::{
    traits::{DeviceTrait, StreamTrait},
    FromSample, Sample, SampleFormat, SizedSample,
};
use livekit::webrtc::{
    audio_frame::AudioFrame, audio_source::native::NativeAudioSource,
    native::apm::AudioProcessingModule,
};
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, watch};

const RATE: u32 = 48_000;
const BLOCK: usize = 480;

struct Block {
    samples: [i16; BLOCK],
    delay_ms: i32,
}

// Both hardware callbacks deliver actual samples at the device's native rate.
// The phase is carried across callbacks, including for 44.1 kHz devices.
struct Framer {
    rate: u32,
    phase: u32,
    previous: f32,
    filled: usize,
    samples: [i16; BLOCK],
    sender: mpsc::Sender<Block>,
}

impl Framer {
    fn new(rate: u32, sender: mpsc::Sender<Block>) -> Self {
        Self {
            rate,
            phase: 0,
            previous: 0.0,
            filled: 0,
            samples: [0; BLOCK],
            sender,
        }
    }
    fn push(&mut self, sample: f32, delay_ms: i32) {
        self.phase += RATE;
        while self.phase >= self.rate {
            self.phase -= self.rate;
            let value = sample + (self.previous - sample) * (self.phase as f32 / RATE as f32);
            self.samples[self.filled] = (value * 32768.0) as i16;
            self.filled += 1;
            if self.filled == BLOCK {
                let _ = self.sender.try_send(Block {
                    samples: self.samples,
                    delay_ms,
                });
                self.filled = 0;
            }
        }
        self.previous = sample;
    }
}

fn latency(delay: Option<Duration>) -> i32 {
    delay.map_or(0, |duration| duration.as_millis().min(500) as i32)
}

type ErrorCallback = Box<dyn FnMut(cpal::StreamError) + Send>;

fn error_handler(app: &AppHandle, failed: &Arc<AtomicBool>) -> ErrorCallback {
    let app = app.clone();
    let failed = failed.clone();
    Box::new(move |e| {
        if !failed.swap(true, Ordering::Relaxed) {
            status(
                &app,
                "error",
                Some(&format!("音频设备已停止：{e}，请在声音设置中重新选择设备")),
            );
        }
    })
}

fn capture<T: SizedSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<Block>,
    err: ErrorCallback,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let mut frames = Framer::new(config.sample_rate.0, tx);
    device.build_input_stream(
        config,
        move |samples: &[T], info| {
            let time = info.timestamp();
            let delay = latency(time.callback.duration_since(&time.capture));
            for frame in samples.chunks_exact(channels) {
                let sum = frame
                    .iter()
                    .map(|sample| f32::from_sample(*sample))
                    .sum::<f32>();
                frames.push(sum / channels as f32, delay);
            }
        },
        err,
        None,
    )
}

fn play<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mix: Arc<Mutex<Mixer>>,
    settings: watch::Receiver<AudioSettings>,
    reference: mpsc::Sender<Block>,
    err: ErrorCallback,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let channels = usize::from(config.channels);
    let device_rate = config.sample_rate.0;
    let mut reference = Framer::new(device_rate, reference);
    let (mut prev, mut next, mut phase) = (0.0, 0.0, device_rate);
    device.build_output_stream(
        config,
        move |buffer: &mut [T], info| {
            let time = info.timestamp();
            let delay = latency(time.playback.duration_since(&time.callback));
            let master = f32::from(settings.borrow().output_gain) / 100.0;
            let mut mixer = mix.lock().unwrap();
            for frame in buffer.chunks_exact_mut(channels) {
                while phase >= device_rate {
                    phase -= device_rate;
                    prev = next;
                    next = mixer.next_sample(master);
                }
                let sample = prev + (next - prev) * (phase as f32 / device_rate as f32);
                phase += RATE;
                let output = T::from_sample(sample.clamp(-1.0, 0.999_969_5));
                frame.fill(output);
                reference.push(sample, delay);
            }
        },
        err,
        None,
    )
}

pub(super) struct AudioIo {
    _worker: Task,
    _input: cpal::Stream,
    _output: Option<cpal::Stream>,
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
            format => return Err(format!("不支持设备的采样格式 {format}")),
        }.map_err(|error| error.to_string())?
    };
}

impl AudioIo {
    pub(super) fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    pub(super) fn open(
        app: &AppHandle,
        prefs: &AudioSettings,
        settings: watch::Receiver<AudioSettings>,
        pressed: Arc<AtomicBool>,
        mix: Arc<Mutex<Mixer>>,
        source: Option<NativeAudioSource>,
    ) -> Result<Self, String> {
        let input = devices::select(&prefs.input_device, true)?;
        let infmt = input
            .default_input_config()
            .map_err(|e| format!("无法读取麦克风的采样格式：{e}"))?;
        let failed = Arc::new(AtomicBool::new(false));
        let (input_tx, input_rx) = mpsc::channel(5);
        let (reference_tx, reference_rx) = mpsc::channel(8);
        let input = build_stream!(
            infmt.sample_format(),
            capture,
            &input,
            &infmt.config(),
            input_tx,
            error_handler(app, &failed)
        );
        let output = if source.is_some() {
            let output = devices::select(&prefs.output_device, false)?;
            let outfmt = output
                .default_output_config()
                .map_err(|e| format!("无法读取播放设备的采样格式：{e}"))?;
            let stream = build_stream!(
                outfmt.sample_format(),
                play,
                &output,
                &outfmt.config(),
                mix,
                settings.clone(),
                reference_tx,
                error_handler(app, &failed)
            );
            stream
                .play()
                .map_err(|e| format!("无法启动播放设备：{e}"))?;
            Some(stream)
        } else {
            None
        };
        input.play().map_err(|e| format!("无法启动麦克风：{e}"))?;
        let app = app.clone();
        let fault = failed.clone();
        let worker = Task(tokio::spawn(async move {
            if let Err(error) =
                process(&app, settings, pressed, source, input_rx, reference_rx).await
            {
                fault.store(true, Ordering::Relaxed);
                status(&app, "error", Some(&error));
            }
        }));
        Ok(Self {
            _input: input,
            _output: output,
            _worker: worker,
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
    fn open(&mut self, prefs: &AudioSettings, level: u16, pressed: bool) -> bool {
        if prefs.muted || prefs.mode != "activation" {
            self.hangover = 0;
            return !prefs.muted && (prefs.mode == "open" || (prefs.mode == "ptt" && pressed));
        }
        if level >= prefs.threshold {
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

async fn process(
    app: &AppHandle,
    settings: watch::Receiver<AudioSettings>,
    pressed: Arc<AtomicBool>,
    source: Option<NativeAudioSource>,
    mut input: mpsc::Receiver<Block>,
    mut reference: mpsc::Receiver<Block>,
) -> Result<(), String> {
    let mut effects: Option<(bool, bool, AudioProcessingModule)> = None;
    let mut gate = Gate::default();
    let (mut render_delay, mut ticks, mut peak) = (0, 0, 0);
    while let Some(mut block) = input.recv().await {
        let (echo, noise) = {
            let prefs = settings.borrow();
            (
                source.is_some() && prefs.echo_cancellation,
                prefs.noise_suppression,
            )
        };
        if effects
            .as_ref()
            .is_none_or(|current| (current.0, current.1) != (echo, noise))
        {
            effects = Some((
                echo,
                noise,
                AudioProcessingModule::new(echo, false, true, noise),
            ));
        }
        let apm = &mut effects.as_mut().unwrap().2;
        while let Ok(mut played) = reference.try_recv() {
            render_delay = played.delay_ms;
            if echo {
                apm.process_reverse_stream(&mut played.samples, RATE as i32, 1)
                    .map_err(|e| e.to_string())?;
            }
        }
        apm.set_stream_delay_ms((render_delay + block.delay_ms).min(500))
            .map_err(|e| e.to_string())?;
        apm.process_stream(&mut block.samples, RATE as i32, 1)
            .map_err(|e| e.to_string())?;
        {
            let prefs = settings.borrow();
            let gain = f32::from(prefs.input_gain) / 100.0;
            for sample in &mut block.samples {
                *sample = (f32::from(*sample) * gain) as i16;
            }
            let level = input_level(&block.samples);
            peak = peak.max(level);
            if !gate.open(&prefs, level, pressed.load(Ordering::Relaxed)) {
                block.samples.fill(0);
            }
        }
        ticks += 1;
        if ticks == 6 {
            let _ = app.emit("voice-level", serde_json::json!({"level":peak}));
            ticks = 0;
            peak = 0;
        }
        if let Some(source) = &source {
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_hangover_and_ptt_respect_manual_mute() {
        let mut prefs = AudioSettings {
            mode: "activation".into(),
            ..Default::default()
        };
        let mut gate = Gate::default();
        assert!(!gate.open(&prefs, 5, false));
        assert!(gate.open(&prefs, 50, false));
        for _ in 0..22 {
            assert!(gate.open(&prefs, 5, false));
        }
        assert!(!gate.open(&prefs, 5, false));
        prefs.mode = "ptt".into();
        assert!(!gate.open(&prefs, 100, false));
        assert!(gate.open(&prefs, 0, true));
        prefs.muted = true;
        assert!(!gate.open(&prefs, 100, true));
        assert_eq!(input_level(&[0; BLOCK]), 0);
    }

    #[test]
    fn resampling_keeps_exact_ten_millisecond_frames() {
        for rate in [16_000, 44_100, 48_000, 96_000] {
            let (tx, mut rx) = mpsc::channel(12);
            let mut framer = Framer::new(rate, tx);
            for _ in 0..rate / 10 {
                framer.push(0.5, 10);
            }
            for _ in 0..10 {
                assert_eq!(rx.try_recv().unwrap().samples[BLOCK - 1], 16384);
            }
            assert!(rx.try_recv().is_err());
        }
    }
}
