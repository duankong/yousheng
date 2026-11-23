use super::*;
use futures_util::{SinkExt, StreamExt};
use std::path::Path;
use tokio_tungstenite::{connect_async, tungstenite::Message as Frame};

const INVITE: &str = "this-test-invite-is-public";

fn core(path: &Path) -> Core {
    Core {
        owner: player_id(&"a".repeat(64)).unwrap(),
        invite: Sha256::digest(INVITE.as_bytes()).into(),
        channels: Channels::open(path).unwrap(),
        chat: Chat::default(),
        members: BTreeMap::new(),
        pending: HashSet::new(),
    }
}

fn hello(secret: char) -> Op {
    Op::Hello {
        invite_key: INVITE.into(),
        nickname: "同名玩家".into(),
        identity: secret.to_string().repeat(64),
        playback_address: None,
    }
}

fn login(c: &mut Core, secret: char) -> String {
    c.login(hello(secret)).unwrap().0
}

fn join(c: &mut Core, id: &str, room: &str) {
    c.apply(
        id,
        Op::Join {
            channel_id: room.into(),
        },
    )
    .ok()
    .unwrap();
}

#[test]
fn ownership_survives_restart_and_deleted_room_stays_deleted() {
    let path = std::env::temp_dir().join(format!("duipin-{}.sqlite", Uuid::new_v4()));
    let mut c = core(&path);
    let creator = login(&mut c, 'b');
    let room = c.channels.create(&creator, "游戏").unwrap();
    assert!(!c.snapshot(&creator)["admin"].as_bool().unwrap());
    drop(c);
    let mut c = core(&path);
    let other = login(&mut c, 'c');
    assert!(c
        .apply(
            &other,
            Op::Delete {
                channel_id: room.clone()
            }
        )
        .is_err());
    assert_eq!(c.snapshot(&other)["channels"][0]["canDelete"], false);
    let owner = login(&mut c, 'b');
    assert_eq!(owner, creator);
    assert_eq!(c.snapshot(&owner)["channels"][0]["canDelete"], true);
    assert!(c
        .apply(
            &owner,
            Op::Delete {
                channel_id: room.clone()
            }
        )
        .is_ok());
    drop(c);
    let mut c = core(&path);
    assert!(c.channels.find(&room).is_err());
    let replacement = c.channels.create(&creator, "游戏").unwrap();
    assert_ne!(replacement, room);
    let admin = login(&mut c, 'a');
    assert!(c
        .apply(
            &admin,
            Op::Delete {
                channel_id: replacement
            }
        )
        .is_ok());
    drop(c);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn chat_is_bounded_scoped_and_cleared_when_an_occupied_room_is_deleted() {
    let mut c = core(Path::new(":memory:"));
    let a = login(&mut c, 'a');
    let b = login(&mut c, 'b');
    let red = c.channels.create(&a, "红").unwrap();
    let blue = c.channels.create(&b, "蓝").unwrap();
    join(&mut c, &a, &red);
    join(&mut c, &b, &blue);
    assert!(c
        .apply(
            &b,
            Op::Chat {
                channel_id: red.clone(),
                text: "forged".into()
            }
        )
        .is_err());
    for n in 0..101 {
        assert!(c
            .apply(
                &a,
                Op::Chat {
                    channel_id: red.clone(),
                    text: n.to_string()
                }
            )
            .is_ok());
    }
    let history = c.snapshot(&a)["messages"].as_array().unwrap().clone();
    assert_eq!(history.len(), 100);
    assert_eq!(history[0]["text"], "1");
    assert_eq!(c.snapshot(&b)["messages"], json!([]));
    join(&mut c, &b, &red);
    assert_eq!(c.snapshot(&b)["messages"], json!(history));
    let revoked = c.members[&b]
        .membership
        .as_ref()
        .unwrap()
        .revoked
        .subscribe();
    assert!(c
        .apply(
            &a,
            Op::Delete {
                channel_id: red.clone()
            }
        )
        .is_ok());
    assert!(revoked.has_changed().is_err());
    assert_eq!(c.snapshot(&b)["channelId"], Value::Null);
    assert!(c.chat.history(Some(&red)).is_empty());
    assert!(c
        .apply(
            &b,
            Op::Chat {
                channel_id: red,
                text: "late".into()
            }
        )
        .is_err());
}

fn config() -> Config {
    Config {
        database: ":memory:".into(),
        owner_identity: "a".repeat(64),
        invite_key: INVITE.into(),
        livekit_key: "test-only-api".into(),
        livekit_secret: "this-is-a-local-integration-test-secret".into(),
        livekit_http_url: "http://127.0.0.1:9".into(),
    }
}

type TestSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn read(ws: &mut TestSocket) -> Value {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Frame::Text(v) = ws.next().await.unwrap().unwrap() {
                return serde_json::from_str(&v).unwrap();
            }
        }
    })
    .await
    .unwrap()
}

async fn command(ws: &mut TestSocket, op: Value) -> Value {
    ws.send(Frame::Text(op.to_string().into())).await.unwrap();
    read(ws).await
}

#[tokio::test]
async fn websocket_login_and_gateway_reject_stale_tokens_after_leave() {
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
                    tx.send(body).await.unwrap();
                    Json(json!({}))
                }
            }),
        );
    let sf = tokio::spawn(async move { axum::serve(sf_listener, sf).await.unwrap() });
    let (stop, rx) = watch::channel(false);
    let app = router_with_shutdown(cfg, rx).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (mut ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    ws.send(Frame::Text(
        json!({"type":"hello","inviteKey":INVITE,"nickname":"玩家","identity":"b".repeat(64)})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let snapshot = read(&mut ws).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["admin"], false);
    let denied = command(
        &mut ws,
        json!({"type":"join","channelId":"unknown","requestId":41}),
    )
    .await;
    assert_eq!(denied["requestId"], 41);
    assert!(denied["error"].is_string());
    ws.send(Frame::Text(
        json!({"type":"create","name":"频道","requestId":42})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    assert_eq!(
        read(&mut ws).await,
        json!({"type":"result","requestId":42,"error":null})
    );
    let room = read(&mut ws).await["channels"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ws.send(Frame::Text(
        json!({"type":"join","channelId":room}).to_string().into(),
    ))
    .await
    .unwrap();
    assert_eq!(read(&mut ws).await["channelId"], room);
    let voice = read(&mut ws).await;
    assert_eq!(voice["type"], "voice");
    let ticket = voice["token"].as_str().unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let endpoint = format!("http://{addr}/rtc/validate");
    assert_eq!(
        http.get(&endpoint)
            .bearer_auth(ticket)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "success"
    );
    let (mut signal, _) = connect_async(format!("ws://{addr}/rtc/v1?access_token={ticket}"))
        .await
        .unwrap();
    signal
        .send(Frame::Binary(b"media signaling".as_slice().to_vec().into()))
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), signal.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data(),
        &b"media signaling"[..]
    );
    ws.send(Frame::Text(json!({"type":"leave"}).to_string().into()))
        .await
        .unwrap();
    assert_eq!(read(&mut ws).await["channelId"], Value::Null);
    let removal = timeout(Duration::from_secs(2), kicks.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(removal, json!({"room":room,"identity":snapshot["selfId"]}));
    assert!(!matches!(
        timeout(Duration::from_secs(2), signal.next()).await,
        Err(_) | Ok(Some(Ok(Frame::Binary(_))))
    ));
    assert_eq!(
        http.get(&endpoint)
            .bearer_auth(ticket)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    // The same player can return once the private remove call completes. A ticket from
    // that player's earlier stay must still fail even though the room and identity match.
    timeout(Duration::from_secs(3), async {
        loop {
            ws.send(Frame::Text(
                json!({"type":"join","channelId":room}).to_string().into(),
            ))
            .await
            .unwrap();
            let event = read(&mut ws).await;
            if event["type"] == "snapshot" {
                assert_eq!(event["channelId"], room);
                break;
            }
            assert_eq!(event["type"], "error");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let fresh = read(&mut ws).await;
    assert_eq!(
        http.get(&endpoint)
            .bearer_auth(fresh["token"].as_str().unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        http.get(&endpoint)
            .bearer_auth(ticket)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    stop.send(true).unwrap();
    assert!(!matches!(
        timeout(Duration::from_secs(2), ws.next()).await,
        Err(_) | Ok(Some(Ok(Frame::Text(_))))
    ));
    server.abort();
    sf.abort();
}

#[tokio::test]
#[ignore = "requires DUIPIN_TEST_SFU pointing to the packaged LiveKit Server executable"]
async fn packaged_livekit_admits_member_and_destroys_deleted_room() {
    use std::process::Stdio;
    let executable = std::env::var_os("DUIPIN_TEST_SFU").expect("set DUIPIN_TEST_SFU");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut cfg = config();
    cfg.livekit_http_url = format!("http://127.0.0.1:{port}");
    let yaml = format!("port: {port}\nbind_addresses: [127.0.0.1]\nrtc:\n  udp_port: {udp}\n  tcp_port: 0\n  node_ip: 127.0.0.1\n  enable_loopback_candidate: true\nkeys:\n  {}: {}\n",cfg.livekit_key,cfg.livekit_secret);
    let mut process = tokio::process::Command::new(executable);
    #[cfg(windows)]
    process.creation_flags(0x08000000);
    let mut sfu = process
        .args(["--config-body", &yaml])
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(15), async {
        while tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            assert!(
                sfu.try_wait().unwrap().is_none(),
                "packaged SFU exited before startup"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let signer = Media::new(&cfg).unwrap();
    let base = cfg.livekit_http_url.clone();
    let (_stop, stopping) = watch::channel(false);
    let app = router_with_shutdown(cfg, stopping).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (mut ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let me = command(
        &mut ws,
        json!({"type":"hello","identity":"b".repeat(64),"inviteKey":INVITE,"nickname":"测试"}),
    )
    .await;
    let room = command(&mut ws, json!({"type":"create","name":"测试频道"})).await["channels"][0]
        ["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        command(&mut ws, json!({"type":"join","channelId":room})).await["channelId"],
        room
    );
    let ticket = read(&mut ws).await["token"].as_str().unwrap().to_owned();
    let endpoint = format!("http://{addr}/rtc/validate");
    assert_eq!(
        http.get(&endpoint)
            .bearer_auth(&ticket)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let (mut media_ws, _) =
        connect_async(format!("ws://{addr}/rtc?protocol=15&access_token={ticket}"))
            .await
            .unwrap();
    let first = timeout(Duration::from_secs(4), media_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(first,Frame::Binary(bytes) if !bytes.is_empty()));
    let admin = signer
        .token("test-service", "", "", json!({"roomList":true}))
        .unwrap();
    let list = || async {
        http.post(format!("{base}/twirp/livekit.RoomService/ListRooms"))
            .bearer_auth(&admin)
            .json(&json!({"names":[room]}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    assert_eq!(list().await["rooms"][0]["name"], room);
    let snapshot = command(&mut ws, json!({"type":"delete","channelId":room})).await;
    assert_eq!(snapshot["channelId"], Value::Null);
    assert_eq!(snapshot["selfId"], me["selfId"]);
    assert_eq!(
        http.get(&endpoint)
            .bearer_auth(ticket)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    timeout(Duration::from_secs(5), async {
        while list().await["rooms"]
            .as_array()
            .is_some_and(|rooms| !rooms.is_empty())
        {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    server.abort();
    sfu.kill().await.unwrap();
}
