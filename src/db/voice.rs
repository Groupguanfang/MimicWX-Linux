use super::DbManager;
use anyhow::{bail, Result};

const MAX_VOICE_BYTES: usize = 2 * 1024 * 1024;

impl DbManager {
    pub(crate) async fn verify_voice_sent(
        &self,
        chat: &str,
        started_at: i64,
        duration_ms: u32,
        mut messages: tokio::sync::broadcast::Receiver<super::DbMessage>,
    ) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut refresh = tokio::time::interval(std::time::Duration::from_millis(500));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                result = messages.recv() => {
                    match result {
                        Ok(message) => {
                            if message.chat == chat && message.is_self
                                && message.create_time >= started_at - 2
                                && matches!(message.parsed, super::MsgContent::Voice {
                                    duration_ms: Some(actual)
                                } if actual.abs_diff(duration_ms) <= 2500)
                                && self.claim_receipt(&message)
                            {
                                tracing::info!("✅ [DB] 语音发送验证成功: {chat}");
                                return Ok(true);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(false),
                    }
                }
                _ = refresh.tick() => self.refresh_receipts(),
                _ = tokio::time::sleep_until(deadline) => return Ok(false),
            }
        }
    }

    pub(crate) async fn voice_data(&self, chat: &str, server_id: i64) -> Result<Option<Vec<u8>>> {
        if chat.is_empty() || server_id <= 0 {
            return Ok(None);
        }
        let connections = self
            .ensure_msg_conns(false)?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let directory = self.db_dir.clone();
        let snapshots = std::sync::Arc::clone(&self.snapshots);
        let chat = chat.to_string();
        tokio::task::spawn_blocking(move || {
            let table = format!("Msg_{:x}", md5::compute(&chat));
            let query = format!(
                "SELECT local_id, create_time FROM [{table}]
                 WHERE server_id=?1 AND (local_type & 65535)=34 LIMIT 2"
            );
            let mut message = None;
            for connection in connections {
                let connection = connection
                    .lock()
                    .map_err(|_| anyhow::anyhow!("语音消息连接不可用"))?;
                let Ok(mut statement) = connection.prepare(&query) else {
                    continue;
                };
                let mut rows = statement.query([server_id])?;
                while let Some(row) = rows.next()? {
                    if message.is_some() {
                        bail!("语音消息不唯一");
                    }
                    message = Some((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?));
                }
            }
            let Some((local_id, create_time)) = message else {
                return Ok(None);
            };
            let mut found = None;
            for entry in std::fs::read_dir(directory.join("message"))?.take(512) {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if !media_database_name(&name) {
                    continue;
                }
                let Ok(connection) = snapshots.open(&format!("message/{name}")) else {
                    continue;
                };
                let Ok(mut statement) = connection.prepare(
                    "SELECT voice_data, length(voice_data) FROM VoiceInfo
                     WHERE chat_name_id IN (SELECT rowid FROM Name2Id WHERE user_name=?1)
                       AND local_id=?2 AND create_time=?3 AND (svr_id=?4 OR svr_id=0)
                     LIMIT 2",
                ) else {
                    continue;
                };
                let mut rows =
                    statement.query(rusqlite::params![chat, local_id, create_time, server_id])?;
                while let Some(row) = rows.next()? {
                    let length: usize = row.get(1)?;
                    if length == 0 {
                        continue;
                    }
                    if length > MAX_VOICE_BYTES {
                        bail!("微信语音超过 2 MiB");
                    }
                    if found.is_some() {
                        bail!("语音音频记录不唯一");
                    }
                    found = Some(row.get::<_, Vec<u8>>(0)?);
                }
            }
            Ok(found)
        })
        .await?
    }
}

fn media_database_name(name: &str) -> bool {
    name.strip_prefix("media_")
        .and_then(|name| name.strip_suffix(".db"))
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_numbered_media_shards() {
        assert!(media_database_name("media_0.db"));
        assert!(media_database_name("media_12.db"));
        for name in [
            "media_.db",
            "media_0.db-wal",
            "media_0.db/other",
            "message_0.db",
            "../media_0.db",
        ] {
            assert!(!media_database_name(name));
        }
    }

    #[tokio::test]
    async fn reads_a_voice_from_a_numbered_media_shard() {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let directory =
            std::env::temp_dir().join(format!("mimicwx-voice-read-{}", u64::from_le_bytes(random)));
        std::fs::create_dir_all(directory.join("message")).unwrap();
        let key = [0u8; 32];
        let chat = "wxid_voice_fixture";
        let server_id = 3622866258353243884_i64;
        let local_id = 48_i64;
        let create_time = 1794912685_i64;

        let message_path = directory.join("message/message_0.db");
        let message = crate::db::tests::open_fixture_database(&message_path, &key);
        let table = format!("Msg_{:x}", md5::compute(chat));
        message
            .execute_batch(&format!(
                "PRAGMA query_only=OFF;
                 CREATE TABLE [{table}] (
                    local_id INTEGER, server_id INTEGER, create_time INTEGER, local_type INTEGER
                 );
                 INSERT INTO [{table}] VALUES ({local_id}, {server_id}, {create_time}, 34);"
            ))
            .unwrap();
        drop(message);

        let media_path = directory.join("message/media_0.db");
        let media = crate::db::tests::open_fixture_database(&media_path, &key);
        media
            .execute_batch(
                "PRAGMA query_only=OFF;
                 CREATE TABLE Name2Id (user_name TEXT NOT NULL);
                 CREATE TABLE VoiceInfo (
                    chat_name_id INTEGER, local_id INTEGER, svr_id INTEGER,
                    create_time INTEGER, voice_data BLOB
                 );",
            )
            .unwrap();
        media
            .execute(
                "INSERT INTO Name2Id (rowid, user_name) VALUES (1, ?1)",
                [chat],
            )
            .unwrap();
        media
            .execute(
                "INSERT INTO VoiceInfo VALUES (1, ?1, ?2, ?3, ?4)",
                rusqlite::params![local_id, server_id, create_time, vec![1u8, 2, 3, 4]],
            )
            .unwrap();
        drop(media);

        let manager = DbManager::new("0".repeat(64), directory.clone()).unwrap();
        assert_eq!(
            manager.voice_data(chat, server_id).await.unwrap(),
            Some(vec![1, 2, 3, 4])
        );
        assert_eq!(manager.voice_data(chat, 1).await.unwrap(), None);

        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
