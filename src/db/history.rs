use super::{build_single_table_meta, read_message_row, DbManager, DbMessage, RawMsg};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Params};
use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::sync::Arc;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct HistoryCursor {
    pub time: i64,
    pub message_seq: i64,
    pub source: String,
}

pub(crate) enum HistoryBefore {
    Cursor(HistoryCursor),
    Sequence(i64),
}

pub(crate) struct HistoryPage {
    pub messages: Vec<(HistoryCursor, DbMessage)>,
    pub next_cursor: Option<HistoryCursor>,
}

#[derive(Clone, Debug)]
pub(crate) struct QuoteHistory {
    pub messages: Vec<DbMessage>,
    pub target_index: usize,
    pub complete_minutes: Range<i64>,
}

impl DbManager {
    pub(crate) async fn app_message_content(
        &self,
        chat: String,
        server_id: i64,
    ) -> Result<Option<String>> {
        let connections = self
            .ensure_msg_conns(false)?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            let hash = format!("{:x}", md5::compute(&chat));
            let mut content = None;
            for connection in connections {
                let connection = connection
                    .lock()
                    .map_err(|_| anyhow::anyhow!("历史消息数据库连接不可用"))?;
                for table in history_tables(&connection, &chat, &hash)? {
                    let meta = build_single_table_meta(&connection, &table)
                        .context("无法识别历史消息表结构")?;
                    let Some(server_id_col) = meta.server_id_col else {
                        continue;
                    };
                    let select = meta.select_from_sql;
                    let query = format!("{select} WHERE {server_id_col} = ?1 LIMIT 2");
                    for row in read_rows(&connection, &query, [server_id], &chat)? {
                        if row.msg_type & 0xffff == 49 {
                            anyhow::ensure!(content.is_none(), "原消息 ID 在会话中重复");
                            content = Some(row.content);
                        }
                    }
                }
            }
            Ok(content)
        })
        .await?
    }

    pub(crate) async fn quote_history(
        self: &Arc<Self>,
        chat: String,
        server_id: i64,
    ) -> Result<QuoteHistory> {
        let database = Arc::clone(self);
        let (messages, target_index, complete_minutes) = tokio::task::spawn_blocking(move || {
            let shards = database.ensure_msg_conns(true)?.clone();
            let hash = format!("{:x}", md5::compute(&chat));
            let mut tables = Vec::new();
            let mut target_time = None;
            for (shard, connection) in shards {
                let conn = connection
                    .lock()
                    .map_err(|_| anyhow::anyhow!("历史消息数据库连接不可用"))?;
                for table in history_tables(&conn, &chat, &hash)? {
                    let meta =
                        build_single_table_meta(&conn, &table).context("无法识别历史消息表结构")?;
                    if let Some(server_id_col) = &meta.server_id_col {
                        let sql = format!(
                            "{} WHERE {server_id_col} = ?1 LIMIT 2",
                            meta.select_from_sql
                        );
                        for row in read_rows(&conn, &sql, [server_id], &chat)? {
                            anyhow::ensure!(target_time.is_none(), "原消息 ID 在会话中重复");
                            target_time = Some(row.create_time);
                        }
                    }
                    tables.push((format!("{shard}:{table}"), Arc::clone(&connection), meta));
                }
            }

            let time = target_time
                .filter(|time| *time > 0)
                .context("本地微信历史中没有这条原消息或时间不可用")?;
            let minute = time.div_euclid(60) * 60;
            let complete_minutes = minute - 120..minute + 180;
            let mut rows = Vec::new();
            for (source, connection, meta) in tables {
                let conn = connection
                    .lock()
                    .map_err(|_| anyhow::anyhow!("历史消息数据库连接不可用"))?;
                let select = &meta.select_from_sql;
                let Some(clock) = &meta.time_col else {
                    continue;
                };
                let sequence = &meta.id_col;
                let core = format!("{select} WHERE {clock} >= ?1 AND {clock} < ?2 LIMIT 4097");
                let earlier = format!(
                    "{select} WHERE {clock} < ?1 ORDER BY {clock} DESC, {sequence} DESC LIMIT 24"
                );
                let later =
                    format!("{select} WHERE {clock} >= ?1 ORDER BY {clock}, {sequence} LIMIT 24");
                let mut context = read_rows(
                    &conn,
                    &core,
                    params![complete_minutes.start, complete_minutes.end],
                    &chat,
                )?;
                anyhow::ensure!(context.len() <= 4096, "引用目标附近的聊天记录过密");
                context.extend(read_rows(&conn, &earlier, [complete_minutes.start], &chat)?);
                context.extend(read_rows(&conn, &later, [complete_minutes.end], &chat)?);
                rows.extend(context.into_iter().map(|row| (source.clone(), row)));
            }
            rows.sort_by(|(left_source, left), (right_source, right)| {
                (left.create_time, left.local_id, left_source).cmp(&(
                    right.create_time,
                    right.local_id,
                    right_source,
                ))
            });
            let target_index = rows
                .iter()
                .position(|(_, row)| row.server_id == server_id)
                .context("微信历史已改变，无法核对原消息")?;
            Ok::<_, anyhow::Error>((
                rows.into_iter().map(|(_, row)| row).collect(),
                target_index,
                complete_minutes,
            ))
        })
        .await??;
        Ok(QuoteHistory {
            messages: self.describe_messages(messages).await,
            target_index,
            complete_minutes,
        })
    }

    pub(crate) async fn message_history(
        self: &Arc<Self>,
        chat: String,
        before: Option<HistoryBefore>,
        limit: usize,
    ) -> Result<HistoryPage> {
        let database = Arc::clone(self);
        let mut rows = tokio::task::spawn_blocking(move || {
            let shards = database.ensure_msg_conns(true)?.clone();
            let mut result = Vec::new();
            let hash = format!("{:x}", md5::compute(&chat));
            for (name, connection) in shards {
                let connection = connection
                    .lock()
                    .map_err(|_| anyhow::anyhow!("历史消息数据库连接不可用"))?;
                let tables = history_tables(&connection, &chat, &hash)?;
                for table in tables {
                    let mut messages = history_rows(
                        &connection,
                        &name,
                        &table,
                        &chat,
                        before.as_ref(),
                        limit + 1,
                    )
                    .with_context(|| format!("读取历史消息分片 {name} 失败"))?;
                    result.append(&mut messages);
                }
                result.sort_unstable_by(|left, right| right.0.cmp(&left.0));
                result.truncate(limit + 1);
            }
            Ok::<_, anyhow::Error>(result)
        })
        .await??;

        let next_cursor = if rows.len() > limit {
            rows.get(limit - 1).map(|(cursor, _)| cursor.clone())
        } else {
            None
        };
        rows.truncate(limit);
        rows.reverse();
        let (cursors, raw): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        let messages = self.describe_messages(raw).await;
        Ok(HistoryPage {
            messages: cursors.into_iter().zip(messages).collect(),
            next_cursor,
        })
    }
}

fn history_tables(conn: &Connection, chat: &str, hash: &str) -> Result<Vec<String>> {
    let legacy = conn
        .query_row(
            "SELECT rowid FROM Name2Id WHERE user_name = ?1",
            [chat],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .unwrap_or_default()
        .map(|id| format!("ChatMsg_{id}"))
        .unwrap_or_default();
    let mut statement = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table'
         AND name COLLATE NOCASE IN (?1, ?2, ?3)",
    )?;
    let tables = statement.query_map(
        params![format!("Msg_{hash}"), format!("Chat_{hash}"), legacy],
        |row| row.get(0),
    )?;
    Ok(tables.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn history_rows(
    conn: &Connection,
    shard: &str,
    table: &str,
    chat: &str,
    before: Option<&HistoryBefore>,
    limit: usize,
) -> Result<Vec<(HistoryCursor, RawMsg)>> {
    let meta = build_single_table_meta(conn, table).context("无法识别历史消息表结构")?;
    let source = format!("{shard}:{table}");
    let id = meta.id_col;
    let time = meta.history_time;
    let select = meta.select_from_sql;
    let order = format!("ORDER BY {time} DESC, {id} DESC LIMIT ?1");
    let limit = limit as i64;

    let rows = match before {
        Some(HistoryBefore::Cursor(cursor)) => {
            let comparison = if source < cursor.source { "<=" } else { "<" };
            read_rows(
                conn,
                &format!("{select} WHERE ({time}, {id}) {comparison} (?2, ?3) {order}"),
                params![limit, cursor.time, cursor.message_seq],
                chat,
            )?
        }
        Some(HistoryBefore::Sequence(sequence)) => read_rows(
            conn,
            &format!("{select} WHERE {id} < ?2 {order}"),
            params![limit, sequence],
            chat,
        )?,
        None => read_rows(conn, &format!("{select} {order}"), [limit], chat)?,
    };
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                HistoryCursor {
                    time: row.create_time,
                    message_seq: row.local_id,
                    source: source.clone(),
                },
                row,
            )
        })
        .collect())
}

fn read_rows(conn: &Connection, sql: &str, params: impl Params, chat: &str) -> Result<Vec<RawMsg>> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(params, |row| read_message_row(conn, row, chat))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
