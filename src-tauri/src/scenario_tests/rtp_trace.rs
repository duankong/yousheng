use iroh::EndpointId;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::Instant,
};

const CAPACITY: usize = 40_000;
static TRACE: OnceLock<Mutex<Trace>> = OnceLock::new();

enum Event {
    Packet {
        peer: EndpointId,
        flow: u16,
        sequence: u16,
        timestamp: u32,
        ssrc: u32,
    },
    Marker {
        sender: usize,
        receiver: Option<usize>,
        number: usize,
    },
}

struct Trace {
    start: Instant,
    events: Vec<(Instant, &'static str, Event)>,
}

pub(super) struct TraceFile(Option<PathBuf>);

impl TraceFile {
    pub(super) fn start() -> Result<Self, String> {
        let path = std::env::var_os("DUIPIN_TEST_RTP_TRACE").map(PathBuf::from);
        if path.is_some() {
            TRACE
                .set(Mutex::new(Trace {
                    start: Instant::now(),
                    events: Vec::with_capacity(CAPACITY),
                }))
                .map_err(|_| "RTP trace initialized twice")?;
        }
        Ok(Self(path))
    }

    pub(super) fn write(self) -> Result<(), String> {
        let Some(path) = self.0 else { return Ok(()) };
        let trace = TRACE.get().unwrap().lock().unwrap();
        if trace.events.len() == CAPACITY {
            return Err("RTP trace reached its capture limit".into());
        }
        let mut file = BufWriter::new(File::create(path).map_err(super::err)?);
        for (time, stage, event) in &trace.events {
            let us = time.saturating_duration_since(trace.start).as_micros();
            match event {
                Event::Packet {
                    peer,
                    flow,
                    sequence,
                    timestamp,
                    ssrc,
                } => {
                    writeln!(
                        file,
                        "{stage},{us},{peer},{flow},{ssrc},{sequence},{timestamp}"
                    )
                }
                Event::Marker {
                    sender,
                    receiver,
                    number,
                } => {
                    let receiver = receiver.map_or(0, |receiver| receiver + 1);
                    writeln!(
                        file,
                        "{stage},{us},{},{receiver},{}",
                        sender + 1,
                        number + 1
                    )
                }
            }
            .map_err(super::err)?;
        }
        file.flush().map_err(super::err)
    }
}

fn record(at: Instant, stage: &'static str, event: Event) {
    if let Some(trace) = TRACE.get() {
        let mut trace = trace.lock().unwrap();
        if trace.events.len() < CAPACITY {
            trace.events.push((at, stage, event));
        }
    }
}

pub(crate) fn packet(stage: &'static str, peer: EndpointId, flow: u16, data: &[u8]) {
    // RTP and RTCP share the UDP transport. SRTP leaves these RTP header fields clear.
    if TRACE.get().is_none()
        || data.len() < 12
        || data[0] >> 6 != 2
        || (192..=223).contains(&data[1])
    {
        return;
    }
    record(
        Instant::now(),
        stage,
        Event::Packet {
            peer,
            flow,
            sequence: u16::from_be_bytes(data[2..4].try_into().unwrap()),
            timestamp: u32::from_be_bytes(data[4..8].try_into().unwrap()),
            ssrc: u32::from_be_bytes(data[8..12].try_into().unwrap()),
        },
    );
}

pub(super) fn marker(
    stage: &'static str,
    sender: usize,
    receiver: Option<usize>,
    number: usize,
    at: Instant,
) {
    record(
        at,
        stage,
        Event::Marker {
            sender,
            receiver,
            number,
        },
    );
}
