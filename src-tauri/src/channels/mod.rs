use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::path::Path;
use uuid::Uuid;

pub(crate) struct Channel {
    pub id: String,
    pub name: String,
    pub owner: String,
}

#[derive(Serialize)]
pub(crate) struct ChannelPreview {
    id: String,
    name: String,
}

impl Channel {
    pub fn can_delete(&self, player: &str, admin: bool) -> bool {
        admin || self.owner == player
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_reads_saved_channels_while_host_is_offline() {
        let storage = tempfile::tempdir().unwrap();
        let file = storage.path().join("channels.sqlite3");
        assert!(Channels::preview(&file).unwrap().is_empty());
        assert!(!file.exists());
        let mut channels = Channels::open(&file).unwrap();
        let id = channels.create("host", "排位").unwrap();
        let removed = channels.create("host", "旧队伍").unwrap();
        channels.delete(&removed, "host", true).unwrap();
        drop(channels);
        let list = Channels::preview(&file).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].name, "排位");
    }

    #[test]
    fn names_are_limited_to_twenty_unicode_characters() {
        let storage = tempfile::tempdir().unwrap();
        let mut channels = Channels::open(&storage.path().join("channels.sqlite3")).unwrap();
        let name = "🎮".repeat(20);
        let id = channels.create("host", &name).unwrap();
        assert_eq!(channels.find(&id).unwrap().name, name);
        assert_eq!(
            channels.create("host", &"🎮".repeat(21)),
            Err("频道名称须为 1–20 个字符")
        );
        assert_eq!(
            channels.create("host", "包含\n换行"),
            Err("频道名称不能包含控制字符")
        );
        assert_eq!(channels.list.len(), 1);
    }

    #[test]
    fn saved_long_names_remain_available() {
        let storage = tempfile::tempdir().unwrap();
        let file = storage.path().join("channels.sqlite3");
        let channels = Channels::open(&file).unwrap();
        let name = "旧".repeat(40);
        channels
            .db
            .execute(
                "INSERT INTO channels (id, name, owner) VALUES (?1, ?2, ?3)",
                params!["old", name, "host"],
            )
            .unwrap();
        drop(channels);
        assert_eq!(Channels::preview(&file).unwrap()[0].name, name);
        let mut channels = Channels::open(&file).unwrap();
        assert_eq!(channels.find("old").unwrap().name, name);
        channels.delete("old", "host", false).unwrap();
        assert!(channels.list.is_empty());
    }
}

pub(crate) struct Channels {
    db: Connection,
    pub list: Vec<Channel>,
}

impl Channels {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS channels (
            id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL, owner TEXT NOT NULL
        )",
        )?;
        let list = Self::read_list(&db)?;
        Ok(Self { db, list })
    }

    fn read_list(db: &Connection) -> rusqlite::Result<Vec<Channel>> {
        db.prepare("SELECT id, name, owner FROM channels ORDER BY rowid")?
            .query_map([], |r| {
                Ok(Channel {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    owner: r.get(2)?,
                })
            })?
            .collect()
    }

    pub fn preview(path: &Path) -> Result<Vec<ChannelPreview>, String> {
        if !path.try_exists().map_err(|e| e.to_string())? {
            return Ok(Vec::new());
        }
        let list = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|db| Self::read_list(&db))
            .map_err(|e| format!("无法读取本机频道：{e}"))?;
        Ok(list
            .into_iter()
            .map(|c| ChannelPreview {
                id: c.id,
                name: c.name,
            })
            .collect())
    }

    pub fn find(&self, id: &str) -> Result<&Channel, &'static str> {
        self.list.iter().find(|c| c.id == id).ok_or("频道已删除")
    }

    pub fn create(&mut self, player: &str, name: &str) -> Result<String, &'static str> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 20 {
            return Err("频道名称须为 1–20 个字符");
        }
        if name.chars().any(char::is_control) {
            return Err("频道名称不能包含控制字符");
        }
        if self.list.len() >= 128 {
            return Err("大厅频道数量已达上限");
        }
        let id = Uuid::new_v4().to_string();
        self.db
            .execute(
                "INSERT INTO channels (id, name, owner) VALUES (?1, ?2, ?3)",
                params![id, name, player],
            )
            .map_err(|_| "频道保存失败")?;
        self.list.push(Channel {
            id: id.clone(),
            name: name.into(),
            owner: player.into(),
        });
        Ok(id)
    }

    pub fn delete(&mut self, id: &str, player: &str, admin: bool) -> Result<(), &'static str> {
        if !self.find(id)?.can_delete(player, admin) {
            return Err("只有频道创建者或大厅管理员可以删除频道");
        }
        self.db
            .execute("DELETE FROM channels WHERE id = ?1", [id])
            .map_err(|_| "频道删除失败")?;
        self.list.retain(|c| c.id != id);
        Ok(())
    }
}
