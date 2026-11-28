use crate::{
    client::{emit_room, AcceptedRoom, Client, ClientCommands},
    diagnostics::{Diagnostics, Role, RoomDiagnostics, RoomPhase},
    host::{Host, HostIdentity},
    network::{GuestEndpoint, Ticket},
    player_profile,
    preferences::{Preferences, Profile, RecentRoom},
    voice::{AudioSettings, PeerGain, VoiceEngine},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{path::BaseDirectory, AppHandle, Emitter, Manager, State};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

mod rooms;
mod updates;

use rooms::RoomState;

struct Session {
    id: String,
    client: Client,
    host: Option<Host>,
    guest: Option<GuestEndpoint>,
    diagnostics: RoomDiagnostics,
}

impl Session {
    async fn close(self) {
        self.client.close().await;
        if let Some(host) = self.host {
            host.close().await;
        }
        if let Some(guest) = self.guest {
            guest.close().await;
        }
        self.diagnostics.mark(RoomPhase::Closed);
    }
}

struct Desktop {
    profile: AsyncMutex<Profile>,
    voice: Arc<VoiceEngine>,
    diagnostics: Diagnostics,
    rooms: RoomState,
    available_update: AsyncMutex<Option<tauri_plugin_updater::Update>>,
    invite: Mutex<Option<String>>,
}
impl Desktop {
    fn activate(
        &self,
        app: AppHandle,
        accepted: AcceptedRoom,
        session_id: &str,
        diagnostics: &RoomDiagnostics,
    ) -> Client {
        let notify = app.clone();
        let id = session_id.to_owned();
        accepted.activate(
            app,
            self.voice.clone(),
            session_id.to_owned(),
            diagnostics.handle(),
            move |message| {
                // The socket task has to finish before teardown can join it.
                tokio::spawn(async move {
                    let state = notify.state::<Desktop>();
                    let mut slot = state.rooms.current.write().await;
                    if let Some(session) = slot.take_if(|session| session.id == id) {
                        session.diagnostics.mark(RoomPhase::Disconnected);
                        session.close().await;
                        emit_room(&notify, &id, json!({"type":"offline","message":message}));
                    }
                });
            },
        )
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionInfo {
    id: String,
    invite: String,
    is_host: bool,
    snapshot: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    history_error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionDetails {
    transport: &'static str,
    rtt_ms: Option<f64>,
}

#[tauri::command]
async fn lobby_connection_details(state: State<'_, Desktop>) -> Result<ConnectionDetails, String> {
    let slot = state.rooms.current.read().await;
    let session = slot.as_ref().ok_or("房间已断开")?;
    let (transport, rtt_ms) = match &session.guest {
        None => ("local", None),
        Some(guest) => match guest.selected_path() {
            Some((route, rtt)) => (route, Some(rtt.as_secs_f64() * 1000.0)),
            None => ("unknown", None),
        },
    };
    Ok(ConnectionDetails { transport, rtt_ms })
}

#[tauri::command]
async fn export_call_diagnostics(
    app: AppHandle,
    state: State<'_, Desktop>,
) -> Result<String, String> {
    let directory = app
        .path()
        .download_dir()
        .map_err(|_| "无法找到下载文件夹".to_owned())?;
    let diagnostics = state.diagnostics.clone();
    let path = tokio::task::spawn_blocking(move || diagnostics.export(&directory))
        .await
        .map_err(|_| "导出任务已中断".to_owned())?
        .map_err(|error| format!("无法保存到下载文件夹（{error}）"))?;
    Ok(path.to_string_lossy().into_owned())
}

fn executable(app: &AppHandle) -> Result<PathBuf, String> {
    let installed = app
        .path()
        .resolve("resources/livekit-server.exe", BaseDirectory::Resource)
        .map_err(|e| e.to_string())?;
    #[cfg(debug_assertions)]
    if !installed.is_file() {
        return Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/livekit-server.exe"));
    }
    Ok(installed)
}
#[tauri::command]
async fn host_lobby(
    app: AppHandle,
    state: State<'_, Desktop>,
    session_id: String,
) -> Result<ConnectionInfo, String> {
    let (host_start, opening) = state.rooms.begin_host(&session_id).await?;
    {
        let mut slot = state.rooms.current.write().await;
        opening.drain(&mut slot).await?;
    }
    let (identity, owner_nickname) = opening
        .prepare(async {
            let profile = state.profile.lock().await;
            let owner_nickname = player_profile::nickname(&profile.settings.nickname)?.to_owned();
            Ok((HostIdentity::from(&*profile), owner_nickname))
        })
        .await?;
    let executable = executable(&app)?;
    let diagnostics = state.diagnostics.room(Role::Host);
    let host = opening
        .prepare(diagnostics.measure(
            RoomPhase::StartHost,
            Host::start(&identity, &executable, owner_nickname, diagnostics.handle()),
        ))
        .await?;
    let gateway = host.gateway;
    let invite = host.invite.clone();
    let credential = host.credential.clone();
    let mut host = Some(host);
    let published = async {
        let (nickname, avatar) = opening
            .prepare(async {
                let profile = state.profile.lock().await;
                Ok((
                    player_profile::nickname(&profile.settings.nickname)?.to_owned(),
                    profile.settings.avatar.clone(),
                ))
            })
            .await?;
        let hello = json!({"type":"hello","inviteKey":identity.invite_key,"identity":credential,"nickname":nickname,"avatar":avatar});
        let accepted = opening
            .prepare(diagnostics.measure(
                RoomPhase::AcceptRoom,
                AcceptedRoom::connect(gateway, hello, &diagnostics),
            ))
            .await?;
        let info = ConnectionInfo {
            id: session_id.clone(),
            invite,
            is_host: true,
            snapshot: accepted.snapshot().clone(),
            history_error: None,
        };
        let profile = opening
            .prepare(async { Ok(state.profile.lock().await) })
            .await?;
        let mut slot = opening
            .prepare(async { Ok(state.rooms.current.write().await) })
            .await?;
        opening.publish(|| {
            diagnostics.mark(RoomPhase::Ready);
            let client = state.activate(app.clone(), accepted, &session_id, &diagnostics);
            let reply = sync_admitted_profile(&client.commands, &profile.settings, &nickname, &avatar);
            *slot = Some(Session {
                id: session_id.clone(),
                client,
                host: host.take(),
                guest: None,
                diagnostics,
            });
            (info, reply)
        })
    }
    .await;
    if let Some(host) = host {
        host.close().await;
    }
    let (info, reply) = published?;
    drop(opening);
    drop(host_start);
    if let Some(reply) = reply {
        if let Err(error) = reply.await {
            emit_room(
                &app,
                &session_id,
                json!({"type":"error","message":profile_sync_error(error)}),
            );
        }
    }
    Ok(info)
}

#[tauri::command]
async fn share_host_invite(state: State<'_, Desktop>) -> Result<String, String> {
    let key = state.profile.lock().await.invite_key.clone();
    let invite = {
        let slot = state.rooms.current.read().await;
        let host = slot
            .as_ref()
            .and_then(|s| s.host.as_ref())
            .ok_or("本机房间已关闭")?;
        host.overlay.share_invite(key)
    };
    invite.await
}

#[tauri::command]
async fn join_lobby(
    app: AppHandle,
    state: State<'_, Desktop>,
    session_id: String,
    invite: String,
) -> Result<ConnectionInfo, String> {
    let invite = invite.trim().to_owned();
    let ticket = Ticket::parse(&invite)?;
    let host_id = ticket.address.id;
    let opening = state.rooms.begin_join(&session_id, host_id).await?;
    let diagnostics = state.diagnostics.room(Role::Guest);
    let (name, id, avatar) = opening
        .prepare(async {
            let profile = state.profile.lock().await;
            if host_id == iroh::SecretKey::from_bytes(&profile.host_key).public() {
                return Err("这是本机房间的邀请".into());
            }
            Ok((
                player_profile::nickname(&profile.settings.nickname)?.to_owned(),
                profile.player_credential(&host_id.to_string()),
                profile.settings.avatar.clone(),
            ))
        })
        .await?;
    let guest = opening
        .prepare(diagnostics.measure(
            RoomPhase::ConnectTunnel,
            // Keep iroh's connection future off the Windows IPC thread stack.
            Box::pin(GuestEndpoint::start(&ticket, &diagnostics)),
        ))
        .await?;
    let hello = json!({"type":"hello","inviteKey":ticket.key,"identity":id,"nickname":name,"avatar":avatar,"playbackAddress":guest.media.to_string()});
    let accepted = match opening
        .prepare(diagnostics.measure(
            RoomPhase::AcceptRoom,
            AcceptedRoom::connect(guest.gateway, hello, &diagnostics),
        ))
        .await
    {
        Ok(accepted) => accepted,
        Err(error) => {
            guest.close().await;
            return Err(error);
        }
    };
    let snapshot = accepted.snapshot().clone();
    let mut candidate = Some((guest, accepted));
    if let Err(error) = state
        .rooms
        .replace(&opening, || {
            let (guest, accepted) = candidate.take().expect("prepared guest");
            diagnostics.mark(RoomPhase::Ready);
            Session {
                id: session_id.clone(),
                client: state.activate(app, accepted, &session_id, &diagnostics),
                host: None,
                guest: Some(guest),
                diagnostics,
            }
        })
        .await
    {
        let (guest, accepted) = candidate.expect("unpublished guest");
        drop(accepted);
        guest.close().await;
        return Err(error);
    }
    drop(opening);
    let nickname = snapshot["ownerNickname"]
        .as_str()
        .expect("checked by room handshake");
    let history_error = state
        .profile
        .lock()
        .await
        .remember_room(&host_id.to_string(), nickname, &invite)
        .err();
    Ok(ConnectionInfo {
        id: session_id,
        invite,
        is_host: false,
        snapshot,
        history_error,
    })
}

#[tauri::command]
async fn cancel_join(state: State<'_, Desktop>, session_id: String) -> Result<bool, String> {
    Ok(state.rooms.cancel_join(&session_id))
}

#[tauri::command]
async fn leave_lobby(state: State<'_, Desktop>, session_id: String) -> Result<(), String> {
    state.rooms.leave(&session_id).await
}
#[tauri::command]
async fn lobby_action(
    state: State<'_, Desktop>,
    session_id: String,
    action: Value,
) -> Result<(), String> {
    send_lobby_action(&state.rooms.current, &session_id, action).await
}

fn current_commands(
    session: &RwLock<Option<Session>>,
    session_id: Option<&str>,
) -> Result<Option<ClientCommands>, String> {
    let slot = session
        .try_read()
        .map_err(|_| "房间正在切换，操作未发送".to_owned())?;
    if let Some(id) = session_id {
        if slot.as_ref().is_some_and(|s| s.id != id) {
            return Err("房间已切换，操作未发送".into());
        }
    }
    Ok(slot.as_ref().map(|s| s.client.commands.clone()))
}

async fn send_lobby_action(
    session: &RwLock<Option<Session>>,
    session_id: &str,
    action: Value,
) -> Result<(), String> {
    let commands = current_commands(session, Some(session_id))?.ok_or("请先加入房间")?;
    commands.send(action).await
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn slow_ack_does_not_block_another_action_or_session_switch() {
        let (client, mut old_peer) = crate::client::tests::command_peer();
        let slot = Arc::new(RwLock::new(Some(Session {
            id: "old".into(),
            client,
            host: None,
            guest: None,
            diagnostics: Diagnostics::default().room(Role::Host),
        })));
        let send = |id: &'static str, text: &'static str| {
            let slot = slot.clone();
            tokio::spawn(async move {
                send_lobby_action(&slot, id, json!({"type":"text","text":text})).await
            })
        };
        let first = send("old", "first");
        let (_, first_ack) = old_peer.recv().await.unwrap();
        let details_reader = slot.read().await;
        let original = current_commands(&slot, Some("old")).unwrap().unwrap();
        let second = send("old", "second");
        let (second_message, second_ack) =
            tokio::time::timeout(Duration::from_secs(1), old_peer.recv())
                .await
                .expect("second action must reach the peer before the first ACK")
                .unwrap();
        assert_eq!(second_message["text"], "second");
        drop(details_reader);
        let (new_client, mut new_peer) = crate::client::tests::command_peer();
        {
            let mut switching = slot
                .try_write()
                .expect("pending commands must allow session replacement");
            assert!(
                send_lobby_action(&slot, "old", json!({"type":"text","text":"stale"}))
                    .await
                    .is_err()
            );
            switching.replace(Session {
                id: "new".into(),
                client: new_client,
                host: None,
                guest: None,
                diagnostics: Diagnostics::default().room(Role::Guest),
            });
        }
        drop(old_peer);
        assert!(original
            .send(json!({"type":"profile","nickname":"old"}))
            .await
            .is_err());
        assert_eq!(
            send_lobby_action(&slot, "old", json!({"type":"text","text":"stale"}))
                .await
                .unwrap_err(),
            "房间已切换，操作未发送"
        );
        let third = send("new", "third");
        let (third_message, third_ack) = new_peer.recv().await.unwrap();
        assert_eq!(third_message["text"], "third");
        third_ack.send(Ok(())).unwrap();
        second_ack.send(Ok(())).unwrap();
        first_ack.send(Ok(())).unwrap();
        assert!(
            first.await.unwrap().is_ok()
                && second.await.unwrap().is_ok()
                && third.await.unwrap().is_ok()
        );
    }

    #[tokio::test]
    async fn profile_saved_during_host_admission_is_queued_before_the_next_save() {
        let (client, mut host) = crate::client::tests::command_peer();
        let storage = tempfile::tempdir().unwrap();
        let mut profile = Profile::load(storage.path().into()).unwrap();
        let mut during_admission = profile.settings.clone();
        during_admission.nickname = "更新后的房主".into();
        during_admission.avatar = "updated-avatar".into();
        profile.save(during_admission).unwrap();

        let mut replay = Box::pin(
            sync_admitted_profile(&client.commands, &profile.settings, "原来的房主", "")
                .expect("profile changed since hello"),
        );
        let next = send_profile(&client.commands, "再次修改的房主", "");
        let (message, ack) = host.recv().await.unwrap();
        assert_eq!(
            message,
            json!({"type":"profile","nickname":"更新后的房主","avatar":"updated-avatar"})
        );
        let (next_message, next_ack) = host.recv().await.unwrap();
        assert_eq!(next_message["nickname"], "再次修改的房主");
        next_ack.send(Ok(())).unwrap();
        assert!(next.await.is_ok());
        assert!(futures_util::poll!(&mut replay).is_pending());
        ack.send(Ok(())).unwrap();
        assert!(replay.await.is_ok());
    }
}
#[tauri::command]
async fn get_preferences(state: State<'_, Desktop>) -> Result<Preferences, String> {
    Ok(state.profile.lock().await.settings.clone())
}

#[tauri::command]
async fn list_room_history(state: State<'_, Desktop>) -> Result<Vec<RecentRoom>, String> {
    state.profile.lock().await.room_history()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileUpdate {
    nickname: String,
    avatar: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_sync_error: Option<String>,
}

fn send_profile(
    client: &ClientCommands,
    nickname: &str,
    avatar: &str,
) -> impl std::future::Future<Output = Result<(), String>> + 'static {
    client.send(json!({"type":"profile","nickname":nickname,"avatar":avatar}))
}

fn sync_admitted_profile(
    client: &ClientCommands,
    settings: &Preferences,
    nickname: &str,
    avatar: &str,
) -> Option<impl std::future::Future<Output = Result<(), String>> + 'static> {
    let current = settings.nickname.trim();
    (current != nickname || settings.avatar != avatar)
        .then(|| send_profile(client, current, &settings.avatar))
}

fn profile_sync_error(error: String) -> String {
    format!("个人资料已保存在本机。当前房间同步失败，重新加入后将使用新资料。{error}")
}

#[tauri::command]
async fn set_profile(
    state: State<'_, Desktop>,
    nickname: String,
    avatar: String,
) -> Result<ProfileUpdate, String> {
    let nickname = player_profile::nickname(&nickname)?.to_owned();
    player_profile::validate_avatar(&avatar)?;
    let mut profile = state.profile.lock().await;
    let mut preferences = profile.settings.clone();
    preferences.nickname = nickname.clone();
    preferences.avatar = avatar.clone();
    profile.save(preferences)?;
    let pending = current_commands(&state.rooms.current, None)
        .map(|available| available.map(|client| send_profile(&client, &nickname, &avatar)));
    drop(profile);
    let sync = match pending {
        Ok(Some(reply)) => reply.await,
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    };
    Ok(ProfileUpdate {
        nickname,
        avatar,
        profile_sync_error: sync.err().map(profile_sync_error),
    })
}
#[tauri::command]
async fn list_devices(state: State<'_, Desktop>) -> Result<crate::voice::AudioDevices, String> {
    state.voice.devices().await
}
#[tauri::command]
async fn monitor_microphone(state: State<'_, Desktop>, active: bool) -> Result<(), String> {
    state.voice.monitor(active).await
}
#[tauri::command]
async fn preview_speaker_volume(state: State<'_, Desktop>, gain: u16) -> Result<(), String> {
    state.voice.preview_speaker_volume(gain).await
}
#[tauri::command]
async fn set_audio(
    app: AppHandle,
    state: State<'_, Desktop>,
    audio: AudioSettings,
) -> Result<(), String> {
    let mut profile = state.profile.lock().await;
    let old = profile.settings.audio.clone();
    let before = if profile.settings.shortcut_error.is_some() {
        None
    } else {
        shortcut(&old)?
    };
    let after = shortcut(&audio)?;
    if before != after {
        if let Some(new) = after {
            app.global_shortcut()
                .register(new)
                .map_err(|e| format!("快捷键注册失败：{e}"))?
        }
    }
    let result = async {
        state.voice.configure(audio.clone()).await?;
        let mut next = profile.settings.clone();
        next.audio = audio;
        next.shortcut_error = None;
        profile.save(next)
    }
    .await;
    if let Err(e) = result {
        if before != after {
            if let Some(new) = after {
                let _ = app.global_shortcut().unregister(new);
            }
        }
        let _ = state.voice.configure(old).await;
        return Err(e);
    }
    if before != after {
        state.voice.set_ptt(false);
        if let Some(key) = before {
            let _ = app.global_shortcut().unregister(key);
        }
    }
    Ok(())
}
fn shortcut(settings: &AudioSettings) -> Result<Option<Shortcut>, String> {
    if settings.mode != "ptt" {
        return Ok(None);
    }
    settings
        .shortcut
        .parse::<Shortcut>()
        .map(Some)
        .map_err(|_| "无效的全局快捷键".into())
}
#[tauri::command]
async fn set_peer(
    state: State<'_, Desktop>,
    id: String,
    volume: u16,
    muted: bool,
) -> Result<(), String> {
    if volume > 200 || id.is_empty() || id.len() > 128 {
        return Err("无效的队友音量".into());
    }
    let mut profile = state.profile.lock().await;
    let gain = PeerGain { volume, muted };
    let previous = profile.settings.peers.get(&id).copied().unwrap_or_default();
    state.voice.set_peer_gain(id.clone(), gain).await?;
    let mut next = profile.settings.clone();
    next.peers.insert(id.clone(), gain);
    if let Err(error) = profile.save(next) {
        state.voice.set_peer_gain(id, previous).await?;
        return Err(error);
    }
    Ok(())
}
#[tauri::command]
fn take_invite(state: State<'_, Desktop>) -> Option<String> {
    state.invite.lock().ok()?.take()
}

fn receive_invite(app: &AppHandle, url: String) {
    if Ticket::parse(&url).is_err() {
        return;
    }
    if let Some(state) = app.try_state::<Desktop>() {
        if let Ok(mut pending) = state.invite.lock() {
            *pending = Some(url.clone())
        }
        let _ = app.emit("invite-open", url);
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
    }
}

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _, event| {
                    if let Some(s) = app.try_state::<Desktop>() {
                        s.voice.set_ptt(event.state() == ShortcutState::Pressed)
                    }
                })
                .build(),
        )
        .setup(|app| {
            let directory = match std::env::var_os("DUIPIN_DATA_DIR") {
                Some(path) => PathBuf::from(path),
                None => app.path().app_data_dir()?,
            };
            let mut profile = Profile::load(directory).map_err(std::io::Error::other)?;
            let voice = Arc::new(VoiceEngine::new(app.handle().clone()));
            let hotkey = shortcut(&profile.settings.audio).and_then(|key| match key {
                Some(key) => app
                    .global_shortcut()
                    .register(key)
                    .map_err(|e| format!("按键说话快捷键注册失败，请在声音设置中更换：{e}")),
                None => Ok(()),
            });
            profile.settings.shortcut_error = hotkey.err();
            let settings = profile.settings.clone();
            app.manage(Desktop {
                profile: AsyncMutex::new(profile),
                voice: voice.clone(),
                diagnostics: Diagnostics::default(),
                rooms: RoomState::default(),
                available_update: AsyncMutex::new(None),
                invite: Mutex::new(None),
            });
            tauri::async_runtime::block_on(async {
                voice
                    .configure(settings.audio)
                    .await
                    .map_err(std::io::Error::other)?;
                for (id, gain) in settings.peers {
                    voice
                        .set_peer_gain(id, gain)
                        .await
                        .map_err(std::io::Error::other)?
                }
                Ok::<_, std::io::Error>(())
            })?;
            if let Some(urls) = app.deep_link().get_current()? {
                for url in urls {
                    receive_invite(app.handle(), url.to_string());
                }
            }
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |e| {
                for url in e.urls() {
                    receive_invite(&handle, url.to_string());
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            set_profile,
            list_room_history,
            host_lobby,
            share_host_invite,
            join_lobby,
            cancel_join,
            leave_lobby,
            lobby_action,
            get_preferences,
            lobby_connection_details,
            export_call_diagnostics,
            updates::set_auto_update,
            updates::check_update,
            updates::install_update,
            set_audio,
            set_peer,
            list_devices,
            monitor_microphone,
            preview_speaker_volume,
            take_invite
        ])
        .build(tauri::generate_context!())
        .expect("无法启动友声");
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
            let state = app.state::<Desktop>();
            if state.rooms.begin_shutdown() {
                api.prevent_exit();
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = handle.state::<Desktop>();
                    state.rooms.shutdown().await;
                    handle.exit(0);
                });
            } else if code.is_none() {
                api.prevent_exit();
            }
        }
    });
}
