mod media;
#[cfg(test)]
mod tests;

use crate::{channels::Channels, chat::Chat, player_profile};
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
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{watch, Mutex},
    time::{interval_at, timeout, Instant},
};
use uuid::Uuid;

pub struct Config {
    pub database: PathBuf,
    pub owner_identity: String,
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
    room: String,
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

impl Member {
    fn room(&self) -> Option<&str> {
        self.membership.as_ref().map(|m| m.room.as_str())
    }
}

struct Core {
    owner: String,
    invite: [u8; 32],
    channels: Channels,
    chat: Chat,
    members: BTreeMap<String, Member>,
    avatar_revision: u64,
    // Removing a sender on SFU acknowledgement wakes joins waiting for this player and room.
    pending: BTreeMap<(String, String), watch::Sender<()>>,
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

pub fn router_with_shutdown(config: Config, stop: watch::Receiver<bool>) -> Result<Router, String> {
    let media = Media::new(&config)?;
    let owner = player_id(&config.owner_identity)?;
    if config.invite_key.len() < 16 {
        return Err("邀请凭证长度不足".into());
    }
    let state = Shared {
        core: Arc::new(Mutex::new(Core {
            owner,
            invite: Sha256::digest(config.invite_key.as_bytes()).into(),
            channels: Channels::open(&config.database).map_err(|e| e.to_string())?,
            chat: Chat::default(),
            members: BTreeMap::new(),
            avatar_revision: 0,
            pending: BTreeMap::new(),
        })),
        media,
        stop,
    };
    Ok(Router::new()
        .route("/ws", get(upgrade))
        .route("/rtc", get(media::gateway))
        .route("/rtc/{*rest}", get(media::gateway))
        .with_state(state))
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
        avatar: String,
        identity: String,
        playback_address: Option<String>,
    },
    Profile {
        nickname: String,
        avatar: String,
    },
    Create {
        name: String,
    },
    Join {
        channel_id: String,
    },
    Leave,
    Delete {
        channel_id: String,
    },
    Chat {
        channel_id: String,
        text: String,
    },
}

impl Core {
    fn notify(&self, room: Option<&str>) {
        for member in self
            .members
            .values()
            .filter(|m| room.is_none() || m.room() == room)
        {
            member.dirty.send_replace(());
        }
    }

    fn login(
        &mut self,
        op: Op,
    ) -> Result<(String, Uuid, watch::Receiver<()>, Option<String>), &'static str> {
        let Op::Hello {
            invite_key,
            nickname,
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
        let id = player_id(&identity)?;
        let name = player_profile::nickname(&nickname)?;
        player_profile::validate_avatar(&avatar)?;
        if self.members.len() >= 64 && !self.members.contains_key(&id) {
            return Err("大厅人数已满");
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
        let previous_room = old.and_then(|m| m.membership.map(|m| m.room));
        self.notify(None);
        Ok((id, session, rx, previous_room))
    }

    fn snapshot(&self, id: &str, sent_avatar_revision: Option<u64>) -> Value {
        let me = &self.members[id];
        let admin = self.owner == id;
        let channels: Vec<_> = self.channels.list.iter().map(|c|
            json!({"id":c.id, "name":c.name, "ownerId":c.owner, "canDelete":c.can_delete(id, admin)})).collect();
        let members: Vec<_> = self
            .members
            .iter()
            .map(|(id, m)| {
                let mut member = json!({"id":id, "name":m.name, "channelId":m.room()});
                if sent_avatar_revision != Some(self.avatar_revision) {
                    member["avatar"] = json!(m.avatar);
                }
                member
            })
            .collect();
        json!({"type":"snapshot", "selfId":id, "admin":admin, "channels":channels, "members":members,
            "channelId":me.room(), "messages":self.chat.history(me.room())})
    }

    fn apply(&mut self, id: &str, op: Op) -> Result<Cleanup, &'static str> {
        let mut cleanup = Cleanup::None;
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
            }
            Op::Create { name } => {
                self.channels.create(id, &name)?;
            }
            Op::Join { channel_id } => {
                self.channels.find(&channel_id)?;
                if self.pending.contains_key(&(channel_id.clone(), id.into())) {
                    return Err("上一次通话正在结束，请稍后重试");
                }
                let member = self.members.get_mut(id).unwrap();
                if member.room() != Some(&channel_id) {
                    let (revoked, _) = watch::channel(());
                    let old = member.membership.replace(Membership {
                        room: channel_id,
                        epoch: Uuid::new_v4().to_string(),
                        revoked,
                    });
                    if let Some(old) = old {
                        cleanup = Cleanup::Participant(old.room, id.into());
                    }
                }
            }
            Op::Leave => {
                if let Some(old) = self.members.get_mut(id).unwrap().membership.take() {
                    cleanup = Cleanup::Participant(old.room, id.into());
                }
            }
            Op::Delete { channel_id } => {
                self.channels.delete(&channel_id, id, self.owner == id)?;
                self.chat.clear(&channel_id);
                for m in self
                    .members
                    .values_mut()
                    .filter(|m| m.room() == Some(&channel_id))
                {
                    m.membership = None;
                }
                cleanup = Cleanup::Room(channel_id);
            }
            Op::Chat { channel_id, text } => {
                let member = &self.members[id];
                self.chat
                    .send(member.room(), &channel_id, id, &member.name, &text)?;
                self.notify(Some(&channel_id));
                return Ok(cleanup);
            }
        }
        self.notify(None);
        Ok(cleanup)
    }

    fn active(&self, id: &str, session: Uuid) -> bool {
        self.members.get(id).is_some_and(|m| m.session == session)
    }
}

enum Cleanup {
    None,
    Participant(String, String),
    Room(String),
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
    let login = serde_json::from_str(&text)
        .map_err(|_| "邀请请求格式无效")
        .and_then(|op| core.login(op));
    let (id, session, mut dirty, old) = match login {
        Ok(result) => result,
        Err(e) => {
            drop(core);
            send(&mut ws, json!({"type":"error","message":e})).await;
            return;
        }
    };
    if let Some(room) = old {
        media::cleanup(&state, &mut core, Cleanup::Participant(room, id.clone()));
    }
    drop(core);
    let mut sent_epoch = None;
    let mut sent_avatar_revision = None;
    let period = Duration::from_secs(15);
    let mut heartbeat = interval_at(Instant::now() + period, period);
    let mut last_seen = Instant::now();
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            changed = dirty.changed() => {
                if changed.is_err() { break; }
                let core = state.core.lock().await;
                if !core.active(&id, session) { break; }
                let snapshot = core.snapshot(&id, sent_avatar_revision);
                sent_avatar_revision = Some(core.avatar_revision);
                let member = &core.members[&id];
                let epoch = member.membership.as_ref().map(|m| m.epoch.clone());
                let voice = if epoch != sent_epoch {
                    member.membership.as_ref().map(|m| state.media.voice(&id, &member.name, m))
                } else { None };
                sent_epoch = epoch;
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
                let op = serde_json::from_value::<Op>(request).map_err(|_| "不支持的大厅操作");
                let mut core = state.core.lock().await;
                if let Ok(Op::Join { channel_id }) = &op {
                    let pending = core.pending.get(&(channel_id.clone(), id.clone()));
                    if let Some(mut cleared) = pending.map(watch::Sender::subscribe) {
                        drop(core);
                        tokio::select! {
                            _ = stop.changed() => break,
                            _ = timeout(Duration::from_secs(3), cleared.changed()) => (),
                        }
                        core = state.core.lock().await;
                    }
                }
                if *stop.borrow() || !core.active(&id, session) { break; }
                let result = op.and_then(|op| {
                    let retry_voice = matches!(&op, Op::Join { .. });
                    let cleanup = core.apply(&id, op)?;
                    if retry_voice { sent_epoch = None; }
                    media::cleanup(&state, &mut core, cleanup);
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
        if let Some(old) = member.membership {
            media::cleanup(&state, &mut core, Cleanup::Participant(old.room, id));
        }
        core.notify(None);
    }
}
