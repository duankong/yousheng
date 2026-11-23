use rusqlite::{params, Connection};
use std::path::Path;
use uuid::Uuid;

pub(crate) struct Channel {
    pub id: String,
    pub name: String,
    pub owner: String,
}

impl Channel {
    pub fn can_delete(&self, player: &str, admin: bool) -> bool {
        admin || self.owner == player
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
        let list = db
            .prepare("SELECT id, name, owner FROM channels ORDER BY rowid")?
            .query_map([], |r| {
                Ok(Channel {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    owner: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Self { db, list })
    }

    pub fn find(&self, id: &str) -> Result<&Channel, &'static str> {
        self.list.iter().find(|c| c.id == id).ok_or("频道已删除")
    }

    pub fn create(&mut self, player: &str, name: &str) -> Result<String, &'static str> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 40 || name.chars().any(char::is_control) {
            return Err("频道名称须为 1–40 个字符");
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
