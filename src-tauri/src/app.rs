use crate::{
    channels::{ChannelPreview, Channels},
    client::{Client, ClientCommands},
    host::Host,
    network::{GuestEndpoint, Ticket},
    player_profile,
    preferences::{Preferences, Profile},
    voice::{AudioSettings, PeerGain, VoiceEngine},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
};
use tauri::{path::BaseDirectory, AppHandle, Emitter, Manager, State};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

mod updates;

struct Session {
    client: Client,
    host: Option<Host>,
    guest: Option<GuestEndpoint>,
}
struct Desktop {
    profile: AsyncMutex<Profile>,
    voice: Arc<VoiceEngine>,
    session: RwLock<Option<Session>>,
    available_update: AsyncMutex<Option<tauri_plugin_updater::Update>>,
    invite: Mutex<Option<String>>,
    closing: AtomicBool,
}
impl Desktop {
    async fn teardown(&self, session: Session) {
        session.client.close().await;
        self.voice.disconnect().await;
        if let Some(host) = session.host {
            host.close().await;
        }
        if let Some(guest) = session.guest {
            guest.close().await;
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionInfo {
    invite: String,
    is_host: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionDetails {
    transport: &'static str,
    rtt_ms: Option<f64>,
}

#[tauri::command]
async fn lobby_connection_details(state: State<'_, Desktop>) -> Result<ConnectionDetails, String> {
    let slot = state.session.read().await;
    let session = slot.as_ref().ok_or("大厅已断开")?;
    let (transport, rtt_ms) = match &session.guest {
        None => ("local", None),
        Some(guest) => match guest.selected_path() {
            Some((route, rtt)) => (route, Some(rtt.as_secs_f64() * 1000.0)),
            None => ("unknown", None),
        },
    };
    Ok(ConnectionDetails { transport, rtt_ms })
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
fn save_name(profile: &mut Profile, name: &str) -> Result<(), String> {
    if profile.settings.nickname == name {
        return Ok(());
    }
    let mut next = profile.settings.clone();
    next.nickname = name.to_owned();
    profile.save(next)
}

#[tauri::command]
async fn host_lobby(
    app: AppHandle,
    state: State<'_, Desktop>,
    nickname: String,
) -> Result<ConnectionInfo, String> {
    let name = player_profile::nickname(&nickname)?;
    let mut slot = state.session.write().await;
    if let Some(session) = slot.take() {
        state.teardown(session).await;
    }
    let mut profile = state.profile.lock().await;
    save_name(&mut profile, name)?;
    let host = Host::start(&profile, &executable(&app)?).await?;
    let hello = json!({"type":"hello","inviteKey":profile.invite_key,"identity":host.credential,"nickname":name,"avatar":profile.settings.avatar});
    drop(profile);
    let result = Client::open(app, state.voice.clone(), host.gateway, hello).await;
    match result {
        Ok(client) => {
            let info = ConnectionInfo {
                invite: host.invite.clone(),
                is_host: true,
            };
            *slot = Some(Session {
                client,
                host: Some(host),
                guest: None,
            });
            Ok(info)
        }
        Err(e) => {
            host.close().await;
            Err(e)
        }
    }
}

#[tauri::command]
async fn share_host_invite(state: State<'_, Desktop>) -> Result<String, String> {
    let key = state.profile.lock().await.invite_key.clone();
    let invite = {
        let slot = state.session.read().await;
        let host = slot
            .as_ref()
            .and_then(|s| s.host.as_ref())
            .ok_or("本机大厅已关闭")?;
        host.overlay.share_invite(key)
    };
    invite.await
}

#[tauri::command]
async fn join_lobby(
    app: AppHandle,
    state: State<'_, Desktop>,
    invite: String,
    nickname: String,
) -> Result<ConnectionInfo, String> {
    let ticket = Ticket::parse(&invite)?;
    let name = player_profile::nickname(&nickname)?;
    let mut slot = state.session.write().await;
    let mut profile = state.profile.lock().await;
    if ticket.address.id == iroh::SecretKey::from_bytes(&profile.host_key).public() {
        return Err("这是本机大厅的邀请，请选择「开启大厅」".into());
    }
    save_name(&mut profile, name)?;
    let id = profile.player_credential(&ticket.address.id.to_string());
    let avatar = profile.settings.avatar.clone();
    drop(profile);
    if let Some(session) = slot.take() {
        state.teardown(session).await;
    }
    let guest = GuestEndpoint::start(&ticket).await?;
    let hello = json!({"type":"hello","inviteKey":ticket.key,"identity":id,"nickname":name,"avatar":avatar,"playbackAddress":guest.media.to_string()});
    match Client::open(app, state.voice.clone(), guest.gateway, hello).await {
        Ok(client) => {
            *slot = Some(Session {
                client,
                host: None,
                guest: Some(guest),
            });
            Ok(ConnectionInfo {
                invite,
                is_host: false,
            })
        }
        Err(e) => {
            guest.close().await;
            Err(e)
        }
    }
}

#[tauri::command]
async fn leave_lobby(state: State<'_, Desktop>) -> Result<(), String> {
    let mut slot = state.session.write().await;
    if let Some(session) = slot.take() {
        state.teardown(session).await;
    }
    Ok(())
}
#[tauri::command]
async fn lobby_action(state: State<'_, Desktop>, action: Value) -> Result<(), String> {
    send_lobby_action(&state.session, action).await
}

fn current_commands(session: &RwLock<Option<Session>>) -> Result<Option<ClientCommands>, String> {
    let slot = session
        .try_read()
        .map_err(|_| "大厅正在切换，操作未发送".to_owned())?;
    Ok(slot.as_ref().map(|s| s.client.commands.clone()))
}

async fn send_lobby_action(session: &RwLock<Option<Session>>, action: Value) -> Result<(), String> {
    let commands = current_commands(session)?.ok_or("请先加入大厅")?;
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
            client,
            host: None,
            guest: None,
        })));
        let send = |text: &'static str| {
            let slot = slot.clone();
            tokio::spawn(async move {
                send_lobby_action(&slot, json!({"type":"text","text":text})).await
            })
        };
        let first = send("first");
        let (_, first_ack) = old_peer.recv().await.unwrap();
        let details_reader = slot.read().await;
        let original = current_commands(&slot).unwrap().unwrap();
        let second = send("second");
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
                send_lobby_action(&slot, json!({"type":"text","text":"stale"}))
                    .await
                    .is_err()
            );
            switching.replace(Session {
                client: new_client,
                host: None,
                guest: None,
            });
        }
        drop(old_peer);
        assert!(original
            .send(json!({"type":"profile","nickname":"old"}))
            .await
            .is_err());
        let third = send("third");
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
}
#[tauri::command]
async fn get_preferences(state: State<'_, Desktop>) -> Result<Preferences, String> {
    Ok(state.profile.lock().await.settings.clone())
}

#[tauri::command]
async fn list_local_channels(state: State<'_, Desktop>) -> Result<Vec<ChannelPreview>, String> {
    let path = state.profile.lock().await.channel_database();
    Channels::preview(&path)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileUpdate {
    #[serde(flatten)]
    preferences: Preferences,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_sync_error: Option<String>,
}

#[tauri::command]
async fn set_profile(
    state: State<'_, Desktop>,
    nickname: String,
    avatar: String,
) -> Result<ProfileUpdate, String> {
    let name = player_profile::nickname(&nickname)?;
    player_profile::validate_avatar(&avatar)?;
    let commands = current_commands(&state.session);
    let mut profile = state.profile.lock().await;
    let mut preferences = profile.settings.clone();
    preferences.nickname = name.into();
    preferences.avatar = avatar;
    profile.save(preferences.clone())?;
    let pending = commands.map(|available| {
        available.map(|client| {
            client.send(json!({"type":"profile","nickname":name,"avatar":preferences.avatar}))
        })
    });
    drop(profile);
    let sync = match pending {
        Ok(Some(reply)) => reply.await,
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    };
    let profile_sync_error = sync.err().map(|error| {
        format!("个人资料已保存在本机。当前大厅同步失败，重进大厅后将使用新资料。{error}")
    });
    Ok(ProfileUpdate {
        preferences,
        profile_sync_error,
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
                session: RwLock::new(None),
                available_update: AsyncMutex::new(None),
                invite: Mutex::new(None),
                closing: AtomicBool::new(false),
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
            list_local_channels,
            host_lobby,
            share_host_invite,
            join_lobby,
            leave_lobby,
            lobby_action,
            get_preferences,
            lobby_connection_details,
            updates::set_auto_update,
            updates::check_update,
            updates::install_update,
            set_audio,
            set_peer,
            list_devices,
            monitor_microphone,
            take_invite
        ])
        .build(tauri::generate_context!())
        .expect("无法启动友声");
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            let state = app.state::<Desktop>();
            if !state.closing.swap(true, Ordering::SeqCst) {
                api.prevent_exit();
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = handle.state::<Desktop>();
                    if let Some(session) = state.session.write().await.take() {
                        state.teardown(session).await;
                    }
                    handle.exit(0);
                });
            }
        }
    });
}
