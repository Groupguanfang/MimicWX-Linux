//! 数据库监听模块
//!
//! 通过 SQLCipher 解密 + inotify 监听数据库变化，实现:
//! - 联系人查询 (contact.db)
//! - 会话列表 (session.db)
//! - 增量消息获取 (message_0.db)
//!
//! 替代原有 AT-SPI2 轮询方案，完全非侵入。
//!
//! 所有读取都使用独立快照，绝不映射、锁定或修改微信的数据库和共享索引。
//!
//! 设计: rusqlite::Connection 是 !Send, 不能跨 .await 持有。
//! 策略: 所有 DB 操作在 spawn_blocking 中完成, 异步方法只操作缓存。

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, trace, warn};

mod history;
mod room;
mod snapshot;
mod voice;
mod watch;

pub(crate) use history::{HistoryBefore, HistoryCursor, HistoryPage, QuoteHistory};
pub(crate) use room::GroupMember;

// =====================================================================
// FFI: sqlite3_key (WCDB 密钥传递方式)
// =====================================================================

extern "C" {
    /// WCDB 使用 sqlite3_key() C API 传递 raw key (非 PRAGMA key).
    /// SQLCipher 会对这个 key 做 PBKDF2 派生.
    fn sqlite3_key(
        db: *mut std::ffi::c_void,
        key: *const u8,
        key_len: std::ffi::c_int,
    ) -> std::ffi::c_int;
}

// =====================================================================
// 类型定义
// =====================================================================

/// 联系人信息
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContactInfo {
    pub username: String,
    pub nick_name: String,
    pub remark: String,
    pub alias: String,
    /// 优先显示名: remark > nick_name > username
    pub display_name: String,
}

/// 会话信息 (来自数据库)
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbSessionInfo {
    pub username: String,
    pub display_name: String,
    pub unread_count: i32,
    pub summary: String,
    pub last_timestamp: i64,
    pub last_msg_sender: String,
}

/// 会话表里足以提前派发纯文本消息的快照。
#[derive(Debug, Clone)]
pub struct SessionMessage {
    pub chat: String,
    pub summary: String,
    pub timestamp: i64,
    pub local_id: i64,
    pub server_id: i64,
    pub unread: bool,
    pub msg_type: i64,
    pub sub_type: i64,
    pub ext_type: i64,
    pub sender: String,
    pub sender_display_name: String,
}

impl SessionMessage {
    pub fn can_dispatch_text(&self, self_wxid: &str) -> bool {
        let incoming = if self.chat.ends_with("@chatroom") {
            !self.sender.is_empty() && self.sender != self_wxid
        } else {
            self.unread && self.chat != self_wxid
        };
        self.local_id > 0
            && self.timestamp > 0
            && self.msg_type == 1
            && self.sub_type == 0
            && self.ext_type == 0
            && !self.summary.is_empty()
            && self.summary.len() < 192
            && incoming
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionCursor {
    local_id: i64,
    timestamp: i64,
}

impl SessionCursor {
    fn matches(&self, message: &SessionMessage) -> bool {
        self.local_id == message.local_id && self.timestamp == message.timestamp
    }
}

/// 结构化消息内容 (按 msg_type 解析)
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "data")]
pub enum MsgContent {
    /// 纯文本 (msg_type=1)
    Text { text: String },
    /// 图片 (msg_type=3)
    Image { path: Option<String> },
    /// 语音 (msg_type=34)
    Voice { duration_ms: Option<u32> },
    /// 视频 (msg_type=43)
    Video { thumb_path: Option<String> },
    /// 表情包 (msg_type=47)
    Emoji { url: Option<String> },
    /// 链接/文件/小程序 (msg_type=49)
    App {
        title: Option<String>,
        desc: Option<String>,
        url: Option<String>,
        app_type: Option<i32>,
    },
    /// 系统消息 (msg_type=10000/10002)
    System { text: String },
    /// 未知类型
    Unknown { raw: String, msg_type: i64 },
}

impl MsgContent {
    /// 消息类型的简短描述 (用于日志)
    pub fn type_label(&self) -> &'static str {
        match self {
            Self::Text { .. } => "文本",
            Self::Image { .. } => "图片",
            Self::Voice { .. } => "语音",
            Self::Video { .. } => "视频",
            Self::Emoji { .. } => "表情",
            Self::App { .. } => "链接",
            Self::System { .. } => "系统",
            Self::Unknown { .. } => "未知",
        }
    }

    /// 日志预览文本
    pub fn preview(&self, max_len: usize) -> String {
        let text = match self {
            Self::Text { text } => text.clone(),
            Self::Image { .. } => "[图片]".into(),
            Self::Voice { duration_ms, .. } => match duration_ms {
                Some(ms) if *ms >= 1000 => format!("[语音 {}s]", ms / 1000),
                Some(ms) if *ms > 0 => format!("[语音 {ms}ms]"),
                _ => "[语音]".into(),
            },
            Self::Video { .. } => "[视频]".into(),
            Self::Emoji { url, .. } => format!("[表情] {}", url.as_deref().unwrap_or("")),
            Self::App {
                title,
                desc,
                app_type,
                ..
            } => {
                let t = title.as_deref().unwrap_or("");
                let d = desc.as_deref().unwrap_or("");
                // 子类型 + 标题后缀推断
                let label = match app_type.unwrap_or(0) {
                    3 => "音乐",
                    6 => "文件",
                    19 => "转发",
                    33 | 36 => "小程序",
                    42 => "名片",
                    2000 => "转账",
                    2001 => "红包",
                    _ => {
                        // 子类型提取失败时, 用标题后缀推断文件
                        let tl = t.to_lowercase();
                        if tl.ends_with(".pdf")
                            || tl.ends_with(".doc")
                            || tl.ends_with(".docx")
                            || tl.ends_with(".xls")
                            || tl.ends_with(".xlsx")
                            || tl.ends_with(".ppt")
                            || tl.ends_with(".pptx")
                            || tl.ends_with(".zip")
                            || tl.ends_with(".rar")
                            || tl.ends_with(".7z")
                            || tl.ends_with(".txt")
                            || tl.ends_with(".csv")
                            || tl.ends_with(".apk")
                            || tl.ends_with(".exe")
                            || tl.ends_with(".dmg")
                        {
                            "文件"
                        } else {
                            "链接"
                        }
                    }
                };
                if !t.is_empty() {
                    format!("[{label}] {t}")
                } else if !d.is_empty() {
                    format!("[{label}] {d}")
                } else {
                    format!("[{label}]")
                }
            }
            Self::System { text } => format!("[系统] {text}"),
            Self::Unknown { msg_type, .. } => format!("[type={msg_type}]"),
        };
        if text.len() > max_len {
            format!("{}...", &text[..text.floor_char_boundary(max_len)])
        } else {
            text
        }
    }
}

/// 数据库消息
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbMessage {
    pub local_id: i64,
    pub server_id: i64,
    pub create_time: i64,
    /// 原始 content 字符串 (向后兼容)
    pub content: String,
    #[serde(skip_serializing)]
    pub source: String,
    /// 结构化解析结果
    pub parsed: MsgContent,
    pub msg_type: i64,
    /// 发言人 wxid (群聊中有意义)
    pub talker: String,
    /// 发言人显示名 (通过联系人缓存解析)
    pub talker_display_name: String,
    /// 发言人实际昵称 (contact.nick_name)
    pub talker_nick_name: String,
    /// 发言人备注 (contact.remark)
    pub talker_remark: String,
    /// 所属会话
    pub chat: String,
    /// 所属会话显示名
    pub chat_display_name: String,
    /// 所属会话实际昵称 (contact.nick_name)
    pub chat_nick_name: String,
    /// 所属会话备注 (contact.remark)
    pub chat_remark: String,
    /// 是否为自己发送的消息
    pub is_self: bool,
}

/// 原始消息 (同步查询返回, 后续异步填充显示名)
struct RawMsg {
    local_id: i64,
    server_id: i64,
    create_time: i64,
    content: String,
    source: String,
    msg_type: i64,
    talker: String,
    chat: String,
    status: i64,
}

pub(crate) struct LocalImageMetadata {
    pub account: PathBuf,
    pub wxid: String,
    pub chat: String,
    pub create_time: i64,
    pub hashes: Vec<String>,
    pub content: String,
}

// =====================================================================
// DbManager — 核心结构
// =====================================================================

/// 消息表结构元数据缓存 (避免每次查询重新执行 PRAGMA table_info)
#[derive(Debug, Clone)]
struct TableMeta {
    /// 表名
    table: String,
    /// 预编译的 SELECT SQL
    select_sql: String,
    select_from_sql: String,
    history_time: String,
    time_col: Option<String>,
    server_id_col: Option<String>,
    /// ID 列名 (local_id / rowid)
    id_col: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DatabaseFileIdentity {
    files: [Option<(u64, u64)>; 3],
    revisions: [Option<(u64, i64, i64)>; 2],
}

pub struct DbManager {
    /// 32 字节原始密钥
    key_bytes: Vec<u8>,
    /// 数据库存储目录 (如 /home/wechat/.local/share/weixin/db_storage/)
    db_dir: PathBuf,
    /// 独立只读快照；微信库只在复制时被顺序读取
    snapshots: Arc<snapshot::SnapshotReader>,
    /// 当前登录账号的 wxid (从 db_dir 路径提取, 用于判断自发消息)
    self_wxid: String,
    /// 当前账号的显示名 (从联系人库查询, 默认 "我")
    self_display_name: tokio::sync::RwLock<String>,
    /// 联系人缓存: username → ContactInfo
    contacts: Mutex<HashMap<String, ContactInfo>>,
    /// 高水位线: "db_name::表名" → 最大 local_id (多数据库区分)
    watermarks: Mutex<HashMap<String, i64>>,
    /// 持久化 message_N.db 连接池 (避免每次查询重做 PBKDF2 ~500ms)
    /// key = 相对路径 (如 "message/message_0.db")
    msg_conns: std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<Connection>>>>,
    msg_file_ids: std::sync::Mutex<HashMap<String, DatabaseFileIdentity>>,
    /// 持久化 contact.db 连接 (避免每次重做 PBKDF2)
    contact_conn: Arc<std::sync::Mutex<Option<Connection>>>,
    /// 持久化 session.db 连接
    session_conn: Arc<std::sync::Mutex<Option<Connection>>>,
    /// 每个会话最后派发的 local_id，用于发现 session.db 的即时更新
    session_cursors: Mutex<HashMap<String, SessionCursor>>,
    /// 启动时刻，避免把历史会话快照误当成新消息
    session_watch_started_at: i64,
    /// 消息表结构元数据缓存: "db_name::table_name" → TableMeta
    /// 表的列结构在运行期间不变, 但微信可能动态创建新表
    table_meta_cache: std::sync::Mutex<HashMap<String, TableMeta>>,
    /// WAL 变化广播通知 (多消费者: 消息循环 + verify_sent 等)
    wal_notify: tokio::sync::broadcast::Sender<()>,
    /// 自发消息内容广播 (get_new_messages 检测到自发消息时发出)
    sent_content_tx: tokio::sync::broadcast::Sender<DbMessage>,
    received_message_tx: tokio::sync::broadcast::Sender<DbMessage>,
    claimed_receipts: std::sync::Mutex<VecDeque<(String, String)>>,
    receipt_refresh: std::sync::Mutex<Option<std::time::Instant>>,
}

impl DbManager {
    pub(crate) async fn local_image_metadata(
        &self,
        chat: &str,
        server_id: i64,
    ) -> Result<Option<LocalImageMetadata>> {
        if chat.is_empty() || server_id <= 0 {
            return Ok(None);
        }
        let connections = self
            .ensure_msg_conns(false)?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let account = self
            .db_dir
            .parent()
            .context("缺少微信账号目录")?
            .to_path_buf();
        let wxid = self.self_wxid.clone();
        let chat = chat.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<LocalImageMetadata>> {
            let table = format!("Msg_{:x}", md5::compute(&chat));
            let query = format!(
                "SELECT create_time, packed_info_data, message_content FROM [{table}]
                 WHERE server_id=?1 AND (local_type & 65535)=3 LIMIT 1"
            );
            let legacy_query = format!(
                "SELECT create_time, packed_info_data, NULL FROM [{table}]
                 WHERE server_id=?1 AND (local_type & 65535)=3 LIMIT 1"
            );
            for connection in connections {
                let connection = connection
                    .lock()
                    .map_err(|_| anyhow::anyhow!("图片元数据连接不可用"))?;
                let Ok(mut statement) = connection
                    .prepare(&query)
                    .or_else(|_| connection.prepare(&legacy_query))
                else {
                    continue;
                };
                let mut rows = statement.query([server_id])?;
                let Some(row) = rows.next()? else { continue };
                let packed = row.get::<_, Vec<u8>>(1).unwrap_or_default();
                let mut hashes = Vec::new();
                for chunk in packed.windows(32) {
                    if chunk.iter().all(u8::is_ascii_hexdigit) {
                        let hash = String::from_utf8_lossy(chunk).to_ascii_lowercase();
                        if !hashes.contains(&hash) {
                            hashes.push(hash);
                        }
                        if hashes.len() >= 32 {
                            break;
                        }
                    }
                }
                return Ok(Some(LocalImageMetadata {
                    account,
                    wxid,
                    chat,
                    create_time: row.get(0)?,
                    hashes,
                    content: wcdb_get_text(row, 2),
                }));
            }
            Ok(None)
        })
        .await?
    }

    /// 创建 DbManager
    pub fn new(key_hex: String, db_dir: PathBuf) -> Result<Self> {
        let key_bytes = hex_to_bytes(&key_hex).context("密钥 hex 格式错误")?;
        anyhow::ensure!(
            key_bytes.len() == 32,
            "密钥长度必须为 32 字节, 实际: {}",
            key_bytes.len()
        );

        info!("📦 DbManager 初始化: db_dir={}", db_dir.display());
        let snapshots = Arc::new(snapshot::SnapshotReader::new(
            key_bytes.clone(),
            db_dir.clone(),
        )?);

        // 从 db_dir 路径提取自己的 wxid
        // 路径格式: .../wxid_xxx_c024/db_storage
        let self_wxid = db_dir
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .find(|s| s.starts_with("wxid_"))
            .map(|directory| account_wxid(directory).to_string())
            .unwrap_or_default();
        if !self_wxid.is_empty() {
            info!("👤 当前账号: {}", self_wxid);
        }

        // 自动发现并连接所有 message_N.db
        let mut conns = HashMap::new();
        let msg_dir = db_dir.join("message");
        if msg_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&msg_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if is_message_db(&name) {
                        let rel_path = format!("message/{}", name);
                        match snapshots.open(&rel_path) {
                            Ok(conn) => {
                                info!("🔗 {} 持久连接已建立", name);
                                conns.insert(rel_path, Arc::new(std::sync::Mutex::new(conn)));
                            }
                            Err(e) => {
                                info!("⚠️ {} 暂不可用 (将在查询时重试): {}", name, e);
                            }
                        }
                    }
                }
            }
        }
        if conns.is_empty() {
            warn!("⚠️ 未发现可用的 message 数据库 (将在首次查询时重试)");
        } else {
            info!("📂 已连接 {} 个消息数据库", conns.len());
        }

        let (wal_tx, _) = tokio::sync::broadcast::channel::<()>(64);
        let (sent_tx, _) = tokio::sync::broadcast::channel::<DbMessage>(128);
        let (received_message_tx, _) = tokio::sync::broadcast::channel::<DbMessage>(128);
        let session_watch_started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let msg_file_ids = conns
            .keys()
            .map(|name| (name.clone(), message_database_identity(&db_dir, name)))
            .collect();
        Ok(Self {
            key_bytes,
            db_dir,
            snapshots,
            self_wxid,
            self_display_name: tokio::sync::RwLock::new("我".to_string()),
            contacts: Mutex::new(HashMap::new()),
            watermarks: Mutex::new(HashMap::new()),
            msg_conns: std::sync::Mutex::new(conns),
            msg_file_ids: std::sync::Mutex::new(msg_file_ids),
            contact_conn: Arc::new(std::sync::Mutex::new(None)),
            session_conn: Arc::new(std::sync::Mutex::new(None)),
            session_cursors: Mutex::new(HashMap::new()),
            session_watch_started_at,
            table_meta_cache: std::sync::Mutex::new(HashMap::new()),
            wal_notify: wal_tx,
            sent_content_tx: sent_tx,
            received_message_tx,
            claimed_receipts: std::sync::Mutex::new(VecDeque::new()),
            receipt_refresh: std::sync::Mutex::new(None),
        })
    }

    /// 历史查询也扫描启动后出现的消息分片。
    fn ensure_msg_conns(
        &self,
        discover_all: bool,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, Arc<std::sync::Mutex<Connection>>>>> {
        let mut guard = self
            .msg_conns
            .lock()
            .map_err(|e| anyhow::anyhow!("msg_conns lock poisoned: {}", e))?;
        let mut identities = self
            .msg_file_ids
            .lock()
            .map_err(|e| anyhow::anyhow!("msg_file_ids lock poisoned: {}", e))?;
        if guard.is_empty() || discover_all {
            let entries = std::fs::read_dir(self.db_dir.join("message"))?;
            for entry in entries {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if !is_message_db(&name) {
                    continue;
                }
                let path = format!("message/{name}");
                if guard.contains_key(&path) {
                    continue;
                }
                let identity = message_database_identity(&self.db_dir, &path);
                match self.snapshots.open(&path) {
                    Ok(conn) => {
                        info!("🔗 {} 持久连接已建立", name);
                        identities.insert(path.clone(), identity);
                        guard.insert(path, Arc::new(std::sync::Mutex::new(conn)));
                    }
                    Err(error) if discover_all => {
                        return Err(error).with_context(|| format!("历史消息分片不可用: {path}"));
                    }
                    Err(_) => continue,
                }
            }
            if discover_all {
                guard.retain(|name, _| self.db_dir.join(name).is_file());
                identities.retain(|name, _| guard.contains_key(name));
            }
        }
        anyhow::ensure!(!guard.is_empty(), "无可用的 message 数据库");
        for (name, connection) in guard.iter() {
            let current = message_database_identity(&self.db_dir, name);
            let previous = identities.entry(name.clone()).or_insert(current);
            let files_rotated = previous.files != current.files;
            let database_changed = previous.revisions != current.revisions;
            if files_rotated || database_changed {
                let mut connection = connection
                    .lock()
                    .map_err(|e| anyhow::anyhow!("conn lock: {}", e))?;
                *connection = self.snapshots.open(name)?;
                if files_rotated {
                    info!("🔗 {} 文件已轮换，数据库连接已恢复", name);
                } else {
                    debug!("🔗 {} 数据库内容已更新，读取快照已刷新", name);
                }
                *previous = current;
            }
        }
        Ok(guard)
    }

    // =================================================================
    // 联系人
    // =================================================================

    /// 加载/刷新联系人缓存 (spawn_blocking 中执行 DB 查询)
    pub async fn refresh_contacts(&self) -> Result<usize> {
        let snapshots = Arc::clone(&self.snapshots);
        let conn_mutex = Arc::clone(&self.contact_conn);

        let contacts = tokio::task::spawn_blocking(move || -> Result<Vec<ContactInfo>> {
            let mut guard = conn_mutex
                .lock()
                .map_err(|e| anyhow::anyhow!("contact_conn lock: {}", e))?;
            *guard = Some(snapshots.open("contact/contact.db")?);
            let conn = guard.as_ref().unwrap();
            let mut stmt =
                conn.prepare("SELECT username, nick_name, remark, alias FROM contact")?;
            // WCDB 压缩可能导致 TEXT 列实际存储为 BLOB (Zstd),
            // 必须用 BLOB 回退读取, 否则部分行 (包括 chatroom) 会被丢弃
            let result: Vec<ContactInfo> = stmt
                .query_map([], |row| {
                    let username = wcdb_get_text(row, 0);
                    if username.is_empty() {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    let nick_name = wcdb_get_text(row, 1);
                    let remark = wcdb_get_text(row, 2);
                    let alias = wcdb_get_text(row, 3);
                    let display_name = if !remark.is_empty() {
                        remark.clone()
                    } else if !nick_name.is_empty() {
                        nick_name.clone()
                    } else {
                        username.clone()
                    };
                    Ok(ContactInfo {
                        username,
                        nick_name,
                        remark,
                        alias,
                        display_name,
                    })
                })?
                .filter_map(|r| match r {
                    Ok(c) => Some(c),
                    Err(e) => {
                        warn!("⚠️ 联系人行读取失败: {}", e);
                        None
                    }
                })
                .collect();
            Ok(result)
        })
        .await??;

        let count = contacts.len();
        // 短暂持锁: 清空并填入联系人
        {
            let mut cache = self.contacts.lock().await;
            cache.clear();
            for c in contacts {
                cache.insert(c.username.clone(), c);
            }
        } // 锁在此释放, 不阻塞 get_new_messages 等热路径
        info!("👥 联系人缓存: {} 条", count);

        // 从 chat_room 表补充群名 (锁已释放, spawn_blocking 不会阻塞读操作)
        let chatrooms = {
            let conn_mutex2 = Arc::clone(&self.contact_conn);
            tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
                let guard = conn_mutex2
                    .lock()
                    .map_err(|e| anyhow::anyhow!("contact_conn lock: {}", e))?;
                if let Some(conn) = guard.as_ref() {
                    let mut result = Vec::new();
                    if let Ok(mut stmt) = conn.prepare(
                        "SELECT cr.username, c.nick_name FROM chat_room cr \
                         LEFT JOIN contact c ON cr.username = c.username \
                         WHERE cr.username IS NOT NULL",
                    ) {
                        let rows: Vec<(String, String)> = stmt
                            .query_map([], |row| {
                                let id = wcdb_get_text(row, 0);
                                let name = wcdb_get_text(row, 1);
                                Ok((id, name))
                            })
                            .ok()
                            .map(|iter| iter.filter_map(|r| r.ok()).collect())
                            .unwrap_or_default();

                        for (id, name) in rows {
                            if !id.is_empty() && !name.is_empty() {
                                info!("👥 chat_room 补充: {} → {}", id, name);
                                result.push((id, name));
                            }
                        }
                    }
                    Ok(result)
                } else {
                    Ok(vec![])
                }
            })
            .await
            .unwrap_or_else(|_| Ok(vec![]))
            .unwrap_or_default()
        };

        // 短暂持锁: 补充群名
        if !chatrooms.is_empty() {
            let mut cache = self.contacts.lock().await;
            let mut added = 0usize;
            for (chatroom_id, nick_name) in chatrooms {
                if !cache.contains_key(&chatroom_id) {
                    cache.insert(
                        chatroom_id.clone(),
                        ContactInfo {
                            username: chatroom_id,
                            nick_name: nick_name.clone(),
                            remark: String::new(),
                            alias: String::new(),
                            display_name: nick_name,
                        },
                    );
                    added += 1;
                }
            }
            if added > 0 {
                info!("👥 群聊名称补充: {} 条", added);
            }
        }

        // 尝试解析当前账号的显示名 (短暂持锁读取, 然后释放)
        if !self.self_wxid.is_empty() {
            let name = self
                .contacts
                .lock()
                .await
                .get(&self.self_wxid)
                .map(|c| c.display_name.clone());
            if let Some(name) = name {
                info!("👤 当前账号昵称: {} ({})", name, self.self_wxid);
                *self.self_display_name.write().await = name;
            }
        }

        Ok(count)
    }

    /// 获取联系人列表
    pub async fn get_contacts(&self) -> Vec<ContactInfo> {
        self.contacts.lock().await.values().cloned().collect()
    }

    /// 当前登录账号的 wxid
    pub fn self_wxid(&self) -> &str {
        &self.self_wxid
    }

    pub async fn emoji_media(&self, checksum: &str) -> Result<Option<HashMap<String, String>>> {
        anyhow::ensure!(
            checksum.len() == 32 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "表情 MD5 无效"
        );
        let snapshots = Arc::clone(&self.snapshots);
        let directory = self.db_dir.clone();
        let checksum = checksum.to_string();
        tokio::task::spawn_blocking(move || {
            if !directory.join("emoticon/emoticon.db").is_file() {
                return Ok(None);
            }
            let connection = snapshots.open("emoticon/emoticon.db")?;
            emoji_media_from_connection(&connection, &checksum)
        })
        .await
        .context("表情媒体查询任务失败")?
    }

    /// 当前登录账号的显示名
    pub async fn self_display_name(&self) -> String {
        self.self_display_name.read().await.clone()
    }

    /// 通过 username 获取联系人/群聊显示名
    pub async fn display_name(&self, username: &str) -> String {
        self.resolve_name(username).await
    }

    /// 通过 username 获取完整联系人信息
    pub async fn contact_info(&self, username: &str) -> Option<ContactInfo> {
        self.contacts.lock().await.get(username).cloned()
    }

    /// 通过 username 获取显示名
    async fn resolve_name(&self, username: &str) -> String {
        self.contacts
            .lock()
            .await
            .get(username)
            .map(|c| c.display_name.clone())
            .unwrap_or_else(|| username.to_string())
    }

    // =================================================================
    // 会话
    // =================================================================

    /// 获取会话列表
    pub async fn get_sessions(&self) -> Result<Vec<DbSessionInfo>> {
        let snapshots = Arc::clone(&self.snapshots);
        let conn_mutex = Arc::clone(&self.session_conn);

        let rows = tokio::task::spawn_blocking(
            move || -> Result<Vec<(String, i32, String, i64, String)>> {
                let mut guard = conn_mutex
                    .lock()
                    .map_err(|e| anyhow::anyhow!("session_conn lock: {}", e))?;
                *guard = Some(snapshots.open("session/session.db")?);
                let conn = guard.as_ref().unwrap();
                let mut stmt = conn.prepare(
                    "SELECT username, unread_count, summary, last_timestamp, last_msg_sender \
                 FROM SessionTable ORDER BY sort_timestamp DESC",
                )?;
                let result = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<i32>>(1)?.unwrap_or(0),
                            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                            row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                            row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        ))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(result)
            },
        )
        .await??;

        // 异步填充显示名
        let mut sessions = Vec::with_capacity(rows.len());
        for (username, unread_count, summary, last_timestamp, last_msg_sender) in rows {
            let display_name = self.resolve_name(&username).await;
            sessions.push(DbSessionInfo {
                username,
                display_name,
                unread_count,
                summary,
                last_timestamp,
                last_msg_sender,
            });
        }
        Ok(sessions)
    }

    /// 轮询 session.db 中即时更新的最后一条消息。
    ///
    /// 微信会先把会话摘要、发送者和 `last_msg_locald_id` 写入 session.db 的 WAL，
    /// 而正文所在的 message_N.db 可能延迟数秒才可见。这里只返回真正变化且发生在
    /// 启动后的纯文本消息，正文到达后由 OneBot 按 local_id 补全并去重。
    pub async fn take_session_updates(&self) -> Result<Vec<SessionMessage>> {
        let snapshots = Arc::clone(&self.snapshots);
        let conn_mutex = Arc::clone(&self.session_conn);
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<SessionMessage>> {
            let mut guard = conn_mutex
                .lock()
                .map_err(|e| anyhow::anyhow!("session_conn lock: {}", e))?;
            if guard.is_none() {
                *guard = Some(snapshots.open("session/session.db")?);
            }
            let conn = guard.as_ref().unwrap();
            let mut stmt = conn.prepare(
                "SELECT st.username, st.summary, st.last_timestamp, \
                        st.last_msg_locald_id, COALESCE(u.server_id, 0), \
                        u.local_id IS NOT NULL, st.last_msg_type, st.last_msg_sub_type, \
                        st.last_msg_ext_type, st.last_msg_sender, st.last_sender_display_name \
                 FROM SessionTable st \
                 LEFT JOIN Name2Id n ON n.user_name = st.username \
                 LEFT JOIN SessionUnreadListTable_1 u \
                   ON u.username_id = n.rowid AND u.local_id = st.last_msg_locald_id \
                 WHERE st.last_msg_locald_id > 0",
            )?;
            let result = stmt
                .query_map([], |row| {
                    Ok(SessionMessage {
                        chat: row.get(0)?,
                        summary: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        timestamp: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        local_id: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        server_id: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                        unread: row.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
                        msg_type: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                        sub_type: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                        ext_type: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                        sender: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                        sender_display_name: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                    })
                })?
                .filter_map(|row| row.ok())
                .collect();
            Ok(result)
        })
        .await??;

        let mut cursors = self.session_cursors.lock().await;
        Ok(session_updates_from_rows(
            rows,
            &mut cursors,
            self.session_watch_started_at,
        ))
    }

    // =================================================================
    // 增量消息
    // =================================================================

    /// 获取新消息 (遍历所有 message_N.db 持久连接)
    pub async fn get_new_messages(&self) -> Result<Vec<DbMessage>> {
        let current_watermarks = self.watermarks.lock().await.clone();
        let self_wxid = self.self_wxid.clone();

        // 克隆 Arc 引用传入 spawn_blocking (安全, 无 unsafe)
        let conn_arcs: Vec<(String, Arc<std::sync::Mutex<Connection>>)> = {
            let conns_guard = self.ensure_msg_conns(false)?;
            conns_guard
                .iter()
                .map(|(name, conn)| (name.clone(), Arc::clone(conn)))
                .collect()
        };

        // 获取表结构缓存: key = "db_name::table_name" → TableMeta
        // 每次都查表列表 (1 条 SQL, 很快), 但只对新出现的表执行 PRAGMA
        let cached_meta: HashMap<String, TableMeta> = {
            self.table_meta_cache
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default()
        };

        let (raw_msgs, new_watermarks, updated_meta) = tokio::task::spawn_blocking(move || -> Result<(Vec<RawMsg>, HashMap<String, i64>, HashMap<String, TableMeta>)> {
            let mut all_msgs = Vec::new();
            let mut wm = current_watermarks;
            let mut name2id_cache: HashMap<String, String> = HashMap::new();
            let mut meta_cache = cached_meta;

            for (db_name, conn_arc) in &conn_arcs {
                let conn = conn_arc.lock().map_err(|e| anyhow::anyhow!("conn lock: {}", e))?;
                let db_prefix = db_name.trim_start_matches("message/").trim_end_matches(".db");

                // 每次都查表列表 (微信可能动态创建新表)
                let tables = discover_msg_tables(&conn);
                if tables.is_empty() { continue; }

                // 对每个表: 查缓存 → 有则复用, 无则 PRAGMA 构建
                let mut table_metas = Vec::new();
                for table in &tables {
                    let cache_key = format!("{}::{}", db_name, table);
                    if let Some(cached) = meta_cache.get(&cache_key) {
                        table_metas.push(cached.clone());
                    } else {
                        // 新表: PRAGMA 获取列结构
                        if let Some(meta) = build_single_table_meta(&conn, table) {
                            info!("📋 {} 新增表结构缓存: {}", db_name, table);
                            meta_cache.insert(cache_key, meta.clone());
                            table_metas.push(meta);
                        }
                    }
                }

                for meta in &table_metas {
                    let wm_key = format!("{}::{}", db_prefix, meta.table);
                    let last_id = wm.get(&wm_key).copied().unwrap_or(0);

                    let mut stmt = match conn.prepare(&meta.select_sql) {
                        Ok(s) => s,
                        Err(e) => { warn!("⚠️ 查询 {} ({}) 失败: {}", meta.table, db_name, e); continue; }
                    };
                    let mut chat = None;
                    let msgs: Vec<_> = match stmt
                        .query_map([last_id], |row| {
                            let chat = chat.get_or_insert_with(|| {
                                resolve_chat_from_table(&meta.table, &conn, &mut name2id_cache)
                            });
                            read_message_row(&conn, row, chat)
                        }) {
                        Ok(rows) => rows.filter_map(|r| match r {
                            Ok(v) => Some(v),
                            Err(e) => { warn!("⚠️ 行解析失败: {}", e); None }
                        }).collect(),
                        Err(e) => { warn!("⚠️ query_map {} ({}) 失败: {}", meta.table, db_name, e); continue; }
                    };

                    if !msgs.is_empty() {
                        let mut max_id = last_id;
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default().as_secs() as i64;
                        for message in msgs {
                            if media_content_pending(
                                message.msg_type,
                                &message.content,
                                message_is_self(&self_wxid, &message.talker, message.status, message.msg_type),
                            )
                                && message.create_time > now.saturating_sub(15)
                                && message.create_time <= now.saturating_add(5)
                            {
                                break;
                            }
                            max_id = max_id.max(message.local_id);
                            all_msgs.push(message);
                        }
                        wm.insert(wm_key.clone(), max_id);
                    }
                }
            }

            Ok((all_msgs, wm, meta_cache))
        }).await??;

        // 回写表结构缓存 (首次构建后不再变化)
        if let Ok(mut cache) = self.table_meta_cache.lock() {
            for (k, v) in updated_meta {
                cache.entry(k).or_insert(v);
            }
        }

        // 更新高水位线
        if !raw_msgs.is_empty() {
            *self.watermarks.lock().await = new_watermarks;
        }

        let result = self.describe_messages(raw_msgs).await;
        for message in &result {
            let base_type = message.msg_type & 0xffff;
            if base_type != 1 {
                debug!(
                    "🔍 msg_type={} (base={}) raw: {:.200}",
                    message.msg_type, base_type, message.content
                );
            }
            if message.is_self {
                let _ = self.sent_content_tx.send(message.clone());
            }
            let _ = self.received_message_tx.send(message.clone());
        }
        for message in &result {
            let preview = message.parsed.preview(40);
            let icon = if message.is_self { "📤 →" } else { "📨" };
            if message.chat.contains("@chatroom") {
                info!(
                    "{icon} [{}] {}({}): {}",
                    message.chat_display_name, message.talker_display_name, message.talker, preview
                );
            } else {
                info!(
                    "{icon} {}({}): {}",
                    message.chat_display_name, message.talker, preview
                );
            }
        }
        Ok(result)
    }

    async fn describe_messages(&self, raw_msgs: Vec<RawMsg>) -> Vec<DbMessage> {
        // 异步填充显示名 (批量: 一次锁定联系人缓存, 避免 N×2 次锁竞争)
        let contacts_cache = self.contacts.lock().await;
        let self_display = self.self_display_name.read().await.clone();
        let resolve = |username: &str| -> String {
            contacts_cache
                .get(username)
                .map(|c| c.display_name.clone())
                .unwrap_or_else(|| username.to_string())
        };

        let mut result = Vec::with_capacity(raw_msgs.len());
        for m in raw_msgs {
            let (mut talker, content) = normalize_group_message(&m.chat, m.talker, m.content);

            let is_self = message_is_self(&self.self_wxid, &talker, m.status, m.msg_type);

            // talker 为空时填充: 自发用 self_wxid, 私聊收到用 chat(对方)
            if talker.is_empty() {
                if is_self {
                    talker = self.self_wxid.clone();
                } else if !m.chat.contains("@chatroom") {
                    talker = m.chat.clone();
                }
            }

            let talker_display = if is_self {
                self_display.clone()
            } else {
                resolve(&talker)
            };
            let chat_display = resolve(&m.chat);
            let talker_contact = if is_self {
                None
            } else {
                contacts_cache.get(&talker)
            };
            let chat_contact = contacts_cache.get(&m.chat);
            let talker_nick_name = if is_self {
                self_display.clone()
            } else {
                talker_contact
                    .map(|contact| contact.nick_name.clone())
                    .unwrap_or_else(|| talker_display.clone())
            };
            let talker_remark = talker_contact
                .map(|contact| contact.remark.clone())
                .unwrap_or_default();
            let chat_nick_name = chat_contact
                .map(|contact| contact.nick_name.clone())
                .unwrap_or_else(|| chat_display.clone());
            let chat_remark = chat_contact
                .map(|contact| contact.remark.clone())
                .unwrap_or_default();
            let parsed = parse_msg_content(m.msg_type, &content);
            result.push(DbMessage {
                local_id: m.local_id,
                server_id: m.server_id,
                create_time: m.create_time,
                content,
                source: m.source,
                parsed,
                msg_type: m.msg_type,
                talker,
                talker_display_name: talker_display,
                talker_nick_name,
                talker_remark,
                chat: m.chat,
                chat_display_name: chat_display,
                chat_nick_name,
                chat_remark,
                is_self,
            });
        }
        result
    }

    /// 标记所有已有消息为已读 (复用持久连接 + 复用表元数据构建)
    pub async fn mark_all_read(&self) -> Result<()> {
        // 克隆 Arc 引用传入 spawn_blocking
        let conn_arcs: Vec<(String, Arc<std::sync::Mutex<Connection>>)> = {
            let conns_guard = self.ensure_msg_conns(false)?;
            conns_guard
                .iter()
                .map(|(name, conn)| (name.clone(), Arc::clone(conn)))
                .collect()
        };

        let wm = tokio::task::spawn_blocking(move || -> Result<HashMap<String, i64>> {
            let mut watermarks = HashMap::new();
            let mut total_tables = 0;

            for (db_name, conn_arc) in &conn_arcs {
                let conn = conn_arc
                    .lock()
                    .map_err(|e| anyhow::anyhow!("conn lock: {}", e))?;
                let db_prefix = db_name
                    .trim_start_matches("message/")
                    .trim_end_matches(".db");

                // 复用 discover_msg_tables + build_single_table_meta (消除重复 PRAGMA)
                let tables = discover_msg_tables(&conn);
                for table in &tables {
                    if let Some(meta) = build_single_table_meta(&conn, table) {
                        let wm_key = format!("{}::{}", db_prefix, table);
                        let sql = format!("SELECT MAX({}) FROM [{}]", meta.id_col, table);
                        if let Ok(max_id) =
                            conn.query_row(&sql, [], |row| row.get::<_, Option<i64>>(0))
                        {
                            if let Some(id) = max_id {
                                watermarks.insert(wm_key, id);
                            }
                        }
                    }
                }
                total_tables += tables.len();
            }
            info!(
                "✅ 已标记 {} 个消息表为已读 (跨 {} 个数据库)",
                total_tables,
                conn_arcs.len()
            );
            Ok(watermarks)
        })
        .await??;

        *self.watermarks.lock().await = wm;
        Ok(())
    }

    // =================================================================
    // 发送验证 (DB 版)
    // =================================================================

    /// 通过数据库验证消息是否发送成功 (事件驱动)
    ///
    /// 订阅 get_new_messages 的自发消息广播, 等待内容匹配.
    /// 无需单独查询 DB, 完全复用现有的消息检测流程.
    /// 调用方应在发送前调用 subscribe_sent() 获取 receiver, 避免竞态.
    /// 超时 20 秒兜底.
    pub async fn verify_sent(
        &self,
        chat: &str,
        text: &str,
        quote_id: Option<&str>,
        mention_ids: &[String],
        mut sent_rx: tokio::sync::broadcast::Receiver<DbMessage>,
    ) -> Result<bool> {
        let text_owned = text.to_string();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut refresh = tokio::time::interval(std::time::Duration::from_millis(500));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                result = sent_rx.recv() => {
                    match result {
                        Ok(message) => {
                            let matches = if let Some(quote_id) = quote_id {
                                message.is_self && message.chat == chat
                                    && (message.msg_type & 0xFFFF) == 49
                                    && crate::onebot::native_reply_matches(&message.content, quote_id, &text_owned)
                            } else {
                                sent_message_matches(chat, &text_owned, &message.chat, &message.content)
                            };
                            if matches
                                && (mention_ids.is_empty()
                                    || (message.is_self
                                        && crate::onebot::native_mentions_match(&message, mention_ids)))
                                && self.claim_receipt(&message)
                            {
                                info!("✅ [DB] 发送验证成功: \"{}\"", text_owned);
                                return Ok(true);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            warn!("⚠️ [DB] 自发消息广播通道已关闭");
                            break;
                        }
                    }
                }
                _ = refresh.tick() => {
                    self.refresh_receipts();
                }
                _ = tokio::time::sleep_until(deadline) => {
                    warn!("⚠️ [DB] 发送验证超时 (20s): \"{}\"", text_owned);
                    break;
                }
            }
        }
        Ok(false)
    }

    /// 通过数据库确认自发图片已进入目标会话。
    ///
    /// 图片没有可比较的文本内容，必须检查会话、发送方、时间窗和消息类型，
    /// 不能把输入框粘贴或 Enter 成功当成已经送达。
    pub async fn verify_image_sent(
        &self,
        chat: &str,
        started_at: i64,
        messages: tokio::sync::broadcast::Receiver<DbMessage>,
    ) -> Result<bool> {
        self.verify_media_sent(chat, started_at, None, messages)
            .await
    }

    pub async fn verify_sticker_sent(
        &self,
        chat: &str,
        started_at: i64,
        checksum: &str,
        messages: tokio::sync::broadcast::Receiver<DbMessage>,
    ) -> Result<bool> {
        self.verify_media_sent(chat, started_at, Some(checksum), messages)
            .await
    }

    async fn verify_media_sent(
        &self,
        chat: &str,
        started_at: i64,
        checksum: Option<&str>,
        mut messages: tokio::sync::broadcast::Receiver<DbMessage>,
    ) -> Result<bool> {
        let label = if checksum.is_some() {
            "原生表情"
        } else {
            "图片"
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut refresh = tokio::time::interval(std::time::Duration::from_millis(500));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                result = messages.recv() => {
                    match result {
                        Ok(message) => {
                            let matches = match checksum {
                                Some(checksum) => sticker_receipt_matches(chat, started_at, checksum, &message),
                                None => image_receipt_matches(chat, started_at, &message),
                            };
                            if matches
                                && self.claim_receipt(&message)
                            {
                                info!("✅ [DB] {label}发送验证成功: {chat}");
                                return Ok(true);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            warn!("⚠️ [DB] 消息广播通道已关闭");
                            break;
                        }
                    }
                }
                _ = refresh.tick() => {
                    self.refresh_receipts();
                }
                _ = tokio::time::sleep_until(deadline) => {
                    warn!("⚠️ [DB] {label}发送验证超时 (20s): {chat}");
                    break;
                }
            }
        }
        Ok(false)
    }

    fn claim_receipt(&self, message: &DbMessage) -> bool {
        let id = if message.server_id == 0 {
            format!("local_{}", message.local_id)
        } else {
            format!("server_{}", message.server_id)
        };
        let key = (message.chat.clone(), id);
        let Ok(mut claimed) = self.claimed_receipts.lock() else {
            return false;
        };
        if claimed.contains(&key) {
            return false;
        }
        claimed.push_back(key);
        while claimed.len() > 4096 {
            claimed.pop_front();
        }
        true
    }

    fn refresh_receipts(&self) {
        let Ok(mut last_refresh) = self.receipt_refresh.lock() else {
            return;
        };
        if last_refresh.is_some_and(|last| last.elapsed() < std::time::Duration::from_millis(500)) {
            return;
        }
        *last_refresh = Some(std::time::Instant::now());
        let _ = self.wal_notify.send(());
    }

    /// 订阅自发消息广播 (在发送前调用, 确保不丢失发送期间的事件)
    pub fn subscribe_sent(&self) -> tokio::sync::broadcast::Receiver<DbMessage> {
        self.sent_content_tx.subscribe()
    }

    /// 订阅解析后的自发消息，用于图片等无法按文本确认的发送回执。
    pub fn subscribe_received(&self) -> tokio::sync::broadcast::Receiver<DbMessage> {
        self.received_message_tx.subscribe()
    }

    /// 订阅 WAL 变化通知
    pub fn subscribe_wal_events(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.wal_notify.subscribe()
    }

    // =================================================================
    // WAL inotify 监听
    // =================================================================

    /// 启动 WAL 文件监听 (inotify，在独立线程运行)
    ///
    /// 返回 broadcast::Receiver, 支持多消费者 (消息循环 + verify_sent 等)
    pub fn spawn_wal_watcher(self: &Arc<Self>) -> tokio::sync::broadcast::Receiver<()> {
        let wal_tx = self.wal_notify.clone();
        let db_dir = self.db_dir.clone();

        std::thread::spawn(move || loop {
            if let Err(e) = wal_watch_loop(&db_dir, &wal_tx) {
                error!("❌ WAL 监听中断，1 秒后重试: {}", e);
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        });

        info!("👁️ WAL 文件监听已启动 (inotify，独立只读快照，broadcast)");
        self.wal_notify.subscribe()
    }
}

// =====================================================================
// 同步辅助函数
// =====================================================================

fn session_updates_from_rows(
    rows: Vec<SessionMessage>,
    cursors: &mut HashMap<String, SessionCursor>,
    started_at: i64,
) -> Vec<SessionMessage> {
    let mut updates = Vec::new();
    for row in rows {
        let previous = cursors.insert(
            row.chat.clone(),
            SessionCursor {
                local_id: row.local_id,
                timestamp: row.timestamp,
            },
        );
        let changed = previous.is_none_or(|cursor| !cursor.matches(&row));
        if changed && row.timestamp >= started_at {
            updates.push(row);
        }
    }
    updates.sort_by_key(|message| (message.timestamp, message.local_id));
    updates
}

fn message_database_identity(db_dir: &Path, name: &str) -> DatabaseFileIdentity {
    let metadata = ["", "-wal", "-shm"]
        .map(|suffix| std::fs::metadata(db_dir.join(format!("{name}{suffix}"))).ok());
    DatabaseFileIdentity {
        files: metadata.each_ref().map(|metadata| {
            metadata
                .as_ref()
                .map(|metadata| (metadata.dev(), metadata.ino()))
        }),
        revisions: [&metadata[0], &metadata[1]].map(|metadata| {
            metadata
                .as_ref()
                .map(|metadata| (metadata.len(), metadata.mtime(), metadata.mtime_nsec()))
        }),
    }
}

/// 从消息表名解析会话 username
/// ChatMsg_<rowid> -> Name2Id.user_name WHERE rowid = <id>
/// Msg_<hash> -> MD5(Name2Id.user_name) == hash (使用缓存 O(1) 查找)
fn resolve_chat_from_table(
    table_name: &str,
    conn: &Connection,
    cache: &mut HashMap<String, String>,
) -> String {
    // 尝试 ChatMsg_<数字> 格式 -> 按 rowid 查找
    if let Some(suffix) = table_name.strip_prefix("ChatMsg_") {
        if let Ok(id) = suffix.parse::<i64>() {
            let sql = "SELECT user_name FROM Name2Id WHERE rowid = ?1";
            if let Ok(name) = conn.query_row(sql, [id], |row| row.get::<_, String>(0)) {
                debug!("✅ ChatMsg rowid={} -> {}", id, name);
                return name;
            }
        }
    }

    // 尝试 Msg_<hash> / MSG_<hash> / Chat_<hash> 格式
    if let Some(hash) = table_name
        .strip_prefix("Msg_")
        .or_else(|| table_name.strip_prefix("MSG_"))
        .or_else(|| table_name.strip_prefix("Chat_"))
    {
        // 懒加载: 首次查找时构建 MD5 hash → username 缓存
        if cache.is_empty() {
            if let Ok(mut stmt) = conn.prepare("SELECT user_name FROM Name2Id") {
                if let Ok(names) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                    for name in names.flatten() {
                        let name_hash = format!("{:x}", md5::compute(name.as_bytes()));
                        cache.insert(name_hash, name);
                    }
                }
            }
            debug!("📦 Name2Id 缓存已构建: {} 条", cache.len());
        }

        // O(1) 查找
        if let Some(name) = cache.get(hash) {
            debug!("✅ Msg hash={} -> user_name={}", hash, name);
            return name.clone();
        }
        debug!("⚠️ hash={} 未在 Name2Id 中找到匹配", hash);
    }

    debug!("⚠️ 无法解析会话名: {}", table_name);
    table_name.to_string()
}

// =====================================================================
// WAL 监听 (inotify，在 std::thread 中运行)
// =====================================================================

fn wal_watch_loop(db_dir: &Path, tx: &tokio::sync::broadcast::Sender<()>) -> Result<()> {
    let msg_dir = db_dir.join("message");

    // 等待 message 目录创建 (轮询, 仅启动时执行一次)
    if !msg_dir.exists() {
        info!("⏳ 等待 message 目录创建: {}", msg_dir.display());
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if msg_dir.exists() {
                info!("📁 message 目录已创建");
                break;
            }
        }
    }

    let watcher = watch::DatabaseWatcher::new(&msg_dir)?;
    info!("👁️ 开始监听消息目录: {} (inotify)", msg_dir.display());
    let _ = tx.send(());
    loop {
        if watcher.wait_for_changes()? {
            let _ = tx.send(());
        }
    }
}

// =====================================================================
// 消息内容解析
// =====================================================================

/// WCDB Zstd BLOB 解压: 检测 Zstd magic 0x28B52FFD, 解压后返回 UTF-8 字符串
fn decompress_wcdb_content(blob: &[u8]) -> String {
    // Zstd magic: 0xFD2FB528 (little-endian) = bytes [0x28, 0xB5, 0x2F, 0xFD]
    if blob.len() >= 4 && blob[0] == 0x28 && blob[1] == 0xB5 && blob[2] == 0x2F && blob[3] == 0xFD {
        match zstd::decode_all(blob) {
            Ok(data) => return String::from_utf8_lossy(&data).to_string(),
            Err(e) => warn!("⚠️ Zstd 解压失败: {}", e),
        }
    }
    // 非 Zstd: 直接 lossy UTF-8
    String::from_utf8_lossy(blob).to_string()
}

/// WCDB 兼容读取: 先尝试 TEXT, 失败则 BLOB + Zstd 解压
/// (WCDB 压缩可能导致 TEXT 列实际存储为 BLOB)
fn wcdb_get_text(row: &rusqlite::Row, idx: usize) -> String {
    match row.get::<_, Option<String>>(idx) {
        Ok(s) => s.unwrap_or_default(),
        Err(_) => match row.get::<_, Option<Vec<u8>>>(idx) {
            Ok(Some(bytes)) => decompress_wcdb_content(&bytes),
            _ => String::new(),
        },
    }
}

fn read_message_row(
    conn: &Connection,
    row: &rusqlite::Row<'_>,
    chat: &str,
) -> rusqlite::Result<RawMsg> {
    Ok(RawMsg {
        local_id: row.get(0)?,
        server_id: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
        create_time: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
        content: wcdb_get_text(row, 3),
        msg_type: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
        talker: message_sender(conn, row, 5)?,
        chat: chat.to_owned(),
        status: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
        source: wcdb_get_text(row, 7),
    })
}

/// 查询 sqlite_master 获取消息表列表 (每次调用, 发现新表)
fn discover_msg_tables(conn: &Connection) -> Vec<String> {
    match conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND \
         (name LIKE 'ChatMsg_%' OR name LIKE 'MSG_%' OR name LIKE 'Chat_%')",
    ) {
        Ok(mut stmt) => stmt
            .query_map([], |row| row.get(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 对单个消息表执行 PRAGMA table_info → 构建 TableMeta (仅新表调用一次)
fn build_single_table_meta(conn: &Connection, table: &str) -> Option<TableMeta> {
    let pragma_sql = format!("PRAGMA table_info({})", table);
    let mut pragma_stmt = conn.prepare(&pragma_sql).ok()?;
    let columns: Vec<String> = pragma_stmt
        .query_map([], |row| row.get::<_, String>(1))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    let id_col = columns
        .iter()
        .find(|c| {
            c.eq_ignore_ascii_case("local_id")
                || c.eq_ignore_ascii_case("localId")
                || c.eq_ignore_ascii_case("rowid")
        })
        .cloned()
        .unwrap_or_else(|| "rowid".to_string());

    let time_col = columns
        .iter()
        .find(|c| c.eq_ignore_ascii_case("create_time") || c.eq_ignore_ascii_case("createTime"))
        .cloned();

    let content_col = columns
        .iter()
        .find(|c| {
            c.eq_ignore_ascii_case("message_content")
                || c.eq_ignore_ascii_case("content")
                || c.eq_ignore_ascii_case("msgContent")
                || c.eq_ignore_ascii_case("compress_content")
        })
        .cloned();

    let type_col = columns
        .iter()
        .find(|c| {
            c.eq_ignore_ascii_case("local_type")
                || c.eq_ignore_ascii_case("type")
                || c.eq_ignore_ascii_case("msgType")
        })
        .cloned();

    let talker_col = columns
        .iter()
        .find(|c| {
            c.eq_ignore_ascii_case("real_sender_id")
                || c.eq_ignore_ascii_case("talker")
                || c.eq_ignore_ascii_case("talkerId")
        })
        .cloned();

    let svr_col = columns
        .iter()
        .find(|c| {
            c.eq_ignore_ascii_case("server_id")
                || c.eq_ignore_ascii_case("svrid")
                || c.eq_ignore_ascii_case("msgSvrId")
        })
        .cloned();

    let content_sel = content_col.as_deref()?;
    let time_sel = time_col.as_deref().unwrap_or("0");
    let type_sel = type_col.as_deref().unwrap_or("0");
    let talker_sel = talker_col.as_deref().unwrap_or("''");
    let svr_sel = svr_col.as_deref().unwrap_or("0");

    let status_col = columns
        .iter()
        .find(|c| c.eq_ignore_ascii_case("status"))
        .cloned();
    let status_sel = status_col.as_deref().unwrap_or("0");
    let source_sel = columns
        .iter()
        .find(|column| column.eq_ignore_ascii_case("source"))
        .map(String::as_str)
        .unwrap_or("''");

    let select_from_sql = format!(
        "SELECT {id}, {svr}, {time}, {content}, {typ}, {talker}, {status}, {source_sel} FROM [{tbl}]",
        id = id_col,
        svr = svr_sel,
        time = time_sel,
        content = content_sel,
        typ = type_sel,
        talker = talker_sel,
        status = status_sel,
        tbl = table,
    );

    Some(TableMeta {
        table: table.to_string(),
        select_sql: format!("{select_from_sql} WHERE {id_col} > ?1 ORDER BY {id_col} ASC"),
        select_from_sql,
        history_time: format!("coalesce({time_sel}, 0)"),
        time_col,
        server_id_col: svr_col,
        id_col,
    })
}

fn account_wxid(directory: &str) -> &str {
    let Some((username, suffix)) = directory.rsplit_once('_') else {
        return directory;
    };
    let is_hash = suffix.len() == 4 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit());
    let is_legacy = suffix.strip_prefix('c').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    if username.starts_with("wxid_") && (is_hash || is_legacy) {
        username
    } else {
        directory
    }
}

fn sent_message_matches(
    expected_chat: &str,
    expected_text: &str,
    actual_chat: &str,
    actual_text: &str,
) -> bool {
    expected_chat == actual_chat
        && !expected_text.trim().is_empty()
        && expected_text.trim() == actual_text.trim()
}

fn message_sender(
    conn: &Connection,
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<String> {
    match row.get_ref(index)? {
        rusqlite::types::ValueRef::Integer(sender_id) => Ok(conn
            .query_row(
                "SELECT user_name FROM Name2Id WHERE rowid = ?1",
                [sender_id],
                |sender| sender.get::<_, String>(0),
            )
            .unwrap_or_default()),
        _ => Ok(wcdb_get_text(row, index)),
    }
}

fn normalize_group_message(chat: &str, sender: String, content: String) -> (String, String) {
    if chat.ends_with("@chatroom") {
        if let Some((header, body)) = content.split_once('\n') {
            let header = header.strip_suffix('\r').unwrap_or(header);
            if let Some(prefix) = header.strip_suffix(':') {
                let valid_prefix = !prefix.is_empty()
                    && prefix.len() < 64
                    && prefix
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
                if valid_prefix && (sender.is_empty() || sender == prefix) {
                    return (prefix.to_string(), body.to_string());
                }
            }
        }
    }
    (sender, content)
}

fn message_is_self(self_id: &str, sender: &str, status: i64, msg_type: i64) -> bool {
    if matches!(msg_type & 0xFFFF, 10000 | 10002) {
        return false;
    }
    if !sender.is_empty() {
        return sender == self_id;
    }
    (status & 0x02) == 0
}

fn emoji_media_from_connection(
    connection: &Connection,
    checksum: &str,
) -> Result<Option<HashMap<String, String>>> {
    let fields = [
        "aeskey",
        "cdnurl",
        "externurl",
        "externmd5",
        "encrypturl",
        "thumburl",
    ];
    Ok(connection
        .query_row(
            "SELECT aes_key, cdn_url, extern_url, extern_md5, encrypt_url, thumb_url \
         FROM kNonStoreEmoticonTable WHERE md5 = ?1 COLLATE NOCASE LIMIT 1",
            [checksum],
            |row| {
                let mut attributes = HashMap::from([("md5".to_string(), checksum.to_string())]);
                for (index, field) in fields.iter().enumerate() {
                    let value = wcdb_get_text(row, index);
                    if !value.is_empty() {
                        attributes.insert((*field).to_string(), value);
                    }
                }
                Ok(attributes)
            },
        )
        .optional()?)
}

fn image_receipt_matches(expected_chat: &str, started_at: i64, message: &DbMessage) -> bool {
    expected_chat == message.chat
        && message.is_self
        && message.create_time >= started_at - 2
        && matches!(
            message.parsed,
            MsgContent::Image { .. } | MsgContent::Emoji { .. }
        )
}

fn sticker_receipt_matches(
    expected_chat: &str,
    started_at: i64,
    checksum: &str,
    message: &DbMessage,
) -> bool {
    image_receipt_matches(expected_chat, started_at, message)
        && (message.msg_type & 0xFFFF) == 47
        && matches!(message.parsed, MsgContent::Emoji { .. })
        && extract_xml_attr(&message.content, "emoji", "md5")
            .is_some_and(|actual| actual.eq_ignore_ascii_case(checksum))
}

fn media_content_pending(msg_type: i64, content: &str, is_self: bool) -> bool {
    match msg_type & 0xFFFF {
        3 | 47 if content.trim().is_empty() => true,
        47 if is_self => {
            extract_xml_attr(content, "emoji", "md5").is_none_or(|checksum| checksum.is_empty())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;

/// 根据 msg_type 解析原始 content 为结构化 MsgContent
/// content 已经过 Zstd 解压 (如果需要), 应为 XML 或纯文本
pub(crate) fn parse_msg_content(msg_type: i64, content: &str) -> MsgContent {
    // 微信 msg_type 高位是标志位 (如 0x600000021), 实际类型在低 16 位
    let base_type = (msg_type & 0xFFFF) as i32;
    match base_type {
        1 => MsgContent::Text {
            text: content.to_string(),
        },
        3 => parse_image(content),
        34 => parse_voice(content),
        42 => parse_contact_card(content),
        43 => parse_video(content),
        47 => parse_emoji(content),
        49 => parse_app(content),
        10000 | 10002 => MsgContent::System {
            text: content.to_string(),
        },
        _ => MsgContent::Unknown {
            raw: content.to_string(),
            msg_type,
        },
    }
}

/// 图片消息: 从 XML 中提取 CDN URL
fn parse_image(content: &str) -> MsgContent {
    let path = extract_xml_attr(content, "img", "cdnmidimgurl")
        .or_else(|| extract_xml_attr(content, "img", "cdnbigimgurl"));
    MsgContent::Image { path }
}

/// 语音消息: 尝试多种属性名提取时长
fn parse_voice(content: &str) -> MsgContent {
    let duration_ms = extract_xml_attr(content, "voicemsg", "voicelength")
        .or_else(|| extract_xml_attr(content, "voicemsg", "voicelen"))
        .or_else(|| extract_xml_attr(content, "voicemsg", "length"))
        .and_then(|v| v.parse::<u32>().ok());
    MsgContent::Voice { duration_ms }
}

/// 名片消息 (msg_type=42): 提取昵称和 wxid
fn parse_contact_card(content: &str) -> MsgContent {
    let nickname = extract_xml_attr(content, "msg", "nickname")
        .or_else(|| extract_xml_attr(content, "msg", "smallheadimgurl"));
    let username = extract_xml_attr(content, "msg", "username");
    let title = nickname.or(username);
    MsgContent::App {
        title,
        desc: Some("名片".to_string()),
        url: None,
        app_type: Some(42),
    }
}

/// 视频消息: 提取 cdnthumburl
fn parse_video(content: &str) -> MsgContent {
    let thumb_path = extract_xml_attr(content, "videomsg", "cdnthumburl");
    MsgContent::Video { thumb_path }
}

/// 表情消息: 提取 cdnurl
fn parse_emoji(content: &str) -> MsgContent {
    let url = extract_xml_attr(content, "emoji", "cdnurl");
    MsgContent::Emoji { url }
}

/// 链接/文件/小程序消息 (msg_type=49): 解析 appmsg XML
/// app_type 子类型: 3=音乐, 4=链接, 5=链接, 6=文件, 19=转发, 33/36=小程序, 2000=转账, 2001=红包
fn parse_app(content: &str) -> MsgContent {
    let title = extract_xml_text(content, "title");
    let desc = extract_xml_text(content, "des");
    let url = extract_xml_text(content, "url");
    let app_type = extract_xml_text(content, "type").and_then(|t| t.parse::<i32>().ok());
    MsgContent::App {
        title,
        desc,
        url,
        app_type,
    }
}

/// 从 XML 中提取指定元素的属性值 (如 <img cdnmidimgurl="..."/>)
fn extract_xml_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    for a in e.attributes().flatten() {
                        if a.key.as_ref() == attr.as_bytes() {
                            return a
                                .decode_and_unescape_value(reader.decoder())
                                .ok()
                                .map(|value| value.into_owned());
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// 从 XML 中提取指定元素的文本内容 (如 <title>标题</title>)
fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut in_tag = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    in_tag = true;
                }
            }
            Ok(Event::Text(ref e)) if in_tag => {
                return e.unescape().ok().map(|s| s.to_string());
            }
            Ok(Event::CData(ref e)) if in_tag => {
                return String::from_utf8(e.to_vec()).ok();
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == tag.as_bytes() {
                    in_tag = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

// =====================================================================
// 工具函数
// =====================================================================

/// 判断文件名是否为 message_N.db 格式 (N 是数字)
/// 排除 message_fts.db, message_resource.db 等辅助数据库
fn is_message_db(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("message_") {
        if let Some(num_part) = rest.strip_suffix(".db") {
            return !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(hex.len() % 2 == 0, "hex 长度必须为偶数");
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .with_context(|| format!("无效 hex 字符: {}", &hex[i..i + 2]))
        })
        .collect()
}
