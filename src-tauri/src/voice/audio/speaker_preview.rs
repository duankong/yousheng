use std::{
    f32::consts::TAU,
    sync::Arc,
    time::{Duration, Instant},
};

use cpal::{traits::DeviceTrait, FromSample, SampleFormat, SizedSample};
use tokio::sync::oneshot;

use super::{devices, CaptureControl};

pub(super) struct SpeakerTone {
    rate: u32,
    frame: u32,
    end: u32,
    gain: f32,
}

impl SpeakerTone {
    pub(super) fn new(rate: u32) -> Self {
        let end = rate / 10;
        Self {
            rate,
            frame: end,
            end,
            gain: 0.0,
        }
    }

    pub(super) fn start(&mut self, gain: f32) {
        self.frame = 0;
        self.gain = gain;
    }

    pub(super) fn keep_playing_at(&mut self, gain: f32) -> bool {
        if gain != self.gain {
            self.frame = self.end;
        }
        self.frame < self.end
    }

    pub(super) fn next_sample(&mut self) -> f32 {
        if self.frame >= self.end {
            return 0.0;
        }
        let elapsed = self.frame as f32 / self.rate as f32;
        let remaining = (self.end - self.frame - 1) as f32 / self.rate as f32;
        self.frame += 1;
        // A short fade on each side of the chime avoids clicks.
        let envelope = (elapsed / 0.005).min(remaining / 0.025).min(1.0);
        0.12 * self.gain * envelope * (TAU * 880.0 * elapsed).sin()
    }
}

pub(in crate::voice) fn prepare_on_device(
    device_id: &str,
    control: Arc<CaptureControl>,
    gain: f32,
) -> Result<(cpal::Stream, oneshot::Receiver<Instant>), String> {
    let device = devices::select(device_id, false)?;
    devices::open_stream(&device, false, |format| {
        build_stream!(
            format.sample_format(),
            open_preview,
            &device,
            &format.config(),
            control.clone(),
            gain
        )
    })
}

pub(in crate::voice) async fn wait_until_played(
    finished: oneshot::Receiver<Instant>,
) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let played_at = finished.await.map_err(|_| "扬声器试听已中断")?;
        tokio::time::sleep_until(played_at.into()).await;
        Ok(())
    })
    .await
    .map_err(|_| "扬声器试听超时")?
    .map_err(str::to_owned)
}

fn open_preview<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    control: Arc<CaptureControl>,
    gain: f32,
) -> Result<(cpal::Stream, oneshot::Receiver<Instant>), cpal::BuildStreamError> {
    let rate = config.sample_rate.0;
    let channels = usize::from(config.channels);
    let mut tone = SpeakerTone::new(rate);
    tone.start(gain);
    let (finished, receiver) = oneshot::channel();
    let mut finished = Some(finished);
    let stream = device.build_output_stream(
        config,
        move |buffer: &mut [T], info| {
            let frames = buffer.chunks_exact_mut(channels);
            let frames_count = frames.len();
            let gain = control.playback_gain();
            tone.keep_playing_at(gain);
            for frame in frames {
                frame.fill(T::from_sample(tone.next_sample()));
            }
            if tone.frame == tone.end {
                if let Some(finished) = finished.take() {
                    // Keep the stream until the device has played this final buffer.
                    let time = info.timestamp();
                    let delay = time
                        .playback
                        .duration_since(&time.callback)
                        .unwrap_or_default();
                    let duration = Duration::from_secs_f64(frames_count as f64 / f64::from(rate));
                    let _ = finished.send(Instant::now() + delay + duration);
                }
            }
        },
        |_| {},
        None,
    )?;
    Ok((stream, receiver))
}
