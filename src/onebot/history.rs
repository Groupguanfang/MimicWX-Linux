use super::{message_id, required_id, OneBotRuntime};
use crate::db::{DbManager, HistoryBefore, HistoryCursor, HistoryPage};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::warn;

const HISTORY_PAGE_LIMIT: u64 = 100;

impl OneBotRuntime {
    pub(super) async fn message_history(
        &self,
        database: Option<&Arc<DbManager>>,
        group: bool,
        params: &Value,
    ) -> Result<Value, String> {
        let chat = required_id(params, if group { "group_id" } else { "user_id" })?;
        if group != chat.ends_with("@chatroom") {
            return Err("历史消息的会话类型与 ID 不一致".into());
        }
        let sequence = params
            .get("message_seq")
            .filter(|value| !value.is_null())
            .map(|value| value.as_i64().ok_or("message_seq 必须是整数"))
            .transpose()?;
        let cursor = params
            .get("cursor")
            .filter(|value| !value.is_null())
            .map(|value| {
                serde_json::from_value::<HistoryCursor>(value.clone()).map_err(|_| "cursor 无效")
            })
            .transpose()?;
        if sequence.is_some() && cursor.is_some() {
            return Err("cursor 与 message_seq 只能传其中一个".into());
        }
        let limit = params
            .get("count")
            .filter(|count| !count.is_null())
            .map(|value| {
                value
                    .as_u64()
                    .filter(|count| *count > 0)
                    .ok_or("count 必须是正整数")
            })
            .transpose()?
            .unwrap_or(HISTORY_PAGE_LIMIT)
            .min(HISTORY_PAGE_LIMIT) as usize;

        let has_cursor = cursor.is_some();
        let before = cursor
            .map(HistoryBefore::Cursor)
            .or(sequence.map(HistoryBefore::Sequence));
        if let Some(database) = database {
            match database.message_history(chat.clone(), before, limit).await {
                Ok(page) => return Ok(self.render_history(page).await),
                Err(error) => warn!(%error, "微信历史数据库查询失败"),
            }
        }
        if has_cursor {
            return Err("微信历史数据库暂不可用，请使用同一个游标重试".into());
        }

        let mut messages = self
            .messages
            .lock()
            .await
            .history(&chat, sequence, limit + 1);
        let has_more = messages.len() > limit;
        if has_more {
            messages.remove(0);
        }
        Ok(json!({
            "messages": messages,
            "has_more": has_more,
            "next_cursor": null,
            "history_source": "cache",
        }))
    }

    async fn render_history(&self, page: HistoryPage) -> Value {
        let mut messages = Vec::with_capacity(page.messages.len());
        for (cursor, message) in page.messages {
            let id = if message.server_id == 0 {
                format!(
                    "local_{}_{}_{:x}",
                    message.chat,
                    message.local_id,
                    md5::compute(&cursor.source)
                )
            } else {
                message_id(&message)
            };
            let event = self
                .render_message(&message, &id, &self.history_messages)
                .await;
            self.history_messages.lock().await.insert(id, event.clone());
            messages.push(event);
        }
        json!({
            "messages": messages,
            "has_more": page.next_cursor.is_some(),
            "next_cursor": page.next_cursor,
            "history_source": "database",
        })
    }
}
