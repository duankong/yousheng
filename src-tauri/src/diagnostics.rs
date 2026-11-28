use crate::{network::TransportEvent, voice::VoiceEvent};
use serde::Serialize;
use std::{
    collections::VecDeque,
    fs::File,
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const RETENTION_MS: u64 = 30 * 60 * 1000;
const MAX_EVENTS: usize = 8192;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Host,
    Guest,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RoomPhase {
    Opening,
    StartHost,
    ConnectTunnel,
    DialPeer,
    AuthenticateInvite,
    AcceptRoom,
    OpenRoomSocket,
    FirstRoomSnapshot,
    Ready,
    Disconnected,
    Closed,
}

// Accept only measurements and controlled labels. Keep arbitrary errors, packet
// payloads, addresses, and real room and player identities out of the report.
#[derive(Clone, Serialize)]
#[serde(tag = "category", content = "data", rename_all = "snake_case")]
pub(crate) enum Event {
    Room {
        role: Role,
        phase: RoomPhase,
        duration_ms: Option<u64>,
        succeeded: Option<bool>,
    },
    Transport(TransportEvent),
    Voice(VoiceEvent),
}

#[derive(Clone, Serialize)]
struct Observation {
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    room: Option<u64>,
    #[serde(flatten)]
    event: Event,
}

#[derive(Default)]
struct History {
    events: VecDeque<Observation>,
}

impl History {
    fn expire(&mut self, now: u64) {
        while self
            .events
            .front()
            .is_some_and(|first| now.saturating_sub(first.elapsed_ms) > RETENTION_MS)
        {
            self.events.pop_front();
        }
    }

    fn push(&mut self, now: u64, room: Option<u64>, event: Event) {
        self.expire(now);
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(Observation {
            elapsed_ms: now,
            room,
            event,
        });
    }
}

struct Recorder {
    started: Instant,
    started_at_unix_ms: u64,
    ids: AtomicU64,
    history: Mutex<History>,
}

#[derive(Clone)]
pub(crate) struct Diagnostics {
    recorder: Arc<Recorder>,
    room: Option<u64>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            recorder: Arc::new(Recorder {
                started: Instant::now(),
                started_at_unix_ms: unix_ms(),
                ids: AtomicU64::new(1),
                history: Mutex::new(History::default()),
            }),
            room: None,
        }
    }
}

impl Diagnostics {
    pub(crate) fn next_id(&self) -> u64 {
        self.recorder.ids.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn record(&self, event: Event) {
        let mut history = self.recorder.history.lock().unwrap();
        history.push(
            self.recorder.started.elapsed().as_millis() as u64,
            self.room,
            event,
        );
    }

    pub(crate) fn room(&self, role: Role) -> RoomDiagnostics {
        let room = RoomDiagnostics {
            diagnostics: Self {
                recorder: self.recorder.clone(),
                room: Some(self.next_id()),
            },
            role,
        };
        room.mark(RoomPhase::Opening);
        room
    }

    pub(crate) fn export(&self, directory: &Path) -> io::Result<PathBuf> {
        let events = {
            let mut history = self.recorder.history.lock().unwrap();
            history.expire(self.recorder.started.elapsed().as_millis() as u64);
            history.events.iter().cloned().collect()
        };
        let exported_at_unix_ms = unix_ms();
        let report = Report {
            schema_version: 1,
            app_version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            started_at_unix_ms: self.recorder.started_at_unix_ms,
            exported_at_unix_ms,
            retention_ms: RETENTION_MS,
            events,
        };
        let data = serde_json::to_vec_pretty(&report).map_err(io::Error::other)?;
        let path = directory.join(format!(
            "GroveCall-diagnostics-{exported_at_unix_ms}-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let mut file = File::create_new(&path)?;
        if let Err(error) = file.write_all(&data) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        Ok(path)
    }
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    app_version: &'static str,
    os: &'static str,
    arch: &'static str,
    started_at_unix_ms: u64,
    exported_at_unix_ms: u64,
    retention_ms: u64,
    events: Vec<Observation>,
}

pub(crate) struct RoomDiagnostics {
    diagnostics: Diagnostics,
    role: Role,
}

struct RoomMeasurement<'a> {
    room: &'a RoomDiagnostics,
    phase: RoomPhase,
    started: Instant,
    succeeded: Option<bool>,
}

impl Drop for RoomMeasurement<'_> {
    fn drop(&mut self) {
        self.room.diagnostics.record(Event::Room {
            role: self.room.role,
            phase: self.phase,
            duration_ms: Some(self.started.elapsed().as_millis() as u64),
            succeeded: self.succeeded,
        });
    }
}

impl RoomDiagnostics {
    pub(crate) fn handle(&self) -> Diagnostics {
        self.diagnostics.clone()
    }

    pub(crate) fn mark(&self, phase: RoomPhase) {
        self.diagnostics.record(Event::Room {
            role: self.role,
            phase,
            duration_ms: None,
            succeeded: None,
        });
    }

    pub(crate) async fn measure<T, E>(
        &self,
        phase: RoomPhase,
        operation: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        // A timed event with no result means the in-flight phase was interrupted.
        let mut measurement = RoomMeasurement {
            room: self,
            phase,
            started: Instant::now(),
            succeeded: None,
        };
        let result = operation.await;
        measurement.succeeded = Some(result.is_ok());
        result
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation() -> Event {
        Event::Room {
            role: Role::Guest,
            phase: RoomPhase::Ready,
            duration_ms: None,
            succeeded: None,
        }
    }

    #[test]
    fn history_expires_by_age_and_is_bounded_by_event_count() {
        let mut history = History::default();
        for room in 0..=MAX_EVENTS as u64 {
            history.push(0, Some(room), observation());
        }
        assert_eq!(history.events.len(), MAX_EVENTS);
        assert_eq!(history.events.front().unwrap().room, Some(1));
        history.push(RETENTION_MS + 1, Some(0), observation());
        assert_eq!(history.events.len(), 1);
        history.expire(2 * RETENTION_MS + 2);
        assert!(history.events.is_empty());
    }

    #[tokio::test]
    async fn dropping_an_in_flight_room_phase_records_interruption() {
        let diagnostics = Diagnostics::default();
        let room = diagnostics.room(Role::Guest);
        let mut phase = Box::pin(room.measure(
            RoomPhase::DialPeer,
            std::future::pending::<Result<(), ()>>(),
        ));
        assert!(futures_util::poll!(&mut phase).is_pending());
        drop(phase);
        let history = diagnostics.recorder.history.lock().unwrap();
        assert!(matches!(
            history.events.back().unwrap().event,
            Event::Room {
                phase: RoomPhase::DialPeer,
                duration_ms: Some(_),
                succeeded: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn export_keeps_finished_room_events_and_excludes_raw_errors() {
        let directory = tempfile::tempdir().unwrap();
        let diagnostics = Diagnostics::default();
        let room = diagnostics.room(Role::Guest);
        let sensitive = "duipin://join/secret-ticket";
        let result = room
            .measure(RoomPhase::ConnectTunnel, async { Err::<(), _>(sensitive) })
            .await;
        assert_eq!(result, Err(sensitive));
        room.mark(RoomPhase::Closed);
        drop(room);
        let report = diagnostics.export(directory.path()).unwrap();
        let second_report = diagnostics.export(directory.path()).unwrap();
        assert_ne!(report, second_report);
        assert_eq!(report.extension().unwrap(), "json");
        let text = std::fs::read_to_string(report).unwrap();
        assert!(!text.contains(sensitive));
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["events"][1]["data"]["phase"], "connect_tunnel");
        assert_eq!(json["events"][1]["data"]["succeeded"], false);
        assert_eq!(json["events"][2]["data"]["phase"], "closed");
        assert_eq!(json["events"].as_array().unwrap().len(), 3);
        assert_eq!(json["events"][0]["room"], json["events"][2]["room"]);
    }
}
