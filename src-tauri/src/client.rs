use crate::voice::VoiceEngine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tauri::{AppHandle, Emitter};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Reply = oneshot::Sender<Result<(), String>>;
pub struct Client {
    commands: mpsc::Sender<(Value, Reply)>,
    tasks: Vec<JoinHandle<()>>,
}

impl Client {
    pub async fn open(
        app: AppHandle,
        voice: Arc<VoiceEngine>,
        port: u16,
        hello: Value,
    ) -> Result<Self, String> {
        let base = format!("ws://127.0.0.1:{port}");
        let (mut ws, _) = timeout(Duration::from_secs(10), connect_async(format!("{base}/ws")))
            .await
            .map_err(|_| "大厅连接超时".to_owned())?
            .map_err(|e| format!("无法连接大厅：{e}"))?;
        ws.send(Message::Text(hello.to_string().into()))
            .await
            .map_err(|e| e.to_string())?;
        let first = timeout(Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| "大厅没有回应".to_owned())?
            .ok_or("大厅已关闭")?
            .map_err(|e| e.to_string())?;
        let initial: Value = serde_json::from_str(first.to_text().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        if initial["type"] != "snapshot" {
            return Err(initial["message"].as_str().unwrap_or("加入大厅失败").into());
        }
        let _ = app.emit("lobby-event", &initial);
        let (commands, mut outgoing) = mpsc::channel::<(Value, Reply)>(64);
        let (target, mut wanted) = watch::channel::<Option<String>>(None);
        let voice_app = app.clone();
        let audio = tokio::spawn(async move {
            let mut changed = wanted.changed().await;
            while changed.is_ok() {
                let token = wanted.borrow_and_update().clone();
                voice.disconnect().await;
                if let Some(token) = token {
                    let result = tokio::select! {
                        biased;
                        update=wanted.changed()=>{changed=update; continue;},
                        result=timeout(Duration::from_secs(18),voice.connect(&base,&token))=>result,
                    };
                    let failure = match result {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(e),
                        Err(_) => Some("语音连接超时".into()),
                    };
                    if let Some(message) = failure {
                        let _ = voice_app
                            .emit("voice-status", json!({"state":"error","message":message}));
                    }
                }
                changed = wanted.changed().await;
            }
            voice.disconnect().await;
        });
        let socket = tokio::spawn(async move {
            let mut joined = Value::Null;
            let mut sequence = 0u64;
            let mut waiting: HashMap<u64, Reply> = HashMap::new();
            loop {
                tokio::select! {
                    next=outgoing.recv()=>match next {
                        Some((mut data,reply))=>{
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
                            let Ok(event)=serde_json::from_str::<Value>(&data) else {break};
                            match event["type"].as_str() {
                                Some("result")=>if let Some(reply)=event["requestId"].as_u64().and_then(|id|waiting.remove(&id)){
                                    let result=event["error"].as_str().map_or(Ok(()),|e|Err(e.to_owned()));
                                    let _=reply.send(result);
                                },
                                Some("snapshot")=>{
                                    if event["channelId"]!=joined {
                                        joined=event["channelId"].clone();
                                        target.send_replace(None);
                                    }
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
            commands,
            tasks: vec![socket, audio],
        })
    }
    pub async fn send(&self, action: Value) -> Result<(), String> {
        let (reply, confirmation) = oneshot::channel();
        self.commands
            .send((action, reply))
            .await
            .map_err(|_| "大厅连接已断开".to_owned())?;
        timeout(Duration::from_secs(8), confirmation)
            .await
            .map_err(|_| "主机未确认操作，请查看当前频道状态".to_owned())?
            .map_err(|_| "大厅连接已断开".to_owned())?
    }
    pub async fn close(self) {
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
    }
}
