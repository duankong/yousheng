use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message as Frame};

const INVITE: &str = "this-test-invite-is-public";

fn hello(secret: char, name: &str) -> Value {
    json!({
        "type":"hello", "roomProtocol":2, "inviteKey":INVITE,
        "identity":secret.to_string().repeat(64), "nickname":name,
    })
}

fn jpeg_url(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    format!("data:image/jpeg;base64,{}", STANDARD.encode(bytes))
}

fn config() -> Config {
    Config {
        owner_identity: "a".repeat(64),
        owner_nickname: "主机".into(),
        invite_key: INVITE.into(),
        livekit_key: "test-only-api".into(),
        livekit_secret: "this-is-a-local-integration-test-secret".into(),
        livekit_http_url: "http://127.0.0.1:9".into(),
    }
}

type TestSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TestRoom {
    url: String,
    http: String,
    stop: watch::Sender<bool>,
    control: RoomControl,
    server: tokio::task::JoinHandle<()>,
}

impl TestRoom {
    async fn start(config: Config) -> Self {
        let (stop, rx) = watch::channel(false);
        let (app, control) = router_with_shutdown(config, rx).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        TestRoom {
            url: format!("ws://{addr}/ws"),
            http: format!("http://{addr}"),
            stop,
            control,
            server,
        }
    }

    async fn connect(&self) -> TestSocket {
        connect_async(&self.url).await.unwrap().0
    }

    async fn login(&self, secret: char, name: &str) -> (TestSocket, Value) {
        let mut ws = self.connect().await;
        send_op(&mut ws, hello(secret, name)).await;
        let snapshot = read(&mut ws).await;
        assert_eq!(snapshot["type"], "snapshot");
        (ws, snapshot)
    }
}

impl Drop for TestRoom {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        self.server.abort();
    }
}

async fn read(ws: &mut TestSocket) -> Value {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Frame::Text(v) = ws.next().await.unwrap().unwrap() {
                return serde_json::from_str(&v).unwrap();
            }
        }
    })
    .await
    .expect("room did not send an event")
}

async fn event(ws: &mut TestSocket, accept: impl Fn(&Value) -> bool) -> Value {
    timeout(Duration::from_secs(3), async {
        loop {
            let result = read(ws).await;
            if accept(&result) {
                return result;
            }
        }
    })
    .await
    .expect("room did not send the expected event")
}

async fn send_op(ws: &mut TestSocket, op: Value) {
    ws.send(Frame::Text(op.to_string().into())).await.unwrap();
}

async fn token(ws: &mut TestSocket) -> String {
    event(ws, |v| v["type"] == "voice" && v["token"].is_string()).await["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn guest_enters_the_hosts_only_room_without_a_second_join() {
    let room = TestRoom::start(config()).await;
    let (mut host, host_state) = room.login('a', "主机").await;
    assert_eq!(host_state["roomName"], "主机 的房间");
    assert_eq!(host_state["roomProtocol"], 2);
    assert_eq!(host_state["members"].as_array().unwrap().len(), 1);
    assert!(host_state.get("channels").is_none());
    assert!(timeout(Duration::from_millis(120), host.next())
        .await
        .is_err());
    let id = &host_state["roomId"];
    let (mut guest, guest_state) = room.login('b', "好友").await;
    assert_eq!(guest_state["roomId"], *id);
    assert_eq!(guest_state["ownerId"], host_state["selfId"]);
    assert_eq!(guest_state["ownerNickname"], "主机");
    assert_eq!(guest_state["members"].as_array().unwrap().len(), 2);
    token(&mut guest).await;
    token(&mut host).await;
    send_op(&mut guest, json!({"type":"chat","roomId":id,"text":"你好"})).await;
    let received = read(&mut host).await;
    assert_eq!(received["messages"][0]["text"], "你好");
    assert_eq!(received["messages"][0]["authorId"], guest_state["selfId"]);
}

#[tokio::test]
async fn unsupported_versions_and_invalid_avatars_leave_existing_members_connected() {
    let room = TestRoom::start(config()).await;
    let (mut host, initial) = room.login('a', "主机").await;
    let mut legacy = room.connect().await;
    let mut old_hello = hello('b', "旧版");
    old_hello.as_object_mut().unwrap().remove("roomProtocol");
    send_op(&mut legacy, old_hello).await;
    let rejected = read(&mut legacy).await;
    assert_eq!(rejected["type"], "error");
    assert!(rejected["message"].as_str().unwrap().contains("更新"));
    let jpeg = include_bytes!("test-avatar.jpg");
    let mut wide = jpeg.to_vec();
    let frame = wide.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    wide[frame + 7..frame + 9].copy_from_slice(&97u16.to_be_bytes());
    for invalid in [
        jpeg_url(&wide),
        jpeg_url(&jpeg[..20]),
        jpeg_url(&vec![0; 8193]),
        "https://example.org/avatar.jpg".into(),
        "data:image/jpeg;base64,invalid?".into(),
    ] {
        let mut replacement = room.connect().await;
        let mut op = hello('a', "替换了");
        op["avatar"] = invalid.into();
        send_op(&mut replacement, op).await;
        assert_eq!(read(&mut replacement).await["type"], "error");
    }
    send_op(
        &mut host,
        json!({"type":"chat","roomId":initial["roomId"],"text":"仍在线"}),
    )
    .await;
    let after = read(&mut host).await;
    assert_eq!(after["ownerNickname"], "主机");
    assert_eq!(after["members"].as_array().unwrap().len(), 1);
    assert_eq!(after["messages"][0]["text"], "仍在线");
}

#[tokio::test]
async fn chat_is_bounded_and_updates_only_send_new_messages() {
    let room = TestRoom::start(config()).await;
    let (mut host, initial) = room.login('a', "主机").await;
    let id = &initial["roomId"];
    send_op(
        &mut host,
        json!({"type":"chat","roomId":"wrong-room","text":"无效","requestId":1}),
    )
    .await;
    assert!(read(&mut host).await["error"].is_string());
    let mut previous = Value::Null;
    for n in 0..101 {
        send_op(
            &mut host,
            json!({"type":"chat","roomId":id,"text":n.to_string()}),
        )
        .await;
        let delta = read(&mut host).await;
        assert_eq!(delta["messagesAfter"], previous);
        assert_eq!(delta["messages"].as_array().unwrap().len(), 1);
        previous = delta["messages"][0]["id"].clone();
    }
    let (_guest, first) = room.login('b', "好友").await;
    assert!(first.get("messagesAfter").is_none());
    let history = first["messages"].as_array().unwrap();
    assert_eq!(history.len(), 100);
    assert_eq!(history[0]["text"], "1");
    assert_eq!(history[99]["text"], "100");
    let reopened = TestRoom::start(config()).await;
    let (_host, fresh) = reopened.login('a', "主机").await;
    assert_ne!(fresh["roomId"], *id);
    assert_eq!(fresh["messages"], json!([]));
}

#[tokio::test]
async fn room_refuses_an_update_shutdown_with_guests_and_rejects_an_inflight_join_on_stop() {
    let room = TestRoom::start(config()).await;
    let (mut host, _) = room.login('a', "主机").await;
    let (mut guest, _) = room.login('b', "好友").await;
    assert!(room.control.stop_if_empty(&room.stop).await.is_err());
    assert!(!*room.stop.borrow());
    guest.close(None).await.unwrap();
    event(&mut host, |v| {
        v["type"] == "snapshot" && v["members"].as_array().is_some_and(|m| m.len() == 1)
    })
    .await;
    let mut incoming = room.connect().await;
    room.control.stop_if_empty(&room.stop).await.unwrap();
    let _ = incoming
        .send(Frame::Text(hello('c', "好友 2").to_string().into()))
        .await;
    if let Some(Ok(Frame::Text(text))) = timeout(Duration::from_secs(2), incoming.next())
        .await
        .unwrap()
    {
        let next: Value = serde_json::from_str(&text).unwrap();
        assert_ne!(next["type"], "snapshot");
    }
    let closed = event(&mut host, |v| v["type"] == "offline").await;
    assert_eq!(closed["message"], "房间已关闭");
}

#[tokio::test]
async fn websocket_rejoin_keeps_chat_live_while_stale_voice_cleanup_finishes() {
    use axum::{http::HeaderMap, routing::post, Json};
    let sf_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = config();
    cfg.livekit_http_url = format!("http://{}", sf_listener.local_addr().unwrap());
    let (kick_tx, mut kicks) = tokio::sync::mpsc::channel(2);
    let sf = Router::new()
        .route(
            "/rtc/validate",
            get(|headers: HeaderMap| async move {
                assert!(headers["authorization"]
                    .to_str()
                    .unwrap()
                    .starts_with("Bearer "));
                "success"
            }),
        )
        .route(
            "/rtc/v1",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut ws| async move {
                    while let Some(Ok(message)) = ws.recv().await {
                        if ws.send(message).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        )
        .route(
            "/twirp/livekit.RoomService/RemoveParticipant",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let tx = kick_tx.clone();
                async move {
                    assert!(headers["authorization"]
                        .to_str()
                        .unwrap()
                        .starts_with("Bearer "));
                    let (ack, wait) = tokio::sync::oneshot::channel();
                    tx.send((body, ack)).await.unwrap();
                    let _ = wait.await;
                    Json(json!({}))
                }
            }),
        );
    let sf = tokio::spawn(async move { axum::serve(sf_listener, sf).await.unwrap() });
    let room = TestRoom::start(cfg).await;
    let (mut host, host_snapshot) = room.login('a', "主机").await;
    let (mut guest, guest_snapshot) = room.login('b', "好友").await;
    let room_id = &host_snapshot["roomId"];
    let old_host = token(&mut host).await;
    let old_guest = token(&mut guest).await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let validate = |ticket: String| {
        http.get(format!("{}/rtc/validate", room.http))
            .bearer_auth(ticket)
            .send()
    };
    assert_eq!(validate(old_guest.clone()).await.unwrap().status(), 200);
    let (mut signal, _) = connect_async(format!(
        "{}/rtc/v1?access_token={old_guest}",
        room.http.replacen("http", "ws", 1)
    ))
    .await
    .unwrap();
    let frame = prost::Message::encode_to_vec(&livekit_protocol::SignalResponse {
        message: Some(livekit_protocol::signal_response::Message::Pong(42)),
    });
    signal
        .send(Frame::Binary(frame.clone().into()))
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), signal.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data(),
        frame
    );

    guest.close(None).await.unwrap();
    let idle = event(&mut host, |v| v["type"] == "voice").await;
    assert_eq!(idle, json!({"type":"voice","roomId":room_id,"token":null}));
    assert!(!matches!(
        timeout(Duration::from_secs(2), signal.next()).await,
        Err(_) | Ok(Some(Ok(Frame::Binary(_))))
    ));
    assert_eq!(validate(old_guest.clone()).await.unwrap().status(), 403);
    assert_eq!(validate(old_host.clone()).await.unwrap().status(), 403);
    let (mut guest, rejoined) = room.login('b', "好友").await;
    assert_eq!(rejoined["selfId"], guest_snapshot["selfId"]);
    assert_eq!(rejoined["members"].as_array().unwrap().len(), 2);
    send_op(
        &mut guest,
        json!({"type":"chat","roomId":room_id,"text":"语音恢复前也能聊天","requestId":42}),
    )
    .await;
    let ack = event(&mut guest, |v| v["type"] == "result").await;
    assert_eq!(ack, json!({"type":"result","requestId":42,"error":null}));
    let chat = event(&mut host, |v| {
        v["messages"][0]["text"] == "语音恢复前也能聊天"
    })
    .await;
    assert_eq!(chat["roomId"], *room_id);
    let mut removed = BTreeSet::new();
    for _ in 0..2 {
        let (removal, ack) = timeout(Duration::from_secs(2), kicks.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(removal["room"], *room_id);
        removed.insert(removal["identity"].as_str().unwrap().to_owned());
        ack.send(()).unwrap();
    }
    assert_eq!(
        removed,
        BTreeSet::from([
            host_snapshot["selfId"].as_str().unwrap().to_owned(),
            guest_snapshot["selfId"].as_str().unwrap().to_owned()
        ])
    );
    let fresh_host = token(&mut host).await;
    let fresh_guest = token(&mut guest).await;
    for (old, fresh) in [(old_host, fresh_host), (old_guest, fresh_guest)] {
        assert_eq!(validate(old).await.unwrap().status(), 403);
        assert_eq!(validate(fresh).await.unwrap().status(), 200);
    }
    send_op(&mut guest, json!({"type":"retryVoice", "requestId":43})).await;
    assert_eq!(
        event(&mut guest, |v| v["type"] == "result").await,
        json!({"type":"result","requestId":43,"error":null})
    );
    assert_eq!(
        validate(token(&mut guest).await).await.unwrap().status(),
        200
    );
    room.stop.send_replace(true);
    assert_eq!(
        event(&mut guest, |v| v["type"] == "offline").await["message"],
        "房间已关闭"
    );
    sf.abort();
}

#[tokio::test]
async fn two_sockets_observe_profile_changes_without_interrupting_voice_or_chat() {
    let room = TestRoom::start(config()).await;
    let (mut host, _) = room.login('a', "主机").await;
    let (mut guest, guest_state) = room.login('b', "旧昵称").await;
    let id = &guest_state["roomId"];
    let guest_id = &guest_state["selfId"];
    token(&mut host).await;
    token(&mut guest).await;
    send_op(
        &mut guest,
        json!({"type":"chat","roomId":id,"text":"改名前"}),
    )
    .await;
    let old_chat = read(&mut host).await["messages"][0].clone();
    assert_eq!(old_chat["authorName"], "旧昵称");
    let portrait = jpeg_url(include_bytes!("test-avatar.jpg"));
    for avatar in [&portrait[..], ""] {
        send_op(
            &mut guest,
            json!({"type":"profile","nickname":"  新昵称  ","avatar":avatar,"requestId":7}),
        )
        .await;
        let reply = event(&mut guest, |v| v["type"] == "result").await;
        assert_eq!(reply, json!({"type":"result","requestId":7,"error":null}));
        let remote = read(&mut host).await;
        let members = remote["members"].as_array().unwrap();
        let changed = members.iter().find(|m| m["id"] == *guest_id).unwrap();
        assert_eq!(changed["name"], "新昵称");
        assert_eq!(changed["avatar"], avatar);
        assert_eq!(members.len(), 2);
        assert_eq!(remote["roomId"], *id);
        assert!(remote["messagesAfter"].is_string());
        assert_eq!(remote["messages"], json!([]));
        send_op(
            &mut guest,
            json!({"type":"chat","roomId":id,"text":"改名后"}),
        )
        .await;
        let remote = read(&mut host).await;
        assert_eq!(remote["messages"][0]["authorId"], *guest_id);
        assert_eq!(remote["messages"][0]["authorName"], "新昵称");
        assert!(remote["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("avatar").is_none()));
    }
    send_op(
        &mut host,
        json!({"type":"profile","nickname":"新房主名","avatar":""}),
    )
    .await;
    let updated = event(&mut guest, |v| v["roomName"] == "新房主名 的房间").await;
    assert_eq!(updated["ownerNickname"], "新房主名");
    assert_eq!(updated["ownerId"], guest_state["ownerId"]);
}
