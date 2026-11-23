//! Keep DTLS, ICE credentials and audio intact. Only substitute the address of
//! our UDP forwarder for the SFU's single UDP-mux candidate on remote clients.
use livekit_protocol::{signal_response::Message, SignalResponse};
use prost::Message as _;
use std::net::SocketAddrV4;

fn candidate(line: &str, target: SocketAddrV4) -> Option<String> {
    let mut fields: Vec<_> = line.split_ascii_whitespace().map(str::to_owned).collect();
    if fields.len() < 8 || !fields[2].eq_ignore_ascii_case("udp") {
        return None;
    }
    fields[4] = target.ip().to_string();
    fields[5] = target.port().to_string();
    // Strip related addresses; our endpoint is a local host candidate.
    fields[7] = "host".into();
    let mut i = 8;
    while i + 1 < fields.len() {
        if matches!(fields[i].as_str(), "raddr" | "rport") {
            fields.drain(i..i + 2);
        } else {
            i += 2;
        }
    }
    Some(fields.join(" "))
}

pub fn redirect(bytes: &[u8], target: SocketAddrV4) -> Result<Option<Vec<u8>>, String> {
    let mut signal = SignalResponse::decode(bytes).map_err(|e| e.to_string())?;
    match &mut signal.message {
        Some(Message::Offer(desc) | Message::Answer(desc)) => {
            let mut lines = Vec::new();
            for line in desc.sdp.lines() {
                if let Some(raw) = line.strip_prefix("a=candidate:") {
                    if let Some(c) = candidate(raw, target) {
                        lines.push(format!("a=candidate:{c}"));
                    }
                } else {
                    lines.push(line.to_owned());
                }
            }
            desc.sdp = lines.join("\r\n") + "\r\n";
        }
        Some(Message::Trickle(trickle)) if !trickle.candidate_init.is_empty() => {
            let mut json: serde_json::Value =
                serde_json::from_str(&trickle.candidate_init).map_err(|e| e.to_string())?;
            if let Some(line) = json["candidate"].as_str().filter(|s| !s.is_empty()) {
                let Some(c) = candidate(line, target) else {
                    return Ok(None);
                };
                json["candidate"] = c.into();
                trickle.candidate_init = json.to_string();
            }
        }
        _ => return Ok(Some(bytes.to_vec())),
    }
    Ok(Some(signal.encode_to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drops_unreachable_tcp_trickle_and_preserves_end_of_candidates() {
        let target = "127.0.0.1:5000".parse().unwrap();
        let encode = |line: &str| {
            SignalResponse {
                message: Some(Message::Trickle(livekit_protocol::TrickleRequest {
                    candidate_init: serde_json::json!({"candidate":line,"sdpMid":"0"}).to_string(),
                    ..Default::default()
                })),
                ..Default::default()
            }
            .encode_to_vec()
        };
        assert!(redirect(
            &encode("candidate:2 1 tcp 1 192.0.2.5 7881 typ host"),
            target
        )
        .unwrap()
        .is_none());
        let end = encode("");
        assert_eq!(redirect(&end, target).unwrap(), Some(end));
    }
    #[test]
    fn rewrites_only_udp_address_and_preserves_ice_metadata() {
        let dest = "127.0.0.1:5678".parse().unwrap();
        assert_eq!(
            candidate(
                "candidate:1 1 udp 100 10.0.0.7 7882 typ srflx raddr 10.0.0.8 rport 1000 ufrag abc",
                dest
            )
            .unwrap(),
            "candidate:1 1 udp 100 127.0.0.1 5678 typ host ufrag abc"
        );
        assert!(candidate("candidate:1 1 tcp 1 127.0.0.1 7881 typ host", dest).is_none());
    }
}
