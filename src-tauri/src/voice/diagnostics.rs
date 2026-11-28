use super::Mixer;
use crate::diagnostics::{Diagnostics, Event};
use livekit::{
    prelude::{ConnectionState, Room},
    webrtc::stats::{InboundRtpStats, RtcStats},
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INBOUND_AUDIO_STREAMS: usize = 32;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum VoiceEvent {
    Status {
        call_id: u64,
        elapsed_ms: u64,
        state: VoiceState,
    },
    Sample {
        call_id: u64,
        elapsed_ms: u64,
        rtc: Option<RtcAudioStats>,
        pcm: Option<PcmStats>,
        playback: Option<PlaybackStats>,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VoiceState {
    Connecting,
    Connected,
    ConnectFailed,
    ConnectCancelled,
    Reconnecting,
    Reconnected,
    Disconnected,
    DeviceError,
    DeviceRecovered,
    DeviceConfigured,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RtcAudioStats {
    inbound_audio: Vec<InboundAudioStats>,
    inbound_audio_truncated: bool,
    publisher_webrtc_rtt_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
struct InboundAudioStats {
    stream_id: u64,
    packets_received_total: u64,
    packets_lost_total: i64,
    jitter_ms: Option<f64>,
    jitter_buffer_residence_ms: Option<f64>,
    concealed_samples_total: u64,
    concealment_events_total: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct PcmStats {
    pub(super) peak_track_queue_samples: usize,
    pub(super) trimmed_samples_total: u64,
    pub(super) unfilled_track_samples_total: u64,
}

#[derive(Default)]
pub(super) struct MixerTiming {
    lock_count: u64,
    total_wait_us: u64,
    max_wait_us: u64,
}

impl MixerTiming {
    pub(super) fn record(&mut self, waited: Duration) {
        let waited = waited.as_micros() as u64;
        self.lock_count += 1;
        self.total_wait_us += waited;
        self.max_wait_us = self.max_wait_us.max(waited);
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct PlaybackStats {
    callback_count: u64,
    callback_interval_count: u64,
    callback_interval_us_total: u64,
    max_callback_interval_us: u64,
    callback_duration_us_total: u64,
    max_callback_duration_us: u64,
    callback_over_budget_count: u64,
    callback_duration_p95_us: Option<u64>,
    callback_duration_p99_us: Option<u64>,
    mixer_lock_count: u64,
    mixer_wait_us_total: u64,
    max_mixer_wait_us: u64,
    max_callback_mixer_wait_us: u64,
    callback_mixer_wait_p95_us: Option<u64>,
    callback_mixer_wait_p99_us: Option<u64>,
    max_callback_frames: usize,
    skipped_callbacks: u64,
}

// A fixed-size histogram keeps observations allocation-free on the output thread.
// Percentiles report the upper edge of the containing bucket, capped at the observed max.
#[derive(Default)]
struct TimingHistogram([u64; 20]);

impl TimingHistogram {
    fn record(&mut self, micros: u64) {
        let bucket = (u64::BITS - micros.leading_zeros()) as usize;
        let last = self.0.len() - 1;
        self.0[bucket.min(last)] += 1;
    }

    fn percentile(&self, count: u64, percentile: u64, max: u64) -> Option<u64> {
        if count == 0 {
            return None;
        }
        let mut rank = (count * percentile).div_ceil(100);
        for (bucket, observed) in self.0.iter().enumerate() {
            if *observed >= rank {
                return Some(if bucket == self.0.len() - 1 {
                    max
                } else {
                    max.min((1 << bucket) - 1)
                });
            }
            rank -= observed;
        }
        Some(max)
    }
}

#[derive(Default)]
struct PlaybackWindow {
    stats: PlaybackStats,
    duration: TimingHistogram,
    mixer_wait: TimingHistogram,
}

#[derive(Default)]
pub(super) struct PlaybackTiming {
    window: Mutex<PlaybackWindow>,
    skipped_callbacks: AtomicU64,
}

impl PlaybackTiming {
    pub(super) fn record(
        &self,
        interval: Option<Duration>,
        duration: Duration,
        frames: usize,
        rate: u32,
        mixer: MixerTiming,
    ) {
        let Ok(mut window) = self.window.try_lock() else {
            self.skipped_callbacks.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let over_budget = duration.as_secs_f64() * f64::from(rate) > frames as f64;
        let duration = duration.as_micros() as u64;
        window.duration.record(duration);
        window.mixer_wait.record(mixer.total_wait_us);
        let stats = &mut window.stats;
        stats.callback_count += 1;
        stats.callback_duration_us_total += duration;
        stats.max_callback_duration_us = stats.max_callback_duration_us.max(duration);
        stats.callback_over_budget_count += u64::from(over_budget);
        stats.max_callback_frames = stats.max_callback_frames.max(frames);
        if let Some(interval) = interval {
            let interval = interval.as_micros() as u64;
            stats.callback_interval_count += 1;
            stats.callback_interval_us_total += interval;
            stats.max_callback_interval_us = stats.max_callback_interval_us.max(interval);
        }
        stats.mixer_lock_count += mixer.lock_count;
        stats.mixer_wait_us_total += mixer.total_wait_us;
        stats.max_mixer_wait_us = stats.max_mixer_wait_us.max(mixer.max_wait_us);
        stats.max_callback_mixer_wait_us =
            stats.max_callback_mixer_wait_us.max(mixer.total_wait_us);
    }

    pub(super) fn sample(&self) -> Option<PlaybackStats> {
        let mut window = self.window.lock().unwrap();
        let PlaybackWindow {
            mut stats,
            duration,
            mixer_wait,
        } = std::mem::take(&mut *window);
        stats.skipped_callbacks = self.skipped_callbacks.swap(0, Ordering::Relaxed);
        drop(window);
        let count = stats.callback_count;
        let max_duration = stats.max_callback_duration_us;
        let max_wait = stats.max_callback_mixer_wait_us;
        stats.callback_duration_p95_us = duration.percentile(count, 95, max_duration);
        stats.callback_duration_p99_us = duration.percentile(count, 99, max_duration);
        stats.callback_mixer_wait_p95_us = mixer_wait.percentile(count, 95, max_wait);
        stats.callback_mixer_wait_p99_us = mixer_wait.percentile(count, 99, max_wait);
        (count != 0 || stats.skipped_callbacks != 0).then_some(stats)
    }
}

#[derive(Clone)]
pub(super) struct CallDiagnostics {
    diagnostics: Diagnostics,
    call_id: u64,
    started: Instant,
    state: Arc<Mutex<CallState>>,
}

#[derive(Default)]
struct CallState {
    ended: bool,
    connection_epoch: u64,
}

pub(super) struct ConnectingCall(Option<CallDiagnostics>);

impl ConnectingCall {
    pub(super) fn begin(diagnostics: &Diagnostics) -> Self {
        let call = CallDiagnostics {
            diagnostics: diagnostics.clone(),
            call_id: diagnostics.next_id(),
            started: Instant::now(),
            state: Default::default(),
        };
        call.status(VoiceState::Connecting);
        Self(Some(call))
    }

    pub(super) fn finish(mut self) -> CallDiagnostics {
        self.0.take().unwrap()
    }
}

impl Drop for ConnectingCall {
    fn drop(&mut self) {
        if let Some(call) = &self.0 {
            call.status(VoiceState::ConnectCancelled);
        }
    }
}

impl CallDiagnostics {
    pub(super) fn status(&self, state: VoiceState) {
        let mut call = self.state.lock().unwrap();
        if !call.ended {
            match state {
                VoiceState::Disconnected => call.ended = true,
                VoiceState::Reconnecting | VoiceState::Reconnected => call.connection_epoch += 1,
                _ => (),
            }
            self.diagnostics.record(Event::Voice(VoiceEvent::Status {
                call_id: self.call_id,
                elapsed_ms: self.elapsed_ms(),
                state,
            }));
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub(super) async fn sample(
        self,
        room: Arc<Room>,
        mix: Arc<Mutex<Mixer>>,
        playback: Arc<PlaybackTiming>,
    ) {
        let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tracker = RtcStatsTracker::default();
        let mut previous_epoch = 0;
        loop {
            interval.tick().await;
            let epoch = self.state.lock().unwrap().connection_epoch;
            if epoch != previous_epoch {
                tracker.streams.clear();
                previous_epoch = epoch;
            }
            let report = match room.connection_state() {
                ConnectionState::Disconnected => break,
                ConnectionState::Reconnecting => None,
                ConnectionState::Connected => {
                    match tokio::time::timeout(SAMPLE_INTERVAL, room.get_stats()).await {
                        Ok(Ok(stats)) if room.connection_state() == ConnectionState::Connected => {
                            Some(stats)
                        }
                        _ => None,
                    }
                }
            };
            let rtc =
                report.map(|stats| tracker.sample(&stats.publisher_stats, stats.subscriber_stats));
            let pcm = mix.try_lock().ok().map(|mut mixer| mixer.sample_pcm());
            let state = self.state.lock().unwrap();
            if state.ended {
                break;
            }
            let rtc = rtc.filter(|_| state.connection_epoch == epoch);
            let playback = playback.sample();
            if rtc.is_none() {
                tracker.streams.clear();
            }
            self.diagnostics.record(Event::Voice(VoiceEvent::Sample {
                call_id: self.call_id,
                elapsed_ms: self.elapsed_ms(),
                rtc,
                pcm,
                playback,
            }));
        }
    }
}

#[derive(Default)]
struct RtcStatsTracker {
    next_id: u64,
    streams: HashMap<(String, u32), StreamSnapshot>,
}

struct StreamSnapshot {
    stream_id: u64,
    packets_received: u64,
    emitted: u64,
    delay: f64,
    concealed_samples: u64,
    concealment_events: u64,
}

impl StreamSnapshot {
    fn from_stats(stats: &InboundRtpStats) -> Self {
        Self {
            stream_id: 0,
            packets_received: stats.received.packets_received,
            emitted: stats.inbound.jitter_buffer_emitted_count,
            delay: stats.inbound.jitter_buffer_delay,
            concealed_samples: stats.inbound.concealed_samples,
            concealment_events: stats.inbound.concealment_events,
        }
    }

    fn continues(&self, previous: &Self) -> bool {
        self.packets_received >= previous.packets_received
            && self.emitted >= previous.emitted
            && self.delay >= previous.delay
            && self.concealed_samples >= previous.concealed_samples
            && self.concealment_events >= previous.concealment_events
    }
}

impl RtcStatsTracker {
    fn sample(&mut self, publisher: &[RtcStats], subscriber: Vec<RtcStats>) -> RtcAudioStats {
        let mut previous = std::mem::take(&mut self.streams);
        let mut inbound = subscriber.into_iter().filter_map(|stat| match stat {
            RtcStats::InboundRtp(stats) if stats.stream.kind == "audio" => Some(stats),
            _ => None,
        });
        let inbound_audio = inbound
            .by_ref()
            .take(MAX_INBOUND_AUDIO_STREAMS)
            .map(|stats| {
                let mut current = StreamSnapshot::from_stats(&stats);
                let key = (stats.rtc.id, stats.stream.ssrc);
                let prior = previous
                    .remove(&key)
                    .filter(|prior| current.continues(prior));
                let residence_ms = if let Some(prior) = prior {
                    current.stream_id = prior.stream_id;
                    let emitted = current.emitted - prior.emitted;
                    (emitted > 0)
                        .then(|| (current.delay - prior.delay) / emitted as f64)
                        .and_then(seconds_to_ms)
                } else {
                    self.next_id += 1;
                    current.stream_id = self.next_id;
                    None
                };
                let report = InboundAudioStats {
                    stream_id: current.stream_id,
                    packets_received_total: current.packets_received,
                    packets_lost_total: stats.received.packets_lost,
                    jitter_ms: seconds_to_ms(stats.received.jitter),
                    jitter_buffer_residence_ms: residence_ms,
                    concealed_samples_total: current.concealed_samples,
                    concealment_events_total: current.concealment_events,
                };
                self.streams.insert(key, current);
                report
            })
            .collect();
        let publisher_webrtc_rtt_ms = publisher.iter().find_map(|stat| match stat {
            RtcStats::RemoteInboundRtp(stats)
                if stats.stream.kind == "audio"
                    && stats.remote_inbound.round_trip_time_measurements > 0 =>
            {
                seconds_to_ms(stats.remote_inbound.round_trip_time)
            }
            _ => None,
        });
        RtcAudioStats {
            inbound_audio,
            inbound_audio_truncated: inbound.next().is_some(),
            publisher_webrtc_rtt_ms,
        }
    }
}

fn seconds_to_ms(seconds: f64) -> Option<f64> {
    let ms = seconds * 1000.0;
    (ms.is_finite() && ms >= 0.0).then_some(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_playback_callback_metrics_per_sampling_window() {
        let playback = PlaybackTiming::default();
        playback.record(
            None,
            Duration::from_micros(40),
            480,
            48_000,
            MixerTiming {
                lock_count: 1,
                total_wait_us: 5,
                max_wait_us: 5,
            },
        );
        playback.record(
            Some(Duration::from_millis(12)),
            Duration::from_micros(20_100),
            960,
            48_000,
            MixerTiming {
                lock_count: 2,
                total_wait_us: 17,
                max_wait_us: 12,
            },
        );
        let stats = serde_json::to_value(playback.sample()).unwrap();
        assert_eq!(
            stats,
            serde_json::json!({
                "callback_count": 2,
                "callback_interval_count": 1,
                "callback_interval_us_total": 12000,
                "max_callback_interval_us": 12000,
                "callback_duration_us_total": 20140,
                "max_callback_duration_us": 20100,
                "callback_over_budget_count": 1,
                "callback_duration_p95_us": 20100,
                "callback_duration_p99_us": 20100,
                "mixer_lock_count": 3,
                "mixer_wait_us_total": 22,
                "max_mixer_wait_us": 12,
                "max_callback_mixer_wait_us": 17,
                "callback_mixer_wait_p95_us": 17,
                "callback_mixer_wait_p99_us": 17,
                "max_callback_frames": 960,
                "skipped_callbacks": 0
            })
        );
        assert!(playback.sample().is_none());
    }

    #[test]
    fn timing_percentiles_use_bounded_buckets_and_preserve_tail_maximum() {
        let mut timing = TimingHistogram::default();
        assert_eq!(timing.percentile(0, 95, 0), None);
        for _ in 0..99 {
            timing.record(40);
        }
        timing.record(900_000);
        assert_eq!(timing.percentile(100, 95, 900_000), Some(63));
        assert_eq!(timing.percentile(100, 100, 900_000), Some(900_000));
    }

    fn inbound(ssrc: u32, packets: u64, emitted: u64, delay: f64) -> RtcStats {
        serde_json::from_value(serde_json::json!({
            "type": "inbound-rtp",
            "id": "sdk-stat-identifier",
            "trackIdentifier": "private-track-name",
            "transportId": "private-transport-identifier",
            "kind": "audio",
            "ssrc": ssrc,
            "packetsReceived": packets,
            "packetsLost": -1,
            "jitter": 0.005,
            "jitterBufferEmittedCount": emitted,
            "jitterBufferDelay": delay,
            "concealedSamples": 24,
            "concealmentEvents": 1,
        }))
        .unwrap()
    }

    #[test]
    fn exports_allowlisted_statistics_and_interval_jitter_buffer_residence() {
        let mut tracker = RtcStatsTracker::default();
        let first = tracker.sample(&[], vec![inbound(98765, 10, 100, 2.0)]);
        assert_eq!(first.inbound_audio[0].jitter_buffer_residence_ms, None);

        let second = tracker.sample(&[], vec![inbound(98765, 30, 300, 8.0)]);
        let stream = &second.inbound_audio[0];
        assert_eq!(stream.stream_id, first.inbound_audio[0].stream_id);
        assert_eq!(stream.jitter_buffer_residence_ms, Some(30.0));
        assert_eq!(
            serde_json::to_value(stream).unwrap(),
            serde_json::json!({
                "stream_id": 1,
                "packets_received_total": 30,
                "packets_lost_total": -1,
                "jitter_ms": 5.0,
                "jitter_buffer_residence_ms": 30.0,
                "concealed_samples_total": 24,
                "concealment_events_total": 1,
            })
        );
    }

    #[test]
    fn reset_missing_and_replaced_streams_start_new_baselines() {
        let mut tracker = RtcStatsTracker::default();
        let first = tracker.sample(&[], vec![inbound(1, 10, 100, 2.0)]);
        let reset = tracker.sample(&[], vec![inbound(1, 2, 10, 0.2)]);
        let replaced = tracker.sample(&[], vec![inbound(2, 40, 400, 8.0)]);
        tracker.sample(&[], vec![]);
        let returned = tracker.sample(&[], vec![inbound(2, 50, 500, 10.0)]);
        for (index, sample) in [first, reset, replaced, returned].iter().enumerate() {
            let stream = &sample.inbound_audio[0];
            assert_eq!(stream.stream_id, (index + 1) as u64);
            assert_eq!(stream.jitter_buffer_residence_ms, None);
        }
    }

    #[test]
    fn caps_audio_streams_and_reports_publisher_rtt_only_after_measurement() {
        let mut tracker = RtcStatsTracker::default();
        let publisher = [
            ("video", 1, 0.300),
            ("audio", 0, 0.120),
            ("audio", 2, 0.020),
        ]
        .map(|(kind, measurements, rtt)| {
            serde_json::from_value(serde_json::json!({
                "type": "remote-inbound-rtp",
                "kind": kind,
                "roundTripTime": rtt,
                "roundTripTimeMeasurements": measurements,
            }))
            .unwrap()
        });
        let streams = (0..MAX_INBOUND_AUDIO_STREAMS + 1)
            .map(|index| inbound(index as u32, 10, 100, 2.0))
            .collect();
        let report = tracker.sample(&publisher, streams);
        assert_eq!(report.inbound_audio.len(), MAX_INBOUND_AUDIO_STREAMS);
        assert_eq!(tracker.streams.len(), MAX_INBOUND_AUDIO_STREAMS);
        assert!(report.inbound_audio_truncated);
        assert_eq!(report.publisher_webrtc_rtt_ms, Some(20.0));
    }
}
