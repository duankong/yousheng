use crate::{
    chat::{ChatMessage, HISTORY_LIMIT},
    lobby::ROOM_PROTOCOL,
    voice::VoiceEngine,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};
use tauri::{AppHandle, Emitter};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{timeout, timeout_at, Instant},
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type Reply = oneshot::Sender<Result<(), String>>;

const QUEUE_TIMEOUT: &str = "操作排队超时，未发送";
const DISCONNECTED: &str = "房间连接已断开";

struct Command {
    action: Value,
    deadline: Instant,
    started: oneshot::Sender<()>,
    reply: Reply,
}

impl Command {
    fn begin(self) -> Option<(Value, Reply)> {
        if Instant::now() >= self.deadline {
            let _ = self.reply.send(Err(QUEUE_TIMEOUT.into()));
            return None;
        }
        self.started.send(()).ok()?;
        Some((self.action, self.reply))
    }
}

#[derive(Clone)]
pub(crate) struct ClientCommands(mpsc::Sender<Command>);

impl ClientCommands {
    pub(crate) fn send(&self, action: Value) -> impl Future<Output = Result<(), String>> + 'static {
        self.send_before(action, Instant::now() + Duration::from_secs(8))
    }

    fn send_before(
        &self,
        action: Value,
        deadline: Instant,
    ) -> impl Future<Output = Result<(), String>> + 'static {
        let (started, mut dispatch) = oneshot::channel();
        let (reply, confirmation) = oneshot::channel();
        let queued = self
            .0
            .try_send(Command {
                action,
                deadline,
                started,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => "房间操作队列已满，未发送".to_owned(),
                mpsc::error::TrySendError::Closed(_) => DISCONNECTED.to_owned(),
            });
        async move {
            queued?;
            match timeout_at(deadline, confirmation).await {
                Ok(result) => result.map_err(|_| DISCONNECTED.to_owned())?,
                Err(_) => {
                    // Closing first decides a simultaneous timeout/dispatch without a late send.
                    dispatch.close();
                    if dispatch.try_recv().is_ok() {
                        Err("主机未确认操作，请查看房间记录".into())
                    } else {
                        Err(QUEUE_TIMEOUT.into())
                    }
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSnapshot<'a> {
    room_id: &'a str,
    room_name: &'a str,
    owner_id: &'a str,
    owner_nickname: &'a str,
    self_id: &'a str,
    #[serde(borrow)]
    members: Vec<WireMember<'a>>,
}

#[derive(Deserialize)]
struct WireMember<'a> {
    id: &'a str,
    name: &'a str,
    #[serde(rename = "avatar", default, deserialize_with = "present_avatar")]
    _avatar: Option<&'a str>,
}

fn present_avatar<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<&'de str>, D::Error> {
    <&str>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
struct WireChat<'a> {
    #[serde(borrow)]
    messages: Vec<ChatMessage<&'a str>>,
}

#[derive(Default)]
struct ChatCursor {
    last_id: Option<String>,
    len: usize,
}

fn decode_snapshot(
    event: &mut Value,
    room: Option<&str>,
    chat_cursor: &mut ChatCursor,
) -> Result<(), String> {
    if event["type"] != "snapshot" {
        return Err(event["message"].as_str().unwrap_or("加入房间失败").into());
    }
    if event["roomProtocol"].as_u64() != Some(ROOM_PROTOCOL) {
        return Err("房间版本不兼容，请和房主更新到相同版本后再试".into());
    }

    const MAX_JS_DATE_MS: u64 = 8_640_000_000_000_000;
    let snapshot = WireSnapshot::deserialize(&*event).map_err(|_| "房间状态无效")?;
    let room_fields = [
        snapshot.room_id,
        snapshot.room_name,
        snapshot.owner_id,
        snapshot.owner_nickname,
        snapshot.self_id,
    ];
    let mut member_ids = HashSet::with_capacity(snapshot.members.len());
    if room_fields.iter().any(|field| field.is_empty())
        || room.is_some_and(|expected| expected != snapshot.room_id)
        || snapshot.members.iter().any(|member| {
            member.id.is_empty() || member.name.is_empty() || !member_ids.insert(member.id)
        })
        || !member_ids.contains(snapshot.self_id)
        || event["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|member| !member.is_object())
    {
        return Err("房间状态无效".into());
    }
    let chat = WireChat::deserialize(&*event).map_err(|_| "房间聊天记录同步失败")?;
    if chat.messages.iter().any(|message| {
        message.time > MAX_JS_DATE_MS
            || [
                message.id,
                message.author_id,
                message.author_name,
                message.text,
            ]
            .iter()
            .any(|field| field.is_empty())
    }) || event["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| !message.is_object())
        || !retain_chat_delta(event, chat_cursor)
    {
        return Err("房间聊天记录同步失败".into());
    }
    Ok(())
}

// Keep the cursor and window size on the Rust side. The UI expands validated deltas.
fn retain_chat_delta(snapshot: &mut Value, cursor: &mut ChatCursor) -> bool {
    let Some(fields) = snapshot.as_object_mut() else {
        return false;
    };
    let delta = match fields.get("messagesAfter") {
        Some(after)
            if after
                .as_str()
                .is_some_and(|id| Some(id) == cursor.last_id.as_deref()) =>
        {
            true
        }
        Some(_) => return false,
        None => false,
    };
    let Some(messages) = fields.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    messages.drain(..messages.len().saturating_sub(HISTORY_LIMIT));
    if !delta || !messages.is_empty() {
        cursor.last_id = messages
            .last()
            .and_then(|message| message["id"].as_str())
            .map(str::to_owned);
    }
    let previous_len = if delta { cursor.len } else { 0 };
    cursor.len = (previous_len + messages.len()).min(HISTORY_LIMIT);
    // This is desktop metadata calculated locally from the canonical chat limit.
    if delta {
        fields.insert("retainedMessageCount".into(), cursor.len.into());
    } else {
        fields.remove("retainedMessageCount");
    }
    true
}

pub struct Client {
    pub(crate) commands: ClientCommands,
    socket: JoinHandle<()>,
    audio: JoinHandle<()>,
}

pub(crate) struct AcceptedRoom {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    base: String,
    initial: Value,
    chat_cursor: ChatCursor,
}

pub(crate) fn emit_room(app: &AppHandle, session_id: &str, mut event: Value) {
    event["sessionId"] = session_id.into();
    let _ = app.emit("lobby-event", event);
}

impl AcceptedRoom {
    pub(crate) async fn connect(port: u16, mut hello: Value) -> Result<Self, String> {
        let base = format!("ws://127.0.0.1:{port}");
        let (mut ws, _) = timeout(Duration::from_secs(10), connect_async(format!("{base}/ws")))
            .await
            .map_err(|_| "房间连接超时".to_owned())?
            .map_err(|e| format!("无法连接房间：{e}"))?;
        hello["roomProtocol"] = ROOM_PROTOCOL.into();
        ws.send(Message::Text(hello.to_string().into()))
            .await
            .map_err(|e| e.to_string())?;
        let first = timeout(Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| "房间没有回应".to_owned())?
            .ok_or("房间已关闭")?
            .map_err(|e| e.to_string())?;
        let mut initial: Value = serde_json::from_str(first.to_text().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let mut chat_cursor = ChatCursor::default();
        decode_snapshot(&mut initial, None, &mut chat_cursor)?;
        Ok(Self {
            ws,
            base,
            initial,
            chat_cursor,
        })
    }

    pub(crate) fn snapshot(&self) -> &Value {
        &self.initial
    }

    pub(crate) fn activate(
        self,
        app: AppHandle,
        voice: Arc<VoiceEngine>,
        session_id: String,
        diagnostics: crate::diagnostics::Diagnostics,
        disconnected: impl FnOnce(String) + Send + 'static,
    ) -> Client {
        let Self {
            mut ws,
            base,
            initial,
            mut chat_cursor,
        } = self;
        let room = initial["roomId"].as_str().unwrap().to_owned();
        emit_room(&app, &session_id, initial);
        let (commands, mut outgoing) = mpsc::channel::<Command>(64);
        let (target, mut wanted) = watch::channel::<Option<String>>(None);
        let audio = tokio::spawn(async move {
            let mut changed = wanted.changed().await;
            while changed.is_ok() {
                let token = wanted.borrow_and_update().clone();
                voice.disconnect().await;
                if let Some(token) = token {
                    tokio::select! {
                        biased;
                        update=wanted.changed()=>{changed=update; continue;},
                          ()=voice.connect(&base,&token,&diagnostics)=>{},
                    }
                }
                changed = wanted.changed().await;
            }
            voice.disconnect().await;
        });
        let socket = tokio::spawn(async move {
            let mut sequence = 0u64;
            let mut waiting: HashMap<u64, Reply> = HashMap::new();
            let mut closure = "房间连接已断开，可从历史房间重新加入".to_owned();
            loop {
                tokio::select! {
                    next=outgoing.recv()=>match next {
                        Some(command)=>{
                            let Some((mut data,reply))=command.begin() else {continue};
                            let Some(fields)=data.as_object_mut() else {let _=reply.send(Err("无效的房间操作".into()));continue};
                            waiting.retain(|_,r|!r.is_closed());
                            sequence+=1;
                            fields.insert("requestId".into(),sequence.into());
                            waiting.insert(sequence,reply);
                            if ws.send(Message::Text(data.to_string().into())).await.is_err(){break}
                        },
                        None=>break,
                    },
                    next=ws.next()=>match next {
                        Some(Ok(Message::Text(data)))=>{
                            let Ok(mut event)=serde_json::from_str::<Value>(&data) else {break};
                            match event["type"].as_str() {
                                Some("result")=>if let Some(reply)=event["requestId"].as_u64().and_then(|id|waiting.remove(&id)){
                                    let result=event["error"].as_str().map_or(Ok(()),|e|Err(e.to_owned()));
                                    let _=reply.send(result);
                                },
                                Some("snapshot")=>{
                                    if let Err(message) = decode_snapshot(&mut event, Some(&room), &mut chat_cursor) {
                                        closure = message;
                                        break;
                                    }
                                    emit_room(&app, &session_id, event);
                                }
                                Some("voice")=>if event["roomId"] == room {
                                    target.send_replace(event["token"].as_str().map(str::to_owned));
                                },
                                Some("error")=>emit_room(&app, &session_id, event),
                                Some("offline")=>{
                                    if let Some(message)=event["message"].as_str() { closure=message.to_owned(); }
                                    break;
                                }
                                _=>{}
                            }
                        }
                        Some(Ok(Message::Ping(p)))=>if ws.send(Message::Pong(p)).await.is_err(){break},
                        Some(Ok(Message::Pong(_)))=>{},
                        _=>break
                    }
                }
            }
            drop(outgoing);
            drop(target);
            disconnected(closure);
        });
        Client {
            commands: ClientCommands(commands),
            socket,
            audio,
        }
    }
}

impl Client {
    pub(crate) fn is_connected(&self) -> bool {
        !self.commands.0.is_closed()
    }

    pub async fn close(self) {
        self.socket.abort();
        let _ = self.socket.await;
        // Dropping the socket's voice sender lets the audio task finish old-room cleanup.
        let _ = self.audio.await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) struct CommandPeer(mpsc::Receiver<Command>);

    impl CommandPeer {
        pub(crate) async fn recv(&mut self) -> Option<(Value, Reply)> {
            self.0.recv().await?.begin()
        }
    }

    pub(crate) fn command_peer() -> (Client, CommandPeer) {
        let (commands, peer) = mpsc::channel(64);
        (
            Client {
                commands: ClientCommands(commands),
                socket: tokio::spawn(async {}),
                audio: tokio::spawn(async {}),
            },
            CommandPeer(peer),
        )
    }

    pub(crate) fn pause_audio_cleanup(client: &mut Client) -> oneshot::Sender<()> {
        let (resume, wait) = oneshot::channel();
        client.audio = tokio::spawn(async move {
            let _ = wait.await;
        });
        resume
    }

    #[tokio::test]
    async fn closing_room_waits_for_voice_cleanup_already_in_progress() {
        let (commands, _peer) = mpsc::channel(1);
        let (target, mut wanted) = watch::channel(());
        let (started, cleaning) = oneshot::channel();
        let (resume, wait) = oneshot::channel();
        let (done, cleaned) = oneshot::channel();
        let socket = tokio::spawn(async move { drop(target) });
        let audio = tokio::spawn(async move {
            assert!(wanted.changed().await.is_err());
            started.send(()).unwrap();
            wait.await.unwrap();
            done.send(()).unwrap();
        });
        let client = Client {
            commands: ClientCommands(commands),
            socket,
            audio,
        };

        cleaning.await.unwrap();
        let mut close = Box::pin(client.close());
        assert!(futures_util::poll!(&mut close).is_pending());
        let _ = resume.send(());
        close.await;
        cleaned
            .await
            .expect("voice cleanup must finish before a new room can start");
    }

    #[tokio::test]
    async fn full_cancelled_and_expired_commands_never_reach_the_peer() {
        let (sender, mut queue) = mpsc::channel(1);
        let commands = ClientCommands(sender);
        let expired = commands.send_before(json!("expired"), Instant::now());
        assert_eq!(
            commands.send(json!("overflow")).await.unwrap_err(),
            "房间操作队列已满，未发送"
        );
        assert_eq!(expired.await.unwrap_err(), QUEUE_TIMEOUT);
        assert!(queue.recv().await.unwrap().begin().is_none());

        let expired = commands.send_before(json!("late"), Instant::now());
        assert!(queue.recv().await.unwrap().begin().is_none());
        assert_eq!(expired.await.unwrap_err(), QUEUE_TIMEOUT);

        drop(commands.send(json!("cancelled")));
        assert!(queue.recv().await.unwrap().begin().is_none());
        assert!(queue.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatched_command_without_a_reply_reports_unknown_outcome() {
        let (commands, mut peer) = command_peer();
        let result = commands
            .commands
            .send_before(json!("sent"), Instant::now() + Duration::from_millis(100));
        let (action, _pending_ack) = peer.recv().await.unwrap();
        assert_eq!(action, "sent");
        assert_eq!(result.await.unwrap_err(), "主机未确认操作，请查看房间记录");
    }

    #[test]
    fn chat_deltas_preserve_order_roll_over_and_accept_full_resync() {
        let mut cursor = ChatCursor::default();
        let mut full = wire_snapshot();
        full["messages"] = Value::Array((0..102).map(|n| wire_message(&n.to_string())).collect());
        full["retainedMessageCount"] = json!(102);
        decode_snapshot(&mut full, None, &mut cursor).unwrap();
        assert_eq!(full["messages"].as_array().unwrap().len(), 100);
        assert_eq!(full["messages"][0]["id"], "2");
        assert!(full.get("retainedMessageCount").is_none());

        let mut delta = wire_snapshot();
        delta["messagesAfter"] = json!("101");
        delta["messages"] = json!([wire_message("102"), wire_message("103")]);
        delta["retainedMessageCount"] = json!(2);
        decode_snapshot(&mut delta, Some("room"), &mut cursor).unwrap();
        assert_eq!(
            delta["messages"],
            json!([wire_message("102"), wire_message("103")])
        );
        assert_eq!(delta["messagesAfter"], "101");
        assert_eq!(delta["retainedMessageCount"], 100);

        let mut unchanged = wire_snapshot();
        unchanged["messagesAfter"] = json!("103");
        unchanged["messages"] = json!([]);
        decode_snapshot(&mut unchanged, Some("room"), &mut cursor).unwrap();
        assert_eq!(unchanged["messages"], json!([]));
        assert_eq!(unchanged["retainedMessageCount"], 100);

        let mut stale = wire_snapshot();
        stale["messagesAfter"] = json!("101");
        assert!(decode_snapshot(&mut stale, Some("room"), &mut cursor).is_err());
        assert_eq!(cursor.last_id.as_deref(), Some("103"));
        assert_eq!(cursor.len, 100);

        let mut resync = wire_snapshot();
        decode_snapshot(&mut resync, Some("room"), &mut cursor).unwrap();
        assert_eq!(cursor.last_id.as_deref(), Some("message"));
        assert_eq!(cursor.len, 1);
    }

    fn wire_message(id: &str) -> Value {
        json!({"id":id,"authorId":"host","authorName":"Host","text":"hello","time":42})
    }

    fn wire_snapshot() -> Value {
        json!({
            "type": "snapshot", "roomProtocol": 2,
            "roomId": "room", "roomName": "Host's room", "ownerId": "host",
            "ownerNickname": "Host", "selfId": "guest",
            "members": [
                {"id": "host", "name": "Host", "avatar": "picture"},
                {"id": "guest", "name": "Guest", "avatar": ""},
            ],
            "messages": [wire_message("message")],
        })
    }

    async fn connect_to_snapshot(event: Value) -> Result<AcceptedRoom, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.next().await.unwrap().unwrap();
            ws.send(Message::Text(event.to_string().into()))
                .await
                .unwrap();
        });
        let result = AcceptedRoom::connect(port, json!({"type":"hello"})).await;
        host.await.unwrap();
        result
    }

    #[tokio::test]
    async fn initial_snapshot_rejects_malformed_members_and_chat() {
        let mut invalid_member = wire_snapshot();
        invalid_member["members"][0]["avatar"] = json!(null);
        let mut invalid_chat = wire_snapshot();
        invalid_chat["messages"][0]["time"] = json!(8_640_000_000_000_001_u64);

        for event in [invalid_member, invalid_chat] {
            assert!(
                connect_to_snapshot(event.clone()).await.is_err(),
                "accepted malformed initial snapshot: {event}"
            );
        }
    }

    #[tokio::test]
    async fn initial_snapshot_preserves_join_and_protocol_errors() {
        let result = connect_to_snapshot(json!({"type":"error","message":"invite rejected"})).await;
        assert_eq!(result.err().unwrap(), "invite rejected");

        let mut wrong_protocol = wire_snapshot();
        wrong_protocol["roomProtocol"] = json!(1);
        let result = connect_to_snapshot(wrong_protocol).await;
        assert_eq!(
            result.err().unwrap(),
            "房间版本不兼容，请和房主更新到相同版本后再试"
        );
    }

    #[test]
    fn snapshot_updates_reject_invalid_metadata_and_nested_values() {
        let mut cursor = ChatCursor::default();
        decode_snapshot(&mut wire_snapshot(), None, &mut cursor).unwrap();
        let mut invalid_member = wire_snapshot();
        invalid_member["members"][0] = json!(["host", "Host", "picture"]);
        let mut invalid_message = wire_snapshot();
        invalid_message["messages"][0] = json!(["message", "host", "Host", "hello", 42]);
        let mut wrong_protocol = wire_snapshot();
        wrong_protocol["roomProtocol"] = json!(1);
        let mut wrong_room = wire_snapshot();
        wrong_room["roomId"] = json!("other");
        let mut missing_owner = wire_snapshot();
        missing_owner.as_object_mut().unwrap().remove("ownerId");
        let mut missing_self = wire_snapshot();
        missing_self["members"].as_array_mut().unwrap().pop();
        let mut duplicate_member = wire_snapshot();
        duplicate_member["members"][0]["id"] = json!("guest");
        let mut missing_author = wire_snapshot();
        missing_author["messages"][0]
            .as_object_mut()
            .unwrap()
            .remove("authorId");
        let mut null_cursor = wire_snapshot();
        null_cursor["messagesAfter"] = Value::Null;
        for mut event in [
            invalid_member,
            invalid_message,
            wrong_protocol,
            wrong_room,
            missing_owner,
            missing_self,
            duplicate_member,
            missing_author,
            null_cursor,
        ] {
            assert!(
                decode_snapshot(&mut event, Some("room"), &mut cursor).is_err(),
                "accepted malformed update: {event}"
            );
            assert_eq!(cursor.last_id.as_deref(), Some("message"));
            assert_eq!(cursor.len, 1);
        }
    }

    #[test]
    fn snapshot_message_time_must_be_a_nonnegative_date_integer() {
        for (time, accepted) in [
            (json!(0), true),
            (json!(8_640_000_000_000_000_u64), true),
            (json!(8_640_000_000_000_001_u64), false),
            (json!(-1), false),
            (json!(0.5), false),
            (json!("42"), false),
        ] {
            let mut event = wire_snapshot();
            event["messages"][0]["time"] = time.clone();
            assert_eq!(
                decode_snapshot(&mut event, Some("room"), &mut ChatCursor::default()).is_ok(),
                accepted,
                "unexpected result for {time}"
            );
        }
    }

    #[test]
    fn snapshot_updates_preserve_avatar_omission_clear_and_member_departure() {
        let mut cursor = ChatCursor::default();
        let mut initial = wire_snapshot();
        initial["members"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"leaving","name":"Leaving","avatar":"old picture"}));
        decode_snapshot(&mut initial, None, &mut cursor).unwrap();
        assert_eq!(initial["members"][0]["avatar"], "picture");

        let mut update = wire_snapshot();
        update["members"][0]
            .as_object_mut()
            .unwrap()
            .remove("avatar");
        update["messagesAfter"] = json!("message");
        update["messages"] = json!([]);
        decode_snapshot(&mut update, Some("room"), &mut cursor).unwrap();
        assert_eq!(
            update["members"],
            json!([{"id":"host","name":"Host"},{"id":"guest","name":"Guest","avatar":""}])
        );
        assert_eq!(update["messages"], json!([]));
        assert_eq!(update["messagesAfter"], "message");
        assert_eq!(update["retainedMessageCount"], 1);
    }
}
