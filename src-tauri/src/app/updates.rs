use super::{Desktop, Session};
use serde::Serialize;
use std::{sync::atomic::Ordering, time::Duration};
use tauri::{AppHandle, State};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::RwLockWriteGuard;
use url::Url;

#[derive(Serialize)]
pub(super) struct UpdateInfo {
    version: String,
    notes: String,
}

#[tauri::command]
pub(super) async fn set_auto_update(
    state: State<'_, Desktop>,
    enabled: bool,
) -> Result<(), String> {
    let mut profile = state.profile.lock().await;
    let mut next = profile.settings.clone();
    next.auto_update = enabled;
    profile.save(next)
}

fn official_release_asset(url: &Url) -> bool {
    url.origin().ascii_serialization() == "https://github.com"
        && url
            .path()
            .starts_with("/duankong/yousheng/releases/download/")
}

fn update_notes(metadata: &serde_json::Value, fallback: Option<&str>) -> String {
    metadata["notesMarkdown"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .or(fallback)
        .unwrap_or_default()
        .to_owned()
}

#[tauri::command]
pub(super) async fn check_update(
    app: AppHandle,
    state: State<'_, Desktop>,
    manual: bool,
) -> Result<Option<UpdateInfo>, String> {
    let mut available = state.available_update.lock().await;
    if !manual && !state.profile.lock().await.settings.auto_update {
        return Ok(None);
    }
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("无法读取更新配置：{e}"))?;
    let mut update = updater
        .check()
        .await
        .map_err(|e| format!("无法检查 GitHub 更新：{e}"))?;
    if let Some(candidate) = &mut update {
        if !official_release_asset(&candidate.download_url) {
            return Err("更新包地址不属于友声的 GitHub Release".into());
        }
        candidate.timeout = Some(Duration::from_secs(300));
    }
    if !manual && !state.profile.lock().await.settings.auto_update {
        return Ok(None);
    }
    let info = update.as_ref().map(|candidate| UpdateInfo {
        version: candidate.version.clone(),
        notes: update_notes(&candidate.raw_json, candidate.body.as_deref()),
    });
    *available = update;
    Ok(info)
}

fn idle_session(state: &Desktop) -> Result<RwLockWriteGuard<'_, Option<Session>>, String> {
    let session = state
        .session
        .try_write()
        .map_err(|_| "大厅状态正在变化，请稍后安装更新")?;
    if session.is_some() || state.closing.load(Ordering::SeqCst) {
        return Err("请先退出或停止大厅，再安装更新".into());
    }
    Ok(session)
}

#[tauri::command]
pub(super) async fn install_update(
    state: State<'_, Desktop>,
    version: String,
) -> Result<(), String> {
    let available = state
        .available_update
        .try_lock()
        .map_err(|_| "更新正在处理中")?;
    let update = available
        .as_ref()
        .filter(|candidate| candidate.version == version)
        .ok_or("请重新检查更新后再安装")?;
    drop(idle_session(&state)?);
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|e| format!("下载或验证更新失败：{e}"))?;
    // A player may have opened a lobby during the download. Hold the same lock
    // that host_lobby and join_lobby use until the Windows installer takes over.
    let _session = idle_session(&state)?;
    update
        .install(bytes)
        .map_err(|e| format!("启动更新安装程序失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_notes_prefer_markdown_and_accept_older_manifests() {
        use serde_json::json;

        let plain = Some("更新内容");
        assert_eq!(
            update_notes(&json!({"notesMarkdown": "## 更新内容"}), plain),
            "## 更新内容"
        );
        for manifest in [
            json!({}),
            json!({"notesMarkdown": 42}),
            json!({"notesMarkdown": " "}),
        ] {
            assert_eq!(update_notes(&manifest, plain), "更新内容");
        }
        assert_eq!(update_notes(&json!({}), None), "");
    }

    #[test]
    fn update_asset_belongs_to_official_github_release() {
        for (address, expected) in [
            (
                "https://github.com/duankong/yousheng/releases/download/v0.1.2/setup.exe",
                true,
            ),
            (
                "https://github.com/other/yousheng/releases/download/v0.1.2/setup.exe",
                false,
            ),
            (
                "https://github.com.example/duankong/yousheng/releases/download/v0.1.2/setup.exe",
                false,
            ),
            (
                "http://github.com/duankong/yousheng/releases/download/v0.1.2/setup.exe",
                false,
            ),
        ] {
            assert_eq!(
                official_release_asset(&Url::parse(address).unwrap()),
                expected
            );
        }
    }
}
