use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatMessage {
    pub(crate) id: String,
    author_id: String,
    author_name: String,
    text: String,
    time: u64,
}

#[derive(Default)]
pub(crate) struct Chat(HashMap<String, VecDeque<ChatMessage>>);

pub(crate) const HISTORY_LIMIT: usize = 100;

impl Chat {
    pub fn send(
        &mut self,
        current: Option<&str>,
        target: &str,
        player: &str,
        name: &str,
        text: &str,
    ) -> Result<(), &'static str> {
        if current != Some(target) {
            return Err("只能发言到当前频道");
        }
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 1000 {
            return Err("消息须为 1–1000 个字符");
        }
        let messages = self.0.entry(target.into()).or_default();
        if messages.len() >= HISTORY_LIMIT {
            messages.pop_front();
        }
        messages.push_back(ChatMessage {
            id: Uuid::new_v4().to_string(),
            author_id: player.into(),
            author_name: name.into(),
            text: text.into(),
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        });
        Ok(())
    }

    pub fn history(&self, current: Option<&str>) -> Vec<&ChatMessage> {
        current
            .and_then(|id| self.0.get(id))
            .into_iter()
            .flatten()
            .collect()
    }

    pub fn clear(&mut self, id: &str) {
        self.0.remove(id);
    }
}
