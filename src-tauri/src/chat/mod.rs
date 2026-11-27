use serde::Serialize;
use std::{
    collections::VecDeque,
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
pub(crate) struct Chat(VecDeque<ChatMessage>);

pub(crate) const HISTORY_LIMIT: usize = 100;

impl Chat {
    pub fn send(&mut self, player: &str, name: &str, text: &str) -> Result<(), &'static str> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 1000 {
            return Err("消息须为 1–1000 个字符");
        }
        if self.0.len() >= HISTORY_LIMIT {
            self.0.pop_front();
        }
        self.0.push_back(ChatMessage {
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

    pub fn history(&self) -> &VecDeque<ChatMessage> {
        &self.0
    }
}
