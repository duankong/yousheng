use super::{Cleanup, Config, Core, Membership, Shared};
use axum::{
    extract::{
        ws::{rejection::WebSocketUpgradeRejection, Message, WebSocket, WebSocketUpgrade},
        OriginalUri, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::btree_map::Entry,
    net::SocketAddrV4,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpStream,
    sync::watch,
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message as Frame},
    MaybeTlsStream, WebSocketStream,
};
use url::Url;

#[derive(Clone)]
pub(super) struct Media {
    key: String,
    secret: String,
    base: Url,
    client: reqwest::Client,
}

impl Media {
    pub fn new(c: &Config) -> Result<Self, String> {
        let base = Url::parse(&c.livekit_http_url).map_err(|e| e.to_string())?;
        if base.scheme() != "http"
            || !base
                .host_str()
                .is_some_and(|h| h == "127.0.0.1" || h == "[::1]")
        {
            return Err("语音服务控制地址须为本机回环地址".into());
        }
        if c.livekit_key.is_empty() || c.livekit_secret.is_empty() {
            return Err("语音服务密钥为空".into());
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(4))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            key: c.livekit_key.clone(),
            secret: c.livekit_secret.clone(),
            base,
            client,
        })
    }

    pub(super) fn token(
        &self,
        id: &str,
        name: &str,
        epoch: &str,
        video: Value,
    ) -> Result<String, jsonwebtoken::errors::Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        encode(
            &Header::new(Algorithm::HS256),
            &json!({"iss":self.key, "sub":id, "name":name, "metadata":epoch,
            "nbf":now.saturating_sub(5), "exp":now + 600, "video":video}),
            &EncodingKey::from_secret(self.secret.as_bytes()),
        )
    }

    pub fn voice(&self, id: &str, name: &str, m: &Membership) -> Value {
        let grant = json!({"roomJoin":true,"room":m.room,"canPublish":true,"canPublishSources":["microphone"],
            "canSubscribe":true,"canPublishData":false,"canUpdateOwnMetadata":false});
        match self.token(id, name, &m.epoch, grant) {
            Ok(token) => json!({"type":"voice","channelId":m.room,"token":token}),
            Err(_) => json!({"type":"error","message":"语音凭证签发失败"}),
        }
    }

    fn claims(&self, token: &str) -> Result<Claims, StatusCode> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[&self.key]);
        validation.validate_nbf = true;
        validation.leeway = 5;
        let claims = decode::<Claims>(
            token,
            &DecodingKey::from_secret(self.secret.as_bytes()),
            &validation,
        )
        .map_err(|_| StatusCode::UNAUTHORIZED)?
        .claims;
        if !claims.video.room_join {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(claims)
    }

    async fn remove(&self, cleanup: &Cleanup) -> bool {
        let (method, args, grants) = match cleanup {
            Cleanup::Participant(room, id) => (
                "RemoveParticipant",
                json!({"room":room,"identity":id}),
                json!({"roomAdmin":true,"room":room}),
            ),
            Cleanup::Room(room) => (
                "DeleteRoom",
                json!({"room":room}),
                json!({"roomCreate":true,"room":room}),
            ),
            Cleanup::None => return true,
        };
        let Ok(token) = self.token("lobby-service", "", "", grants) else {
            return false;
        };
        let Ok(url) = self
            .base
            .join(&format!("/twirp/livekit.RoomService/{method}"))
        else {
            return false;
        };
        let Ok(response) = self
            .client
            .post(url)
            .bearer_auth(token)
            .json(&args)
            .send()
            .await
        else {
            return false;
        };
        if response.status().is_success() {
            return true;
        }
        // Twirp confirms that the participant (or its room) is already gone.
        response.status() == StatusCode::NOT_FOUND
            && response
                .json::<Value>()
                .await
                .ok()
                .is_some_and(|v| v["code"] == "not_found")
    }
}

pub(super) fn cleanup(state: &Shared, core: &mut Core, job: Cleanup) {
    let pending = match &job {
        Cleanup::None => return,
        Cleanup::Participant(room, id) => {
            let key = (room.clone(), id.clone());
            let Entry::Vacant(removal) = core.pending.entry(key.clone()) else {
                return;
            };
            removal.insert(watch::channel(()).0);
            Some(key)
        }
        Cleanup::Room(_) => None,
    };
    let media = state.media.clone();
    let weak = std::sync::Arc::downgrade(&state.core);
    let mut stop = state.stop.clone();
    tokio::spawn(async move {
        let mut delay = Duration::from_millis(200);
        while weak.strong_count() > 0 && !*stop.borrow() {
            if media.remove(&job).await {
                if let (Some(key), Some(core)) = (pending.as_ref(), weak.upgrade()) {
                    core.lock().await.pending.remove(key);
                }
                break;
            }
            tokio::select! { _ = sleep(delay) => (), _ = stop.changed() => break }
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    });
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    metadata: String,
    video: Grant,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Grant {
    room: String,
    room_join: bool,
}

impl Claims {
    fn current<'a>(&self, core: &'a Core) -> Result<&'a super::Member, StatusCode> {
        let m = core.members.get(&self.sub).ok_or(StatusCode::FORBIDDEN)?;
        let grant = m.membership.as_ref().ok_or(StatusCode::FORBIDDEN)?;
        if grant.room != self.video.room || grant.epoch != self.metadata {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(m)
    }
}

// Only signaling endpoints are reachable from outside the host. Keep the administrative
// Twirp service private, and require the same membership for resumed and fresh connections.
pub(super) async fn gateway(
    State(state): State<Shared>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, StatusCode> {
    if *state.stop.borrow() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let validate = match uri.path() {
        "/rtc/validate" | "/rtc/v1/validate" => true,
        "/rtc" | "/rtc/v1" => false,
        _ => return Err(StatusCode::NOT_FOUND),
    };
    let mut token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_owned);
    let mut url = state.media.base.clone();
    url.set_path(uri.path());
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        if key == "access_token" {
            if token.as_ref().is_some_and(|t| t != value.as_ref()) {
                return Err(StatusCode::BAD_REQUEST);
            }
            token = Some(value.into_owned());
        } else if key == "publish" {
            return Err(StatusCode::BAD_REQUEST);
        } else {
            url.query_pairs_mut().append_pair(&key, &value);
        }
    }
    let token = token.ok_or(StatusCode::UNAUTHORIZED)?;
    let claims = state.media.claims(&token)?;
    let core = state.core.lock().await;
    let member = claims.current(&core)?;
    if validate {
        drop(core);
        let response = state
            .media
            .client
            .get(url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        return Ok((
            response.status(),
            response
                .bytes()
                .await
                .map_err(|_| StatusCode::BAD_GATEWAY)?,
        )
            .into_response());
    }
    let ws = ws.map_err(|_| StatusCode::BAD_REQUEST)?;
    let revoked = member.membership.as_ref().unwrap().revoked.subscribe();
    let playback = member.playback;
    url.set_scheme("ws").map_err(|_| StatusCode::BAD_GATEWAY)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| StatusCode::BAD_REQUEST)?,
    );
    // Serialize private admission with membership changes so a kick cannot overtake an in-flight join.
    let (upstream, _) = timeout(Duration::from_secs(3), connect_async(request))
        .await
        .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    drop(core);
    Ok(ws
        .max_message_size(2 * 1024 * 1024)
        .on_upgrade(move |down| bridge(down, upstream, revoked, state.stop, playback)))
}

async fn bridge(
    mut down: WebSocket,
    mut up: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut revoked: watch::Receiver<()>,
    mut stop: watch::Receiver<bool>,
    playback: Option<SocketAddrV4>,
) {
    let forward = async {
        loop {
            tokio::select! {
                frame = down.recv() => {
                    let frame = match frame {
                        Some(Ok(Message::Binary(v))) => Frame::Binary(v),
                        Some(Ok(Message::Text(v))) => Frame::Text(v.to_string().into()),
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                        _ => break,
                    };
                    if up.send(frame).await.is_err() { break; }
                }
                frame = up.next() => {
                    let frame = match frame {
                        Some(Ok(Frame::Binary(v))) => {
                            match crate::ice::local_signal(&v, playback) {
                                Ok(Some(bytes)) => Message::Binary(bytes.into()),
                                Ok(None) => continue,
                                Err(_) => break,
                            }
                        }
                        Some(Ok(Frame::Text(v))) => Message::Text(v.to_string().into()),
                        Some(Ok(Frame::Ping(_) | Frame::Pong(_))) => continue,
                        _ => break,
                    };
                    if down.send(frame).await.is_err() { break; }
                }
            }
        }
    };
    if !*stop.borrow() {
        tokio::select! { biased; _ = revoked.changed() => (), _ = stop.changed() => (), _ = forward => () }
    }
}
