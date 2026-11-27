use super::{attach_remote_track, audio::Playback, Mixer};
use livekit::prelude::RemoteAudioTrack;
use std::sync::{Arc, Mutex};

pub(crate) struct ScenarioPlayback {
    mixer: Arc<Mutex<Mixer>>,
    playback: Playback,
}

impl ScenarioPlayback {
    pub(crate) fn new(tracks: Vec<(RemoteAudioTrack, String)>, rate: u32) -> Self {
        let mixer = Arc::new(Mutex::new(Mixer::default()));
        for (track, player) in tracks {
            attach_remote_track(&mixer, track, player);
        }
        Self {
            mixer,
            playback: Playback::new(rate),
        }
    }

    pub(crate) fn render(&mut self, output: &mut [i16]) {
        self.playback
            .render(output.iter_mut(), 1.0, &self.mixer, |frame, sample| {
                *frame = (sample * 32768.0) as i16;
            });
    }
}
