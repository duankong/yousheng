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
        avatar_revision: 0,
        pending: HashSet::new(),
    }
}

fn hello(secret: char) -> Op {
    Op::Hello {
        invite_key: INVITE.into(),
        nickname: "同名玩家".into(),
        avatar: String::new(),
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

fn jpeg_url(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    format!("data:image/jpeg;base64,{}", STANDARD.encode(bytes))
}

#[test]
fn avatar_and_nickname_updates_keep_voice_and_actor_identity() {
    let mut c = core(Path::new(":memory:"));
    let avatar = jpeg_url(include_bytes!("test-avatar.jpg"));
    let guest = login(&mut c, 'b');
    let admin = login(&mut c, 'a');
    let room = c.channels.create(&guest, "排位").unwrap();
    join(&mut c, &guest, &room);
    let session = c.members[&guest].session;
    let epoch = c.members[&guest].membership.as_ref().unwrap().epoch.clone();
    let revision = c.avatar_revision;
    let op = serde_json::from_value(json!({"type":"profile","id":admin,
        "nickname":"  夜猫  ","avatar":avatar}))
    .unwrap();
    assert!(matches!(c.apply(&guest, op), Ok(Cleanup::None)));
    assert!(c.active(&guest, session));
    assert_eq!(c.members[&guest].membership.as_ref().unwrap().epoch, epoch);
    assert_eq!(c.members[&guest].room(), Some(room.as_str()));
    assert_eq!(c.members[&admin].name, "同名玩家");
    assert_eq!(c.members[&guest].name, "夜猫");
    assert_eq!(c.members[&guest].avatar, avatar);
    assert!(c.channels.find(&room).unwrap().can_delete(&guest, false));

    let members = c.snapshot(&admin, Some(revision))["members"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        members.iter().find(|m| m["id"] == guest).unwrap()["avatar"],
        avatar
    );
    let revision = c.avatar_revision;
    let chat = Op::Chat {
        channel_id: room.clone(),
        text: "报点".into(),
    };
    assert!(c.apply(&guest, chat).is_ok());
    let state = c.snapshot(&guest, Some(revision));
    assert_eq!(state["messages"][0]["authorName"], "夜猫");
    assert!(state["members"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m.get("avatar").is_none()));
    assert!(c
        .apply(
            &guest,
            Op::Profile {
                nickname: "换错了".into(),
                avatar: "data:text/plain,hi".into()
            }
        )
        .is_err());
    assert_eq!(c.members[&guest].name, "夜猫");
    assert_eq!(c.avatar_revision, revision);

    assert!(c
        .apply(
            &guest,
            Op::Profile {
                nickname: "夜猫".into(),
                avatar: String::new()
            }
        )
        .is_ok());
    let state = c.snapshot(&admin, Some(revision));
    assert_eq!(
        state["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == guest)
            .unwrap()["avatar"],
        ""
    );
}

#[test]
fn hello_accepts_old_clients_and_rejects_invalid_or_oversize_pictures() {
    let mut c = core(Path::new(":memory:"));
    let mut hello =
        json!({"type":"hello","identity":"b".repeat(64),"nickname":"玩家","inviteKey":INVITE});
    let first = c
        .login(serde_json::from_value(hello.clone()).unwrap())
        .unwrap()
        .0;
    assert!(c.members[&first].avatar.is_empty());

    let jpeg = include_bytes!("test-avatar.jpg");
    hello["avatar"] = jpeg_url(jpeg).into();
    let rejoined = c
        .login(serde_json::from_value(hello.clone()).unwrap())
        .unwrap()
        .0;
    assert_eq!(first, rejoined);
    assert_eq!(c.members[&first].avatar, hello["avatar"]);
    let session = c.members[&first].session;
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
        hello["avatar"] = invalid.into();
        assert!(c
            .login(serde_json::from_value(hello.clone()).unwrap())
            .is_err());
        assert!(c.active(&first, session));
    }
}

#[test]
fn ownership_survives_restart_and_deleted_room_stays_deleted() {
    let path = std::env::temp_dir().join(format!("duipin-{}.sqlite", Uuid::new_v4()));
    let mut c = core(&path);
    let creator = login(&mut c, 'b');
    let room = c.channels.create(&creator, "游戏").unwrap();
    assert!(!c.snapshot(&creator, None)["admin"].as_bool().unwrap());
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
    assert_eq!(c.snapshot(&other, None)["channels"][0]["canDelete"], false);
    let owner = login(&mut c, 'b');
    assert_eq!(owner, creator);
    assert_eq!(c.snapshot(&owner, None)["channels"][0]["canDelete"], true);
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
    let history = c.snapshot(&a, None)["messages"].as_array().unwrap().clone();
    assert_eq!(history.len(), 100);
    assert_eq!(history[0]["text"], "1");
    assert_eq!(c.snapshot(&b, None)["messages"], json!([]));
    join(&mut c, &b, &red);
    assert_eq!(c.snapshot(&b, None)["messages"], json!(history));
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
    assert_eq!(c.snapshot(&b, None)["channelId"], Value::Null);
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

#[tokio::test]
async fn two_sockets_observe_profile_changes_without_leaving_the_channel() {
    let (_stop, stopping) = watch::channel(false);
    let app = router_with_shutdown(config(), stopping).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (mut host, _) = connect_async(&url).await.unwrap();
    let host_state = command(
        &mut host,
        json!({"type":"hello","inviteKey":INVITE,"identity":"a".repeat(64),"nickname":"主机"}),
    )
    .await;
    assert_eq!(host_state["admin"], true);
    let created = command(&mut host, json!({"type":"create","name":"排位"})).await;
    let room = created["channels"][0]["id"].as_str().unwrap();
    command(&mut host, json!({"type":"join","channelId":room})).await;
    assert_eq!(read(&mut host).await["type"], "voice");

    let (mut guest, _) = connect_async(&url).await.unwrap();
    let guest_state = command(
        &mut guest,
        json!({"type":"hello","inviteKey":INVITE,"identity":"b".repeat(64),"nickname":"旧昵称"}),
    )
    .await;
    let guest_id = guest_state["selfId"].as_str().unwrap();
    assert_eq!(
        read(&mut host).await["members"].as_array().unwrap().len(),
        2
    );
    command(&mut guest, json!({"type":"join","channelId":room})).await;
    assert_eq!(read(&mut guest).await["type"], "voice");
    read(&mut host).await;

    command(
        &mut guest,
        json!({"type":"chat","channelId":room,"text":"改名前"}),
    )
    .await;
    let old_message = read(&mut host).await["messages"][0].clone();
    assert_eq!(old_message["authorName"], "旧昵称");

    let portrait = jpeg_url(include_bytes!("test-avatar.jpg"));
    for avatar in [&portrait[..], ""] {
        let ack = command(
            &mut guest,
            json!({"type":"profile","nickname":"新昵称","avatar":avatar,"requestId":7}),
        )
        .await;
        assert_eq!(ack, json!({"type":"result","requestId":7,"error":null}));
        let (local, remote) = tokio::join!(read(&mut guest), read(&mut host));
        for state in [local, remote] {
            let members = state["members"].as_array().unwrap();
            let changed = members.iter().find(|m| m["id"] == guest_id).unwrap();
            assert_eq!(changed["name"], "新昵称");
            assert_eq!(changed["avatar"], avatar);
            assert_eq!(members.len(), 2);
            assert!(members.iter().all(|m| m["channelId"] == room));
            assert_eq!(state["channelId"], room);
            assert_eq!(state["messages"][0], old_message);
        }
        command(
            &mut guest,
            json!({"type":"chat","channelId":room,"text":"改名后"}),
        )
        .await;
        let remote = read(&mut host).await;
        let newest = remote["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(newest["authorId"], guest_id);
        assert_eq!(newest["authorName"], "新昵称");
        assert!(remote["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("avatar").is_none()));
    }
    server.abort();
}
