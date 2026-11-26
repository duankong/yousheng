use crate::voice::VoiceEngine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};
use tauri::{AppHandle, Emitter};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{timeout, timeout_at, Instant},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Reply = oneshot::Sender<Result<(), String>>;

const QUEUE_TIMEOUT: &str = "操作排队超时，未发送";
const DISCONNECTED: &str = "大厅连接已断开";

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
                mpsc::error::TrySendError::Full(_) => "大厅操作队列已满，未发送".to_owned(),
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
                        Err("主机未确认操作，请查看当前频道状态".into())
                    } else {
                        Err(QUEUE_TIMEOUT.into())
                    }
                }
            }
        }
    }
}

// The WebSocket orders snapshots. Expand negotiated deltas before a snapshot reaches the UI.
fn complete_chat(snapshot: &mut Value, previous: &mut Vec<Value>) -> bool {
    let Some(fields) = snapshot.as_object_mut() else {
        return false;
    };
    let after = fields.remove("messagesAfter");
    let Some(messages) = fields.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    if let Some(after) = after {
        if after.as_str().is_none() || previous.last().and_then(|m| m.get("id")) != Some(&after) {
            return false;
        }
        previous.append(messages);
        previous.drain(..previous.len().saturating_sub(crate::chat::HISTORY_LIMIT));
        messages.clone_from(previous);
    } else {
        previous.clone_from(messages);
    }
    true
}

pub struct Client {
    pub(crate) commands: ClientCommands,
    tasks: Vec<JoinHandle<()>>,
}

impl Client {
    pub async fn open(
        app: AppHandle,
        voice: Arc<VoiceEngine>,
        port: u16,
        mut hello: Value,
    ) -> Result<Self, String> {
        let base = format!("ws://127.0.0.1:{port}");
        let (mut ws, _) = timeout(Duration::from_secs(10), connect_async(format!("{base}/ws")))
            .await
            .map_err(|_| "大厅连接超时".to_owned())?
            .map_err(|e| format!("无法连接大厅：{e}"))?;
        hello["chatDeltas"] = true.into();
        ws.send(Message::Text(hello.to_string().into()))
            .await
            .map_err(|e| e.to_string())?;
        let first = timeout(Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| "大厅没有回应".to_owned())?
            .ok_or("大厅已关闭")?
            .map_err(|e| e.to_string())?;
        let mut initial: Value = serde_json::from_str(first.to_text().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        if initial["type"] != "snapshot" {
            return Err(initial["message"].as_str().unwrap_or("加入大厅失败").into());
        }
        let mut messages = Vec::new();
        if !complete_chat(&mut initial, &mut messages) {
            return Err("大厅聊天记录同步失败".into());
        }
        let _ = app.emit("lobby-event", &initial);
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
                        ()=voice.connect(&base,&token)=>{},
                    }
                }
                changed = wanted.changed().await;
            }
            voice.disconnect().await;
        });
        let socket = tokio::spawn(async move {
            let mut joined = initial["channelId"].clone();
            let mut sequence = 0u64;
            let mut waiting: HashMap<u64, Reply> = HashMap::new();
            loop {
                tokio::select! {
                    next=outgoing.recv()=>match next {
                        Some(command)=>{
                            let Some((mut data,reply))=command.begin() else {continue};
                            let Some(fields)=data.as_object_mut() else {let _=reply.send(Err("无效的大厅操作".into()));continue};
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
                                    if event["channelId"]!=joined {
                                        joined=event["channelId"].clone();
                                        messages.clear();
                                        target.send_replace(None);
                                    }
                                    if !complete_chat(&mut event, &mut messages) { break; }
                                    let _=app.emit("lobby-event",event);
                                }
                                Some("voice")=>if !joined.is_null() && event["channelId"]==joined {
                                    target.send_replace(event["token"].as_str().map(str::to_owned));
                                },
                                Some("error")=>{let _=app.emit("lobby-event",event);}
                                _=>{}
                            }
                        }
                        Some(Ok(Message::Ping(p)))=>if ws.send(Message::Pong(p)).await.is_err(){break},
                        Some(Ok(Message::Pong(_)))=>{},
                        _=>break
                    }
                }
            }
            drop(target);
            let _ = app.emit(
                "lobby-event",
                json!({"type":"offline","message":"大厅连接已断开，可通过原邀请重新加入"}),
            );
        });
        Ok(Self {
            commands: ClientCommands(commands),
            tasks: vec![socket, audio],
        })
    }
    pub async fn close(self) {
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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
                tasks: vec![],
            },
            CommandPeer(peer),
        )
    }

    #[tokio::test]
    async fn full_cancelled_and_expired_commands_never_reach_the_peer() {
        let (sender, mut queue) = mpsc::channel(1);
        let commands = ClientCommands(sender);
        let expired = commands.send_before(json!("expired"), Instant::now());
        assert_eq!(
            commands.send(json!("overflow")).await.unwrap_err(),
            "大厅操作队列已满，未发送"
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
        assert_eq!(
            result.await.unwrap_err(),
            "主机未确认操作，请查看当前频道状态"
        );
    }

    #[test]
    fn chat_deltas_preserve_order_roll_over_and_accept_full_resync() {
        let mut previous = Vec::new();
        let rows: Vec<_> = (0..100).map(|n| json!({"id":n.to_string()})).collect();
        let mut full = json!({"messages":rows});
        assert!(complete_chat(&mut full, &mut previous));
        let mut delta = json!({"messagesAfter":"99","messages":[{"id":"100"},{"id":"101"}]});
        assert!(complete_chat(&mut delta, &mut previous));
        let messages = delta["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 100);
        assert_eq!(messages.first().unwrap()["id"], "2");
        assert_eq!(messages.last().unwrap()["id"], "101");
        let mut unchanged = json!({"messagesAfter":"101","messages":[]});
        assert!(complete_chat(&mut unchanged, &mut previous));
        assert_eq!(unchanged, delta);
        let mut stale = json!({"messagesAfter":"99","messages":[]});
        assert!(!complete_chat(&mut stale, &mut previous));
        let mut resync = json!({"messages":[{"id":"room-b"}]});
        assert!(complete_chat(&mut resync, &mut previous));
        assert_eq!(previous, [json!({"id":"room-b"})]);
    }
}
