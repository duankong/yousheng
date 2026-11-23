use crate::voice::{AudioSettings, PeerGain};
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

pub fn random_key() -> [u8; 32] {
    let mut key = [0; 32];
    OsRng.fill_bytes(&mut key);
    key
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preferences {
    pub nickname: String,
    pub avatar: String,
    pub auto_update: bool,
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub shortcut_error: Option<String>,
    pub audio: AudioSettings,
    pub peers: HashMap<String, PeerGain>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            nickname: String::new(),
            avatar: String::new(),
            auto_update: true,
            shortcut_error: None,
            audio: AudioSettings::default(),
            peers: HashMap::new(),
        }
    }
}

pub struct Profile {
    pub directory: PathBuf,
    pub settings: Preferences,
    identity: [u8; 32],
    pub host_key: [u8; 32],
    pub invite_key: String,
}

fn file_error(error: impl std::fmt::Display) -> String {
    format!("无法读写本机配置：{error}")
}

impl Profile {
    pub fn load(directory: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&directory).map_err(file_error)?;
        let key_path = directory.join("identity.json");
        let keys: [[u8; 32]; 3] = match fs::read(&key_path) {
            Ok(data) => serde_json::from_slice(&data).map_err(file_error)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let keys = [random_key(), random_key(), random_key()];
                write_json(&key_path, &keys)?;
                keys
            }
            Err(e) => return Err(file_error(e)),
        };
        let settings = match fs::read(directory.join("settings.json")) {
            Ok(data) => serde_json::from_slice(&data).map_err(file_error)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Preferences::default(),
            Err(e) => return Err(file_error(e)),
        };
        Ok(Self {
            directory,
            settings,
            identity: keys[0],
            host_key: keys[1],
            invite_key: hex::encode(keys[2]),
        })
    }

    pub fn channel_database(&self) -> PathBuf {
        self.directory.join("channels.sqlite3")
    }

    pub fn player_credential(&self, host: &str) -> String {
        let mut hmac = Hmac::<Sha256>::new_from_slice(&self.identity).expect("fixed HMAC key");
        hmac.update(host.as_bytes());
        hex::encode(hmac.finalize().into_bytes())
    }

    pub fn save(&mut self, next: Preferences) -> Result<(), String> {
        write_json(&self.directory.join("settings.json"), &next)?;
        self.settings = next;
        Ok(())
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let tmp = path.with_extension("new");
    fs::write(&tmp, serde_json::to_vec(value).map_err(file_error)?).map_err(file_error)?;
    fs::rename(tmp, path).map_err(file_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_launch_requires_nickname_and_older_settings_keep_their_name() {
        let storage = tempfile::tempdir().unwrap();
        let profile = Profile::load(storage.path().into()).unwrap();
        assert!(profile.settings.nickname.is_empty());
        assert!(profile.settings.avatar.is_empty());
        assert!(profile.settings.auto_update);
        fs::write(
            storage.path().join("settings.json"),
            r#"{"nickname":"小雨"}"#,
        )
        .unwrap();
        let existing = Profile::load(storage.path().into()).unwrap();
        assert_eq!(existing.settings.nickname, "小雨");
        assert!(existing.settings.avatar.is_empty());
        assert!(existing.settings.auto_update);
        assert_eq!(existing.host_key, profile.host_key);
    }

    #[test]
    fn preferences_and_lobby_specific_identity_survive_restart() {
        let storage = tempfile::tempdir().unwrap();
        let mut first = Profile::load(storage.path().into()).unwrap();
        let credential = first.player_credential("host-a");
        assert_ne!(credential, first.player_credential("host-b"));
        for name in ["first", "second"] {
            let mut prefs = first.settings.clone();
            prefs.nickname = name.into();
            prefs.auto_update = false;
            first.save(prefs).unwrap();
        }
        let reopened = Profile::load(storage.path().into()).unwrap();
        assert_eq!(reopened.settings.nickname, "second");
        assert!(!reopened.settings.auto_update);
        assert_eq!(first.host_key, reopened.host_key);
        assert_eq!(first.invite_key, reopened.invite_key);
        assert_eq!(credential, reopened.player_credential("host-a"));
    }
}
