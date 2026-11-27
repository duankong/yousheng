mod media;
#[cfg(test)]
mod tests;

use crate::{chat::Chat, player_profile};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
    routing::get,
    Router,
};
use media::Media;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{watch, Mutex},
    time::{interval_at, timeout, Instant},
};
use uuid::Uuid;

pub(crate) const ROOM_PROTOCOL: u64 = 2;

pub struct Config {
    pub owner_identity: String,
    pub owner_nickname: String,
    pub invite_key: String,
    pub livekit_key: String,
    pub livekit_secret: String,
    pub livekit_http_url: String,
}

#[derive(Clone)]
struct Shared {
    core: Arc<Mutex<Core>>,
    media: Media,
    stop: watch::Receiver<bool>,
}

struct Membership {
    epoch: String,
    revoked: watch::Sender<()>,
}

struct Member {
    session: Uuid,
    name: String,
    avatar: String,
    playback: Option<std::net::SocketAddrV4>,
    membership: Option<Membership>,
    dirty: watch::Sender<()>,
}

struct Core {
    room_id: String,
    owner: String,
    owner_nickname: String,
    invite: [u8; 32],
    chat: Chat,
    members: BTreeMap<String, Member>,
    avatar_revision: u64,
    // A late SFU kick must finish before the same identity can start voice again.
    pending: BTreeSet<String>,
}

fn player_id(secret: &str) -> Result<String, &'static str> {
    if secret.len() != 64
        || !secret
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("玩家身份无效");
    }
    Ok(format!("{:x}", Sha256::digest(secret.as_bytes())))
}

pub(crate) struct RoomControl(Arc<Mutex<Core>>);

impl RoomControl {
    pub(crate) async fn stop_if_empty(&self, stop: &watch::Sender<bool>) -> Result<(), String> {
        let core = self.0.lock().await;
        if core.members.keys().any(|id| id != &core.owner) {
            return Err("房间里还有好友，请先关闭房间再安装更新".into());
        }
        stop.send_replace(true);
        Ok(())
    }
}

pub(crate) fn router_with_shutdown(
    config: Config,
    stop: watch::Receiver<bool>,
) -> Result<(Router, RoomControl), String> {
    let media = Media::new(&config)?;
    let owner = player_id(&config.owner_identity)?;
    if config.invite_key.len() < 16 {
        return Err("邀请凭证长度不足".into());
    }
    let state = Shared {
        core: Arc::new(Mutex::new(Core {
            room_id: Uuid::new_v4().to_string(),
            owner,
            owner_nickname: config.owner_nickname,
            invite: Sha256::digest(config.invite_key.as_bytes()).into(),
            chat: Chat::default(),
            members: BTreeMap::new(),
            avatar_revision: 0,
            pending: BTreeSet::new(),
        })),
        media,
        stop,
    };
    let control = RoomControl(state.core.clone());
    let router = Router::new()
        .route("/ws", get(upgrade))
        .route("/rtc", get(media::gateway))
        .route("/rtc/{*rest}", get(media::gateway))
        .with_state(state);
    Ok((router, control))
}

async fn upgrade(State(state): State<Shared>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(16 * 1024)
        .max_frame_size(16 * 1024)
        .on_upgrade(move |s| socket(s, state))
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Op {
    Hello {
        invite_key: String,
        nickname: String,
        #[serde(default)]
        room_protocol: u64,
        #[serde(default)]
        avatar: String,
        identity: String,
        playback_address: Option<String>,
    },
    Profile {
        nickname: String,
        avatar: String,
    },
    RetryVoice,
    Chat {
        room_id: String,
        text: String,
    },
}

impl Core {
    fn notify(&self) {
        for member in self.members.values() {
            member.dirty.send_replace(());
        }
    }

    fn login(&mut self, op: Op) -> Result<(String, Uuid, watch::Receiver<()>, bool), &'static str> {
        let Op::Hello {
            invite_key,
            nickname,
            room_protocol,
            avatar,
            identity,
            playback_address,
        } = op
        else {
            return Err("需要邀请凭证");
        };
        if self.invite != <[u8; 32]>::from(Sha256::digest(invite_key.as_bytes())) {
            return Err("邀请凭证无效");
        }
        if room_protocol != ROOM_PROTOCOL {
            return Err("双方的房间协议不兼容，请更新到同一版本后再加入");
        }
        let id = player_id(&identity)?;
        let name = player_profile::nickname(&nickname)?;
        player_profile::validate_avatar(&avatar)?;
        if self.members.len() >= 64 && !self.members.contains_key(&id) {
            return Err("房间人数已满");
        }
        let session = Uuid::new_v4();
        let playback = playback_address
            .map(|address| {
                address
                    .parse::<std::net::SocketAddrV4>()
                    .ok()
                    .filter(|a| a.port() != 0)
                    .ok_or("本地媒体地址无效")
            })
            .transpose()?;
        let (tx, rx) = watch::channel(());
        if self.members.get(&id).map_or("", |m| m.avatar.as_str()) != avatar {
            self.avatar_revision += 1;
        }
        if id == self.owner {
            self.owner_nickname = name.into();
        }
        let old = self.members.insert(
            id.clone(),
            Member {
                session,
                name: name.into(),
                avatar,
                playback,
                membership: None,
                dirty: tx,
            },
        );
        let had_voice = old.is_some_and(|m| m.membership.is_some());
        Ok((id, session, rx, had_voice))
    }

    fn snapshot(
        &self,
        id: &str,
        sent_avatar_revision: Option<u64>,
        last_message: &mut Option<String>,
    ) -> Value {
        let members: Vec<_> = self
            .members
            .iter()
            .map(|(id, m)| {
                let mut member = json!({"id":id, "name":m.name});
                if sent_avatar_revision != Some(self.avatar_revision) {
                    member["avatar"] = json!(m.avatar);
                }
                member
            })
            .collect();
        let history = self.chat.history();
        let mut snapshot = json!({
            "type":"snapshot", "roomProtocol":ROOM_PROTOCOL,
            "roomId":self.room_id, "roomName":format!("{} 的房间", self.owner_nickname),
            "ownerId":self.owner, "ownerNickname":self.owner_nickname, "selfId":id,
            "members":members,
        });
        let mut start = 0;
        // A missing or evicted cursor needs a full window.
        if let Some(index) = last_message
            .as_deref()
            .and_then(|id| history.iter().rposition(|m| m.id == id))
        {
            snapshot["messagesAfter"] = json!(history[index].id);
            start = index + 1;
        }
        *last_message = history.back().map(|m| m.id.clone());
        let messages: Vec<_> = history.iter().skip(start).collect();
        snapshot["messages"] = json!(messages);
        snapshot
    }

    fn apply(&mut self, id: &str, op: Op) -> Result<(), &'static str> {
        match op {
            Op::Hello { .. } => return Err("已完成身份验证"),
            Op::Profile { nickname, avatar } => {
                let name = player_profile::nickname(&nickname)?;
                let member = self.members.get_mut(id).unwrap();
                if member.avatar != avatar {
                    player_profile::validate_avatar(&avatar)?;
                    member.avatar = avatar;
                    self.avatar_revision += 1;
                }
                member.name = name.into();
                if id == self.owner {
                    self.owner_nickname = name.into();
                }
            }
            Op::RetryVoice => {
                let member = &self.members[id];
                if member.membership.is_none() {
                    return Err("有队友加入后才能连接语音");
                }
                member.dirty.send_replace(());
                return Ok(());
            }
            Op::Chat { room_id, text } => {
                if room_id != self.room_id {
                    return Err("只能发言到当前房间");
                }
                let member = &self.members[id];
                self.chat.send(id, &member.name, &text)?;
            }
        }
        self.notify();
        Ok(())
    }

    fn active(&self, id: &str, session: Uuid) -> bool {
        self.members.get(id).is_some_and(|m| m.session == session)
    }
}

impl Shared {
    fn refresh_voice(&self, core: &mut Core) {
        let active = core.members.len() >= 2 && core.members.contains_key(&core.owner);
        let mut revoked = Vec::new();
        for (id, member) in &mut core.members {
            if active {
                if member.membership.is_none() && !core.pending.contains(id) {
                    member.membership = Some(Membership {
                        epoch: Uuid::new_v4().to_string(),
                        revoked: watch::channel(()).0,
                    });
                    member.dirty.send_replace(());
                }
            } else if member.membership.take().is_some() {
                revoked.push(id.clone());
            }
        }
        for id in revoked {
            media::cleanup(self, core, id);
        }
    }
}

async fn send(ws: &mut WebSocket, v: Value) -> bool {
    matches!(
        timeout(
            Duration::from_secs(5),
            ws.send(Message::Text(v.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}

async fn socket(mut ws: WebSocket, state: Shared) {
    let mut stop = state.stop.clone();
    if *stop.borrow() {
        return;
    }
    let hello = tokio::select! {
        _ = stop.changed() => return,
        hello = timeout(Duration::from_secs(8), ws.recv()) => hello,
    };
    let Ok(Some(Ok(Message::Text(text)))) = hello else {
        return;
    };
    let mut core = state.core.lock().await;
    if *stop.borrow() {
        drop(core);
        send(&mut ws, json!({"type":"error","message":"房间已关闭"})).await;
        return;
    }
    let login = serde_json::from_str(&text)
        .map_err(|_| "邀请请求格式无效")
        .and_then(|op| core.login(op));
    let (id, session, mut dirty, had_voice) = match login {
        Ok(result) => result,
        Err(e) => {
            drop(core);
            send(&mut ws, json!({"type":"error","message":e})).await;
            return;
        }
    };
    if had_voice {
        media::cleanup(&state, &mut core, id.clone());
    }
    state.refresh_voice(&mut core);
    core.notify();
    drop(core);
    let mut sent_epoch = None;
    let mut retry_voice = false;
    let mut sent_avatar_revision = None;
    let mut last_message = None;
    let period = Duration::from_secs(15);
    let mut heartbeat = interval_at(Instant::now() + period, period);
    let mut last_seen = Instant::now();
    loop {
        tokio::select! {
            _ = stop.changed() => {
                let _ = timeout(
                    Duration::from_millis(200),
                    send(&mut ws, json!({"type":"offline","message":"房间已关闭"})),
                ).await;
                break;
            }
            changed = dirty.changed() => {
                if changed.is_err() { break; }
                let core = state.core.lock().await;
                if !core.active(&id, session) { break; }
                if *stop.borrow() { continue; }
                let snapshot = core.snapshot(&id, sent_avatar_revision, &mut last_message);
                sent_avatar_revision = Some(core.avatar_revision);
                let member = &core.members[&id];
                let epoch = member.membership.as_ref().map(|m| m.epoch.clone());
                let voice = if epoch != sent_epoch || (retry_voice && epoch.is_some()) {
                    Some(match &member.membership {
                        Some(m) => state.media.voice(&core.room_id, &id, &member.name, m),
                        None => json!({"type":"voice","roomId":core.room_id,"token":null}),
                    })
                } else { None };
                sent_epoch = epoch;
                retry_voice = false;
                drop(core);
                if !send(&mut ws, snapshot).await { break; }
                if let Some(event) = voice {
                    if !send(&mut ws, event).await { break; }
                }
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > Duration::from_secs(45) ||
                    !matches!(timeout(Duration::from_secs(5), ws.send(Message::Ping(Default::default()))).await, Ok(Ok(()))) { break; }
            }
            incoming = ws.recv() => {
                last_seen = Instant::now();
                let text = match incoming {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => break,
                };
                let request = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
                let request_id = request.get("requestId").and_then(Value::as_u64);
                let op = serde_json::from_value::<Op>(request).map_err(|_| "不支持的房间操作");
                let mut core = state.core.lock().await;
                if !core.active(&id, session) { break; }
                if *stop.borrow() { continue; }
                let result = op.and_then(|op| {
                    let retry = matches!(&op, Op::RetryVoice);
                    core.apply(&id, op)?;
                    retry_voice |= retry;
                    Ok(())
                });
                drop(core);
                if let Some(n) = request_id {
                    let reply=json!({"type":"result","requestId":n,"error":result.err()});
                    if !send(&mut ws,reply).await { break; }
                } else if let Err(e) = result {
                    if !send(&mut ws, json!({"type":"error","message":e})).await { break; }
                }
            }
        }
    }
    let mut core = state.core.lock().await;
    if core.active(&id, session) {
        let member = core.members.remove(&id).unwrap();
        if !member.avatar.is_empty() {
            core.avatar_revision += 1;
        }
        if member.membership.is_some() {
            drop(member);
            media::cleanup(&state, &mut core, id);
        }
        state.refresh_voice(&mut core);
        core.notify();
    }
}
