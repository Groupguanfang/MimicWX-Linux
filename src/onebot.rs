mod forward;
mod history;
mod inbound_media;
mod media;
mod mention;
mod reverse;

pub use reverse::ReverseConfig;

use axum::{
    extract::{
        ws::{rejection::WebSocketUpgradeRejection, Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use quick_xml::{events::Event, Reader};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, Mutex};
use tokio::task::{JoinError, JoinSet};
use tokio_tungstenite::tungstenite::Message as ClientMessage;
use tracing::{debug, warn};

use crate::{
    api::{self, AppState},
    db::{ContactInfo, DbManager, DbMessage, MsgContent},
    wechat::{NativeQuoteTarget, TextPart},
};

pub const WS_PATH: &str = "/onebot/ws";
const CACHE_LIMIT: usize = 4096;
const FORWARD_CACHE_LIMIT: usize = 256;
const ACTION_LIMIT: usize = 64;
static SEND_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct MessageCache {
    values: HashMap<String, Value>,
    order: VecDeque<String>,
}

impl MessageCache {
    fn finish_fast(&mut self, id: String, value: Value) -> bool {
        let Some(mut fast) = self.values.get(&id).cloned() else {
            return false;
        };
        let canonical = value["message_id"].as_str().unwrap_or_default().to_string();
        fast["message_id"] = json!(canonical);
        fast["message"] = value["message"].clone();
        fast["raw_message"] = value["raw_message"].clone();
        self.insert(id, value);
        self.insert(canonical, fast);
        true
    }

    fn insert(&mut self, id: String, value: Value) -> bool {
        self.insert_bounded(id, value, CACHE_LIMIT)
    }

    fn insert_bounded(&mut self, id: String, value: Value, limit: usize) -> bool {
        let fresh = !self.values.contains_key(&id);
        if fresh {
            self.order.push_back(id.clone());
        }
        self.values.insert(id, value);
        while self.order.len() > limit {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
        fresh
    }

    fn history(&self, chat: &str, before: Option<i64>, limit: usize) -> Vec<Value> {
        let mut sequences = HashSet::new();
        let mut messages = Vec::new();
        for message in self.values.values() {
            let Some(sequence) = message.get("message_seq").and_then(Value::as_i64) else {
                continue;
            };
            if message.get("chat_id").and_then(Value::as_str) != Some(chat)
                || before.is_some_and(|cursor| sequence >= cursor)
                || (sequence > 0 && !sequences.insert(sequence))
            {
                continue;
            }
            messages.push((sequence, message));
        }
        messages.sort_by_key(|(sequence, _)| *sequence);
        messages
            .into_iter()
            .rev()
            .take(limit)
            .map(|(_, message)| message.clone())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

#[derive(Default)]
struct LocalMessageSet {
    values: HashSet<(String, i64)>,
    order: VecDeque<(String, i64)>,
}

impl LocalMessageSet {
    fn contains(&self, key: &(String, i64)) -> bool {
        self.values.contains(key)
    }

    fn insert(&mut self, key: (String, i64)) {
        if self.values.insert(key.clone()) {
            self.order.push_back(key);
        }
        while self.order.len() > CACHE_LIMIT {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
    }
}

#[derive(Default)]
struct FastDispatch {
    database_messages: LocalMessageSet,
    fast_messages: HashMap<(String, i64), String>,
    fast_order: VecDeque<(String, i64)>,
}

impl FastDispatch {
    fn claim_database(&mut self, key: (String, i64)) -> Option<String> {
        self.database_messages.insert(key.clone());
        self.fast_messages.remove(&key)
    }

    fn claim_fast(&mut self, key: (String, i64), message_id: String) -> bool {
        if self.database_messages.contains(&key) || self.fast_messages.contains_key(&key) {
            return false;
        }
        self.database_messages.insert(key.clone());
        self.fast_messages.insert(key.clone(), message_id);
        self.fast_order.push_back(key);
        while self.fast_order.len() > CACHE_LIMIT {
            if let Some(oldest) = self.fast_order.pop_front() {
                self.fast_messages.remove(&oldest);
            }
        }
        true
    }
}

pub struct OneBotRuntime {
    pub token: Option<String>,
    pub self_id: String,
    pub self_name: String,
    pub events: broadcast::Sender<String>,
    messages: Mutex<MessageCache>,
    history_messages: Mutex<MessageCache>,
    forward_records: Mutex<MessageCache>,
    seen: Mutex<MessageCache>,
    fast_dispatch: Mutex<FastDispatch>,
    send_lock: Mutex<()>,
    media: Option<inbound_media::Store>,
    database: Option<Arc<DbManager>>,
}

impl OneBotRuntime {
    pub fn new(self_id: String, self_name: String, token: Option<String>) -> Arc<Self> {
        Self::create(self_id, self_name, token, None, None)
    }

    pub fn with_media_url(
        self_id: String,
        self_name: String,
        token: Option<String>,
        media_url: String,
        database: Option<Arc<DbManager>>,
    ) -> Result<Arc<Self>, String> {
        let media = inbound_media::Store::new(media_url)?.with_database(database.clone());
        Ok(Self::create(
            self_id,
            self_name,
            token,
            Some(media),
            database,
        ))
    }

    fn create(
        self_id: String,
        self_name: String,
        token: Option<String>,
        media: Option<inbound_media::Store>,
        database: Option<Arc<DbManager>>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            token,
            self_id,
            self_name,
            events,
            messages: Mutex::new(MessageCache::default()),
            history_messages: Mutex::new(MessageCache::default()),
            forward_records: Mutex::new(MessageCache::default()),
            seen: Mutex::new(MessageCache::default()),
            fast_dispatch: Mutex::new(FastDispatch::default()),
            send_lock: Mutex::new(()),
            media,
            database,
        })
    }

    async fn prepare_forward(&self, content: &str, id: &str, segments: &mut Vec<Value>) -> bool {
        let Some(record) = forward::ForwardRecord::parse(content) else {
            return false;
        };
        self.forward_records.lock().await.insert_bounded(
            id.to_owned(),
            record.onebot(),
            FORWARD_CACHE_LIMIT,
        );
        *segments = vec![
            segment("forward", json!({ "id": id })),
            segment("text", json!({ "text": record.context() })),
        ];
        true
    }

    async fn prepare_quoted_forward(
        &self,
        quote: &Quote,
        chat: &str,
        segments: &mut Vec<Value>,
    ) -> bool {
        let kind = quote.kind & 0xffff;
        if kind != 19
            && (kind != 49 || forward::app_type(&quote.content).is_some_and(|app| app != 19))
        {
            return false;
        }
        if self
            .prepare_forward(&quote.content, &quote.svrid, segments)
            .await
        {
            return true;
        }
        if let (Some(database), Ok(server_id)) = (&self.database, quote.svrid.parse::<i64>()) {
            match database
                .app_message_content(chat.to_owned(), server_id)
                .await
            {
                Ok(Some(content)) => {
                    return self.prepare_forward(&content, &quote.svrid, segments).await;
                }
                Ok(None) => {}
                Err(error) => warn!(%error, "无法读取引用的聊天记录"),
            }
        }
        false
    }

    async fn prepare_media(
        &self,
        kind: i64,
        content: &str,
        chat: &str,
        server_id: i64,
        segments: &mut Vec<Value>,
    ) {
        if let Some(media) = &self.media {
            if let Some(id) = media.register_message(kind, content, chat, server_id).await {
                let mut data = json!({ "file": id, "url": media.url(&id) });
                if kind & 0xffff == 47 {
                    data["sticker"] = json!(true);
                    data["sub_type"] = json!("sticker");
                }
                *segments = vec![segment(
                    if kind & 0xffff == 34 {
                        "record"
                    } else {
                        "image"
                    },
                    data,
                )];
            }
        }
    }

    async fn media_response(&self, id: &str, original_only: bool) -> Response {
        let Some(media) = &self.media else {
            return (StatusCode::NOT_FOUND, "media unavailable").into_response();
        };
        if !media.contains(id).await {
            return (StatusCode::NOT_FOUND, "media expired or unavailable").into_response();
        }
        let image = if original_only {
            media.original(id).await
        } else {
            media.image(id).await
        };
        match image {
            Ok(image) => (
                [
                    ("content-type", image.content_type),
                    ("cache-control", "private, no-store"),
                    ("x-content-type-options", "nosniff"),
                    ("referrer-policy", "no-referrer"),
                    (
                        "x-mimicwx-media-source",
                        match image.quality {
                            inbound_media::ImageQuality::Original => "original",
                            inbound_media::ImageQuality::Full => "full",
                            inbound_media::ImageQuality::Preview => "preview",
                            inbound_media::ImageQuality::Thumbnail => "thumbnail",
                        },
                    ),
                ],
                image.bytes,
            )
                .into_response(),
            Err(error) => {
                let status = if error.starts_with("媒体链接已失效")
                    || error == inbound_media::ORIGINAL_UNAVAILABLE
                {
                    StatusCode::NOT_FOUND
                } else {
                    warn!("OneBot 媒体处理失败: {error}");
                    StatusCode::BAD_GATEWAY
                };
                (status, error).into_response()
            }
        }
    }

    fn authorized(&self, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
        let Some(expected) = &self.token else {
            return false;
        };
        let bearer = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let supplied = bearer.or_else(|| {
            query
                .get("access_token")
                .or_else(|| query.get("token"))
                .map(String::as_str)
        });
        supplied == Some(expected.as_str()) && !expected.is_empty()
    }

    pub async fn event_from_message(&self, message: &DbMessage) -> Option<Value> {
        let id = message_id(message);
        let fast_id = self
            .fast_dispatch
            .lock()
            .await
            .claim_database((message.chat.clone(), message.local_id));
        if !self
            .seen
            .lock()
            .await
            .insert(format!("{}:{id}", message.chat), Value::Null)
        {
            return None;
        }
        let event = self.build_event(message, &id).await?;
        if let Some(fast_id) = fast_id {
            if self.messages.lock().await.finish_fast(fast_id, event) {
                debug!(
                    "session.db 提前消息已由正文补全: {}:{} -> {}",
                    message.chat, message.local_id, id,
                );
            }
            return None;
        }
        if message.is_self {
            None
        } else {
            Some(event)
        }
    }

    /// 在正文落库前，用 session.db 的即时快照构造纯文本事件。
    pub async fn event_from_provisional(&self, message: &DbMessage) -> Option<Value> {
        if message.is_self
            || message.local_id <= 0
            || message.talker.is_empty()
            || message.talker == self.self_id
            || !matches!(message.parsed, MsgContent::Text { .. })
        {
            return None;
        }
        let id = provisional_message_id(message);
        let mut dispatch = self.fast_dispatch.lock().await;
        if !dispatch.claim_fast((message.chat.clone(), message.local_id), id.clone()) {
            return None;
        }
        self.build_event(message, &id).await
    }

    async fn build_event(&self, message: &DbMessage, id: &str) -> Option<Value> {
        if matches!(message.parsed, MsgContent::System { .. }) {
            return None;
        }
        if message.chat.ends_with("@chatroom") && !message.is_self && message.talker.is_empty() {
            warn!("忽略缺少发送者的群消息: {}", message.chat);
            return None;
        }
        let event = self.render_message(message, id, &self.messages).await;
        self.messages
            .lock()
            .await
            .insert(id.to_string(), event.clone());
        Some(event)
    }

    async fn render_message(
        &self,
        message: &DbMessage,
        id: &str,
        messages: &Mutex<MessageCache>,
    ) -> Value {
        let is_group = message.chat.ends_with("@chatroom");
        let sender_id = if message.is_self {
            &self.self_id
        } else if is_group {
            &message.talker
        } else {
            &message.chat
        };
        let sender_display_name = if message.is_self {
            &self.self_name
        } else if is_group {
            &message.talker_display_name
        } else {
            &message.chat_display_name
        };
        let sender_nick_name = if message.is_self {
            &self.self_name
        } else if is_group {
            &message.talker_nick_name
        } else {
            &message.chat_nick_name
        };
        let sender_remark = if message.is_self {
            ""
        } else if is_group {
            &message.talker_remark
        } else {
            &message.chat_remark
        };
        let sender_nick_name = if sender_nick_name.trim().is_empty() {
            sender_display_name
        } else {
            sender_nick_name
        };
        let mut segments = Vec::new();
        if let Some(quote) = parse_quote(&message.content) {
            segments.push(segment("reply", json!({ "id": quote.svrid })));
            let mut quoted = quote_message_value(&quote, is_group, &message.chat);
            let mut forward_prepared = false;
            if let Some(segments) = quoted["message"].as_array_mut() {
                forward_prepared = self
                    .prepare_quoted_forward(&quote, &message.chat, segments)
                    .await;
                self.prepare_media(
                    quote.kind,
                    &quote.content,
                    &message.chat,
                    quote.svrid.parse().unwrap_or(0),
                    segments,
                )
                .await;
                let raw_message = cq_message(segments);
                quoted["raw_message"] = json!(raw_message);
            }
            let mut cache = messages.lock().await;
            if !cache.values.contains_key(&quote.svrid)
                || (forward_prepared && cache.values[&quote.svrid].get("message_seq").is_none())
            {
                cache.insert(quote.svrid, quoted);
            }
        }
        let mut body = message_segments(message, &self.self_id, &self.self_name);
        if let MsgContent::App {
            title,
            app_type: Some(19),
            ..
        } = &message.parsed
        {
            if !self.prepare_forward(&message.content, id, &mut body).await {
                let title = title.as_deref().unwrap_or("聊天记录");
                body = vec![segment(
                    "text",
                    json!({ "text": format!("[合并转发｜{title}]\n[聊天记录正文暂不可用]") }),
                )];
            }
        }
        self.prepare_media(
            message.msg_type,
            &message.content,
            &message.chat,
            message.server_id,
            &mut body,
        )
        .await;
        segments.extend(body);
        let mut event = json!({
            "post_type": "message",
            "message_type": if is_group { "group" } else { "private" },
            "sub_type": if is_group { "normal" } else { "friend" },
            "time": message.create_time,
            "self_id": self.self_id,
            "user_id": sender_id,
            "message_id": id,
            "message": segments,
            "raw_message": cq_message(&segments),
            "font": 0,
            "sender": {
                "user_id": sender_id, "nickname": sender_nick_name,
                "card": "",
                "remark": sender_remark,
                "display_name": sender_display_name,
                "sex": "unknown", "age": 0,
            },
            "message_seq": message.local_id,
            "chat_id": message.chat,
        });
        if is_group {
            event["group_id"] = json!(message.chat);
            event["group_name"] = json!(message.chat_display_name);
        } else {
            event["target_id"] = json!(self.self_id);
        }
        event
    }

    async fn cached_message(&self, id: &str) -> Option<Value> {
        let live = self.messages.lock().await.values.get(id).cloned();
        if live
            .as_ref()
            .is_some_and(|message| message.get("message_seq").is_some())
        {
            return live;
        }
        self.history_messages
            .lock()
            .await
            .values
            .get(id)
            .cloned()
            .or(live)
    }

    async fn handle_action(
        &self,
        state: &Arc<AppState>,
        action: &str,
        params: &Value,
    ) -> Result<Value, String> {
        match action {
            "get_login_info" => Ok(json!({ "user_id": self.self_id, "nickname": self.self_name })),
            "get_status" => {
                let online = state.wechat.check_status().await.to_string() == "已登录";
                Ok(json!({ "online": online, "good": online && state.db.is_some() }))
            }
            "get_version_info" => Ok(json!({
                "app_name": "MimicWX", "app_version": env!("CARGO_PKG_VERSION"),
                "protocol_version": "v11", "wechat_id_format": "wxid",
                "version": env!("CARGO_PKG_VERSION"),
            })),
            "get_friend_list" => Ok(json!(self.contacts(state).await?.into_iter()
                .filter(|contact| !contact.username.ends_with("@chatroom"))
                .map(|contact| json!({
                    "user_id": contact.username, "nickname": contact.nick_name, "remark": contact.remark,
                })).collect::<Vec<_>>())),
            "get_group_list" => Ok(json!(self.contacts(state).await?.into_iter()
                .filter(|contact| contact.username.ends_with("@chatroom"))
                .map(|contact| json!({
                    "group_id": contact.username, "group_name": contact.display_name,
                    "member_count": 0, "max_member_count": 0,
                })).collect::<Vec<_>>())),
            "get_stranger_info" => {
                let id = required_id(params, "user_id")?;
                let contact = self.contact(state, &id).await;
                let nickname = contact
                    .as_ref()
                    .map(|contact| contact.nick_name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| id.clone());
                let remark = contact
                    .as_ref()
                    .map(|contact| contact.remark.clone())
                    .unwrap_or_default();
                Ok(json!({
                    "user_id": id, "nickname": nickname, "remark": remark,
                    "card": "", "sex": "unknown", "age": 0,
                }))
            }
            "get_group_member_info" => {
                let id = required_id(params, "user_id")?;
                let contact = self.contact(state, &id).await;
                let nickname = contact
                    .as_ref()
                    .map(|contact| contact.nick_name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| id.clone());
                let remark = contact
                    .as_ref()
                    .map(|contact| contact.remark.clone())
                    .unwrap_or_default();
                let display_name = contact
                    .as_ref()
                    .map(|contact| contact.display_name.clone())
                    .unwrap_or_default();
                Ok(json!({
                    "user_id": id, "nickname": nickname, "card": "",
                    "remark": remark, "display_name": display_name,
                    "sex": "unknown", "age": 0,
                }))
            }
            "get_group_info" => {
                let id = required_id(params, "group_id")?;
                let contact = self.contacts(state).await?.into_iter()
                    .find(|contact| contact.username == id && id.ends_with("@chatroom"))
                    .ok_or_else(|| format!("找不到群聊: {id}"))?;
                Ok(json!({
                    "group_id": id, "group_name": contact.display_name,
                    "member_count": 0, "max_member_count": 0,
                }))
            }
            "get_msg" => {
                let id = required_id(params, "message_id")?;
                self.cached_message(&id).await
                    .ok_or_else(|| format!("消息不在缓存中: {id}"))
            }
            "get_forward_msg" => {
                let id = required_id(params, "id")?;
                self.forward_records.lock().await.values.get(&id).cloned()
                    .ok_or_else(|| "合并转发正文暂不可用，请重新读取原消息".into())
            }
            "get_image" => {
                let id = required_id(params, "file")?;
                let media = self.media.as_ref().ok_or("媒体代理尚未配置")?;
                let image = media.image(&id).await?;
                let extension = image.content_type.rsplit('/').next().unwrap_or("image");
                Ok(json!({
                    "file": id, "filename": format!("{id}.{extension}"),
                    "size": image.bytes.len(), "url": media.url(&id),
                }))
            }
            "get_group_msg_history" | "get_friend_msg_history" => {
                self.message_history(state.db.as_ref(), action == "get_group_msg_history", params).await
            }
            "send_private_msg" => {
                let id = required_id(params, "user_id")?;
                if id.ends_with("@chatroom") { return Err("send_private_msg 不接受群聊 ID".into()); }
                self.send_message(state, &id, params).await
            }
            "send_group_msg" => {
                let id = required_id(params, "group_id")?;
                if !id.ends_with("@chatroom") { return Err("微信 group_id 必须以 @chatroom 结尾".into()); }
                self.send_message(state, &id, params).await
            }
            "send_msg" => {
                let key = match params.get("message_type").and_then(Value::as_str) {
                    Some("group") => "group_id",
                    Some("private") => "user_id",
                    _ if params.get("group_id").is_some() => "group_id",
                    _ => "user_id",
                };
                self.send_message(state, &required_id(params, key)?, params).await
            }
            "mark_msg_as_read" => Ok(Value::Null),
            "can_send_image" => Ok(json!({ "yes": true })),
            "can_send_record" => Ok(json!({ "yes": crate::audio::voice_available().await })),
            _ => Err(format!("不支持的动作: {action}")),
        }
    }

    async fn send_message(
        &self,
        state: &Arc<AppState>,
        target: &str,
        params: &Value,
    ) -> Result<Value, String> {
        let _send_guard = self.send_lock.lock().await;
        let outgoing = outgoing_message(params)?;
        let target = target.strip_prefix("private:").unwrap_or(target);
        let contacts = self.contacts(state).await?;
        let contact = contacts
            .iter()
            .find(|contact| contact.username == target)
            .ok_or_else(|| "发送目标不在联系人缓存中，拒绝按不明确的名称发送".to_string())?;
        if contacts
            .iter()
            .filter(|other| other.display_name == contact.display_name)
            .count()
            != 1
        {
            return Err("发送目标的显示名重复，无法安全定位微信会话".into());
        }
        let infer_names = params.get("auto_escape").and_then(Value::as_bool) != Some(true);
        let needs_members = target.ends_with("@chatroom")
            && outgoing.parts.iter().any(|part| match part {
                OutgoingPart::At(_) => true,
                OutgoingPart::Text(value) => infer_names && value.contains(['@', '＠']),
                _ => false,
            });
        let (parts, mention_names) = if needs_members {
            let db = state.db.as_ref().ok_or("微信数据库不可用")?;
            match db.group_members(target).await {
                Ok(members) => {
                    mention::resolve_mentions(outgoing.parts, &members, &self.self_id, infer_names)?
                }
                Err(error)
                    if outgoing
                        .parts
                        .iter()
                        .any(|part| matches!(part, OutgoingPart::At(_))) =>
                {
                    return Err(format!("无法读取当前群成员，已取消原生 @ 发送: {error}"));
                }
                Err(error) => {
                    warn!(%error, "无法读取当前群成员，按原文本发送");
                    (outgoing.parts, HashMap::new())
                }
            }
        } else {
            (outgoing.parts, HashMap::new())
        };
        let mut native_quote = None;
        let mut sent_segments = Vec::new();
        if let Some(quote_id) = outgoing.quote_id {
            if parts.iter().any(|part| {
                matches!(
                    part,
                    OutgoingPart::Image(_) | OutgoingPart::Sticker(_) | OutgoingPart::Record(_)
                )
            }) {
                return Err("微信原生引用暂仅支持文字回复，请将出站媒体另行发送".into());
            }
            let server_id = self.resolve_quote_id(target, &quote_id, &contacts).await?;
            let history = state
                .db
                .as_ref()
                .ok_or("微信历史数据库暂不可用，无法核对原消息")?
                .quote_history(target.to_owned(), server_id)
                .await
                .map_err(|error| format!("查找引用目标失败: {error}"))?;
            native_quote = Some(native_quote_target(history)?);
            sent_segments.push(segment("reply", json!({ "id": quote_id })));
        }
        let mut text = Vec::new();
        let mut receipt = None;
        let mut verified = true;
        let mut confirmation_pending = false;
        for part in parts {
            let native_sticker = matches!(&part, OutgoingPart::Sticker(_));
            match part {
                OutgoingPart::Text(value) => push_text(&mut text, value),
                OutgoingPart::At(id) => {
                    if let Some(name) = mention_names.get(&id) {
                        text.push(TextPart::Mention {
                            id,
                            name: name.clone(),
                        });
                    } else {
                        let name = if id == "all" {
                            "全体成员".to_string()
                        } else {
                            self.display_name(state, &id).await
                        };
                        push_text(&mut text, format!("@{name}\u{2005}"));
                    }
                }
                OutgoingPart::Image(source) | OutgoingPart::Sticker(source) => {
                    if !text.is_empty() {
                        let response = submit_outgoing_text(
                            state,
                            &contact.display_name,
                            std::mem::take(&mut text),
                            None,
                            &mut sent_segments,
                        )
                        .await?;
                        verified &= response.verified;
                        confirmation_pending |= response.confirmation_pending;
                    }
                    let bytes = if source.len() == 64
                        && source.bytes().all(|byte| byte.is_ascii_hexdigit())
                    {
                        self.media
                            .as_ref()
                            .ok_or("媒体代理尚未配置")?
                            .image(&source)
                            .await?
                            .bytes
                            .to_vec()
                    } else {
                        media::image_bytes(&source).await?
                    };
                    let response = if native_sticker {
                        let response =
                            api::submit_sticker_bytes(state, &contact.display_name, bytes).await?;
                        ensure_sticker_sent(&response)?;
                        response
                    } else {
                        let response =
                            api::submit_image_bytes(state, &contact.display_name, bytes).await?;
                        ensure_image_sent(&response)?;
                        response
                    };
                    verified &= response.verified;
                    confirmation_pending |= response.confirmation_pending;
                    let data = if native_sticker {
                        json!({ "file": source, "sticker": true, "sub_type": "sticker" })
                    } else {
                        json!({ "file": source })
                    };
                    sent_segments.push(segment("image", data));
                    receipt = Some(response);
                }
                OutgoingPart::Record(source) => {
                    let bytes = media::audio_bytes(&source).await?;
                    if !text.is_empty() {
                        let response = submit_outgoing_text(
                            state,
                            &contact.display_name,
                            std::mem::take(&mut text),
                            None,
                            &mut sent_segments,
                        )
                        .await?;
                        verified &= response.verified;
                        confirmation_pending |= response.confirmation_pending;
                    }
                    let response =
                        api::submit_voice_bytes(state, &contact.display_name, bytes).await?;
                    ensure_voice_sent(&response)?;
                    verified &= response.verified;
                    confirmation_pending |= response.confirmation_pending;
                    sent_segments.push(segment("record", json!({ "file": "[outgoing voice]" })));
                    receipt = Some(response);
                }
            }
        }
        if !text.is_empty() {
            let response = submit_outgoing_text(
                state,
                &contact.display_name,
                text,
                native_quote,
                &mut sent_segments,
            )
            .await?;
            verified &= response.verified;
            confirmation_pending |= response.confirmation_pending;
            receipt = Some(response);
        }
        receipt.ok_or_else(|| "消息内容为空".to_string())?;
        let id = format!(
            "sent_{}_{}",
            now_millis(),
            SEND_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let mut cached = json!({
            "message_id": id, "real_id": id, "time": now_seconds(),
            "message_type": if target.ends_with("@chatroom") { "group" } else { "private" },
            "sender": { "user_id": self.self_id, "nickname": self.self_name },
            "message": sent_segments, "raw_message": cq_message(&sent_segments),
            "chat_id": target,
        });
        if target.ends_with("@chatroom") {
            cached["group_id"] = json!(target);
        }
        self.messages.lock().await.insert(id.clone(), cached);
        Ok(json!({
            "message_id": id, "verified": verified,
            "confirmation_pending": confirmation_pending,
        }))
    }

    async fn resolve_quote_id(
        &self,
        target: &str,
        quote_id: &str,
        contacts: &[ContactInfo],
    ) -> Result<i64, String> {
        if let Ok(id) = quote_id.parse::<i64>() {
            if id != 0 {
                return Ok(id);
            }
        }
        let quote = self
            .cached_message(quote_id)
            .await
            .ok_or("原消息尚未取得微信真实 ID，请等待落库后重试")?;
        if !quote_belongs_to_target(&quote, target) {
            return Err("不能引用其他会话中的消息".into());
        }
        if let Some(id) = native_message_id(&quote) {
            return Ok(id);
        }

        let sequence = quote["message_seq"]
            .as_i64()
            .filter(|sequence| *sequence > 0)
            .ok_or("原消息尚未取得微信真实 ID，请等待落库后重试")?;
        let body = quote_content(&quote, contacts, &self.self_id, &self.self_name);
        let cache = self.messages.lock().await;
        let candidates: HashSet<_> = cache
            .values
            .values()
            .filter(|message| {
                quote_belongs_to_target(message, target)
                    && message["sender"]["user_id"] == quote["sender"]["user_id"]
                    && message["message_seq"].as_i64() == Some(sequence)
                    && message["time"] == quote["time"]
                    && quote_content(message, contacts, &self.self_id, &self.self_name) == body
            })
            .filter_map(native_message_id)
            .collect();
        let candidate_count = candidates.len();
        candidates
            .into_iter()
            .next()
            .filter(|_| candidate_count == 1)
            .ok_or_else(|| "被引用消息尚未取得唯一的微信真实 ID，请等待落库后重试".into())
    }

    async fn contacts(&self, state: &Arc<AppState>) -> Result<Vec<ContactInfo>, String> {
        let db = state
            .db
            .as_ref()
            .ok_or_else(|| "微信数据库不可用".to_string())?;
        Ok(db.get_contacts().await)
    }

    async fn display_name(&self, state: &Arc<AppState>, id: &str) -> String {
        if id == self.self_id {
            return self.self_name.clone();
        }
        match state.db.as_ref() {
            Some(db) => db.display_name(id).await,
            None => id.to_string(),
        }
    }

    async fn contact(&self, state: &Arc<AppState>, id: &str) -> Option<ContactInfo> {
        state.db.as_ref()?.contact_info(id).await
    }
}

pub async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Some(runtime) = state.onebot.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "OneBot disabled").into_response();
    };
    if let Some(id) = query.get("media") {
        let original_only = query.get("original").is_some_and(|value| value == "1");
        return runtime.media_response(id, original_only).await;
    }
    if !runtime.authorized(&headers, &query) {
        return (StatusCode::UNAUTHORIZED, "invalid OneBot token").into_response();
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(error) => return error.into_response(),
    };
    ws.on_upgrade(move |socket| handle_socket(state, runtime, Socket::Forward(socket), None))
        .into_response()
}

enum Socket {
    Forward(WebSocket),
    Reverse(Box<reverse::WebSocketClient>),
}

impl Socket {
    async fn recv(&mut self) -> Option<Result<Message, String>> {
        match self {
            Self::Forward(socket) => socket
                .recv()
                .await
                .map(|frame| frame.map_err(|e| e.to_string())),
            Self::Reverse(socket) => socket.next().await.map(|frame| {
                frame
                    .map(|message| match message {
                        ClientMessage::Text(text) => Message::Text(text.as_str().into()),
                        ClientMessage::Close(_) => Message::Close(None),
                        ClientMessage::Ping(data) => Message::Ping(data),
                        ClientMessage::Pong(data) => Message::Pong(data),
                        ClientMessage::Binary(data) => Message::Binary(data),
                        ClientMessage::Frame(_) => Message::Binary(Default::default()),
                    })
                    .map_err(|e| e.to_string())
            }),
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), ()> {
        match self {
            Self::Forward(socket) => socket.send(message).await.map_err(|_| ()),
            Self::Reverse(socket) => {
                let message = match message {
                    Message::Text(text) => ClientMessage::Text(text.as_str().into()),
                    Message::Ping(data) => ClientMessage::Ping(data),
                    Message::Pong(data) => ClientMessage::Pong(data),
                    Message::Binary(data) => ClientMessage::Binary(data),
                    Message::Close(_) => ClientMessage::Close(None),
                };
                tokio::time::timeout(Duration::from_secs(10), socket.send(message))
                    .await
                    .map_err(|_| ())?
                    .map_err(|_| ())
            }
        }
    }
}

type ActionFuture = Pin<Box<dyn Future<Output = Value> + Send>>;

#[derive(Default)]
struct ActionQueue {
    tasks: JoinSet<(bool, Value)>,
    sends: VecDeque<ActionFuture>,
    sending: bool,
}

impl ActionQueue {
    fn len(&self) -> usize {
        self.tasks.len() + self.sends.len()
    }

    fn push(&mut self, ordered: bool, action: impl Future<Output = Value> + Send + 'static) {
        let action: ActionFuture = Box::pin(action);
        if ordered && self.sending {
            self.sends.push_back(action);
        } else {
            self.sending |= ordered;
            self.tasks.spawn(async move { (ordered, action.await) });
        }
    }

    async fn next(&mut self) -> Result<Value, JoinError> {
        let (ordered, response) = self.tasks.join_next().await.expect("active actions")?;
        if ordered {
            if let Some(action) = self.sends.pop_front() {
                self.tasks.spawn(async move { (true, action.await) });
            } else {
                self.sending = false;
            }
        }
        Ok(response)
    }
}

async fn run_action(
    state: Arc<AppState>,
    runtime: Arc<OneBotRuntime>,
    request: Value,
    reverse: Option<Arc<reverse::Peer>>,
) -> Value {
    let mut params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let action = request.get("action").and_then(Value::as_str);
    let result =
        if !params.is_object() {
            Err("params 必须是对象".into())
        } else if let Some(action) = action {
            let prepared = reverse
                .as_ref()
                .map_or(Ok(()), |peer| peer.prepare_action(action, &mut params));
            match prepared {
                Err(error) => Err(error),
                Ok(()) => runtime
                    .handle_action(&state, action, &params)
                    .await
                    .map(|data| match &reverse {
                        Some(peer) => peer.action_result(action, data),
                        None => data,
                    }),
            }
        } else {
            Err("缺少 action".into())
        };
    if let (Some(action @ ("send_msg" | "send_group_msg" | "send_private_msg")), Err(error)) =
        (action, &result)
    {
        warn!(action, reason = %error, "OneBot 发送失败");
    }
    action_response(request.get("echo"), result)
}

async fn handle_socket(
    state: Arc<AppState>,
    runtime: Arc<OneBotRuntime>,
    mut socket: Socket,
    reverse: Option<Arc<reverse::Peer>>,
) {
    let mut events = runtime.events.subscribe();
    let mut actions = ActionQueue::default();
    let self_id = reverse
        .as_ref()
        .map_or_else(|| json!(runtime.self_id), |peer| peer.self_id());
    let lifecycle = json!({
        "time": now_seconds(), "self_id": self_id,
        "post_type": "meta_event", "meta_event_type": "lifecycle", "sub_type": "connect",
    });
    if socket
        .send(Message::Text(lifecycle.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    let mut last_frame = tokio::time::Instant::now();
    heartbeat.tick().await;
    loop {
        let payload = tokio::select! {
            response = actions.next(), if actions.len() > 0 => {
                match response {
                    Ok(response) => response.to_string(),
                    Err(error) => {
                        warn!("OneBot 动作任务失败，关闭连接: {error}");
                        break;
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Ok(payload) => match &reverse {
                        Some(peer) => match peer.event(payload) {
                            Some(event) => event,
                            None => continue,
                        },
                        None => payload,
                    },
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("OneBot 事件队列落后 {skipped} 条，关闭连接以便客户端重新同步");
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = socket.recv() => {
                last_frame = tokio::time::Instant::now();
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let request: Value = match serde_json::from_str(&text) {
                            Ok(value) => value,
                            Err(_) => {
                                let response = action_response(None, Err("无效 JSON".into()));
                                if socket.send(Message::Text(response.to_string().into())).await.is_err() { break; }
                                continue;
                            }
                        };
                        if actions.len() >= ACTION_LIMIT {
                            action_response(request.get("echo"), Err("动作队列已满，请稍后重试".into())).to_string()
                        } else {
                            let ordered = matches!(
                                request.get("action").and_then(Value::as_str),
                                Some("send_msg" | "send_group_msg" | "send_private_msg")
                            );
                            actions.push(ordered, run_action(state.clone(), runtime.clone(), request, reverse.clone()));
                            continue;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() { break; }
                        continue;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(error)) => { debug!("OneBot WebSocket 断开: {error}"); break; }
                    _ => continue,
                }
            }
            _ = heartbeat.tick() => {
                if reverse.is_some() && (last_frame.elapsed() >= Duration::from_secs(75)
                    || socket.send(Message::Ping(Default::default())).await.is_err()) {
                    break;
                }
                if actions.len() < ACTION_LIMIT {
                    let state = state.clone();
                    let runtime = runtime.clone();
                    let self_id = self_id.clone();
                    actions.push(false, async move {
                        let status = runtime.handle_action(&state, "get_status", &json!({})).await
                            .unwrap_or_else(|_| json!({ "online": false, "good": false }));
                        json!({
                            "time": now_seconds(), "self_id": self_id,
                            "post_type": "meta_event", "meta_event_type": "heartbeat",
                            "status": status, "interval": 30000,
                        })
                    });
                }
                continue;
            }
        };
        if socket.send(Message::Text(payload.into())).await.is_err() {
            break;
        }
    }
    actions.tasks.detach_all();
}

fn action_response(echo: Option<&Value>, result: Result<Value, String>) -> Value {
    let mut response = match result {
        Ok(data) => json!({ "status": "ok", "retcode": 0, "data": data }),
        Err(error) => json!({
            "status": "failed", "retcode": if error.starts_with("不支持的") { 1404 } else { 1400 },
            "data": null, "message": error, "wording": error,
        }),
    };
    if let Some(echo) = echo {
        response["echo"] = echo.clone();
    }
    response
}

fn segment(kind: &str, data: Value) -> Value {
    json!({ "type": kind, "data": data })
}

fn push_text(parts: &mut Vec<TextPart>, text: String) {
    if let Some(TextPart::Text(previous)) = parts.last_mut() {
        previous.push_str(&text);
    } else {
        parts.push(TextPart::Text(text));
    }
}

async fn submit_outgoing_text(
    state: &Arc<AppState>,
    to: &str,
    parts: Vec<TextPart>,
    quote: Option<NativeQuoteTarget>,
    sent_segments: &mut Vec<Value>,
) -> Result<api::SendResponse, String> {
    let segments = parts
        .iter()
        .map(|part| match part {
            TextPart::Text(text) => segment("text", json!({ "text": text })),
            TextPart::Mention { id, name } => segment("at", json!({ "qq": id, "name": name })),
        })
        .collect::<Vec<_>>();
    let response = api::submit_text(state, to, parts, quote).await?;
    ensure_text_sent(&response)?;
    sent_segments.extend(segments);
    Ok(response)
}

fn message_id(message: &DbMessage) -> String {
    if message.server_id != 0 {
        message.server_id.to_string()
    } else {
        format!("local_{}_{}", message.chat, message.local_id)
    }
}

fn provisional_message_id(message: &DbMessage) -> String {
    if message.server_id != 0 {
        message.server_id.to_string()
    } else {
        format!("fast_{}_{}", message.chat, message.local_id)
    }
}

fn message_segments(message: &DbMessage, self_id: &str, self_name: &str) -> Vec<Value> {
    let source = if message.source.contains("<atuserlist") {
        &message.source
    } else {
        &message.content
    };
    match &message.parsed {
        MsgContent::Text { text } => text_segments(text, source, self_id, self_name),
        MsgContent::Image { path: Some(path) } if media::is_url(path) => {
            vec![segment("image", json!({ "file": path, "url": path }))]
        }
        MsgContent::Emoji { url: Some(url) } if media::is_url(url) => {
            vec![segment(
                "image",
                json!({ "file": url, "url": url, "sticker": true, "sub_type": "sticker" }),
            )]
        }
        MsgContent::App {
            title,
            app_type: Some(57),
            ..
        } => text_segments(title.as_deref().unwrap_or(""), source, self_id, self_name),
        MsgContent::App {
            title,
            desc,
            url: Some(url),
            app_type: Some(4 | 5),
        } if is_http_url(url) => {
            let title = title
                .as_deref()
                .filter(|title| !title.is_empty())
                .or_else(|| desc.as_deref().filter(|desc| !desc.is_empty()))
                .unwrap_or("分享链接");
            let content = desc.as_deref().unwrap_or("");
            let text = [title, content, url]
                .into_iter()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            vec![
                segment(
                    "share",
                    json!({ "url": url, "title": title, "content": content }),
                ),
                segment("text", json!({ "text": text })),
            ]
        }
        MsgContent::App {
            title, desc, url, ..
        } => {
            let text = [title.as_deref(), desc.as_deref(), url.as_deref()]
                .into_iter()
                .flatten()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            vec![segment("text", json!({ "text": text }))]
        }
        _ => vec![segment(
            "text",
            json!({ "text": message.parsed.preview(1000) }),
        )],
    }
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn text_segments(text: &str, source: &str, self_id: &str, self_name: &str) -> Vec<Value> {
    let mut ids = mention_ids_in_xml(source);
    if !ids.is_empty() {
        let mentions: Vec<_> = text.match_indices("\u{2005}@").collect();
        if mentions.len() == ids.len() {
            let mut cursor = 0;
            let mut segments = Vec::new();
            for ((start, prefix), id) in mentions.into_iter().zip(&ids) {
                let after_at = start + prefix.len();
                let Some(end) = text[after_at..].find('\u{2005}') else {
                    break;
                };
                if cursor < start {
                    segments.push(segment("text", json!({ "text": &text[cursor..start] })));
                }
                segments.push(segment(
                    "at",
                    json!({ "qq": id, "name": &text[after_at..after_at + end] }),
                ));
                cursor = after_at + end + '\u{2005}'.len_utf8();
            }
            if segments.iter().filter(|part| part["type"] == "at").count() == ids.len() {
                if cursor < text.len() {
                    segments.push(segment("text", json!({ "text": &text[cursor..] })));
                }
                return segments;
            }
        }
    }
    if !self_name.is_empty() {
        let marker = format!("@{self_name}");
        if text.match_indices(&marker).any(|(offset, _)| {
            text[offset + marker.len()..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
        }) && !ids.iter().any(|id| id == self_id)
        {
            ids.push(self_id.to_string());
        }
    }
    let mut remaining = text.to_string();
    let mut result = Vec::new();
    for id in ids {
        let marker = if id == self_id && remaining.contains(&format!("@{self_name}")) {
            format!("@{self_name}")
        } else {
            format!("@{id}")
        };
        if let Some(offset) = remaining.find(&marker) {
            if offset > 0 {
                result.push(segment("text", json!({ "text": &remaining[..offset] })));
            }
            result.push(segment("at", json!({ "qq": id })));
            remaining = remaining[offset + marker.len()..].to_string();
        } else {
            result.push(segment("at", json!({ "qq": id })));
        }
    }
    if !remaining.is_empty() {
        result.push(segment("text", json!({ "text": remaining })));
    }
    result
}

fn mention_ids_in_xml(source: &str) -> Vec<String> {
    xml_text(source, "atuserlist")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn native_mentions_match(message: &DbMessage, expected: &[String]) -> bool {
    let mut ids = mention_ids_in_xml(&message.source);
    if ids.is_empty() {
        ids = mention_ids_in_xml(&message.content);
    }
    ids.iter().collect::<HashSet<_>>() == expected.iter().collect::<HashSet<_>>()
}

#[derive(Debug)]
struct Quote {
    svrid: String,
    sender: String,
    displayname: String,
    content: String,
    kind: i64,
    createtime: i64,
}

fn parse_quote(content: &str) -> Option<Quote> {
    let mut reader = Reader::from_str(content);
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) if element.name().as_ref() == b"refermsg" => {
                let body = reader.read_text(element.name()).ok()?;
                let from = xml_text(&body, "fromusr").unwrap_or_default();
                let chat = xml_text(&body, "chatusr").unwrap_or_default();
                return Some(Quote {
                    svrid: xml_text(&body, "svrid").filter(|id| !id.is_empty())?,
                    sender: if !from.is_empty() && !from.ends_with("@chatroom") {
                        from
                    } else {
                        chat
                    },
                    displayname: xml_text(&body, "displayname").unwrap_or_default(),
                    content: xml_text(&body, "content").unwrap_or_default(),
                    kind: xml_text(&body, "type")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(1),
                    createtime: xml_text(&body, "createtime")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0),
                });
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

fn quote_message_value(quote: &Quote, is_group: bool, chat_id: &str) -> Value {
    let quoted = DbMessage {
        local_id: 0,
        server_id: 0,
        create_time: quote.createtime,
        content: quote.content.clone(),
        source: String::new(),
        parsed: crate::db::parse_msg_content(quote.kind, &quote.content),
        msg_type: quote.kind,
        talker: quote.sender.clone(),
        talker_display_name: quote.displayname.clone(),
        talker_nick_name: quote.displayname.clone(),
        talker_remark: String::new(),
        chat: chat_id.to_string(),
        chat_display_name: String::new(),
        chat_nick_name: String::new(),
        chat_remark: String::new(),
        is_self: false,
    };
    let segments = message_segments(&quoted, "", "");
    let mut value = json!({
        "message_id": quote.svrid, "real_id": quote.svrid, "time": quote.createtime,
        "message_type": if is_group { "group" } else { "private" },
        "sender": { "user_id": quote.sender, "nickname": quote.displayname },
        "message": segments, "raw_message": cq_message(&segments),
        "chat_id": chat_id,
    });
    if is_group {
        value["group_id"] = json!(chat_id);
    }
    value
}

fn xml_text(body: &str, tag: &str) -> Option<String> {
    let mut reader = Reader::from_str(body);
    let mut inside = false;
    let mut result = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) if element.name().as_ref() == tag.as_bytes() => inside = true,
            Ok(Event::Text(text)) if inside => result.push_str(&text.unescape().ok()?),
            Ok(Event::CData(text)) if inside => {
                result.push_str(std::str::from_utf8(text.as_ref()).ok()?)
            }
            Ok(Event::End(element)) if inside && element.name().as_ref() == tag.as_bytes() => {
                return Some(result)
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

#[derive(Debug, PartialEq)]
enum OutgoingPart {
    Text(String),
    At(String),
    Image(String),
    Sticker(String),
    Record(String),
}

#[derive(Debug)]
struct OutgoingMessage {
    parts: Vec<OutgoingPart>,
    quote_id: Option<String>,
}

fn outgoing_message(params: &Value) -> Result<OutgoingMessage, String> {
    let message = params
        .get("message")
        .ok_or_else(|| "缺少 message".to_string())?;
    let segments = if let Some(text) = message.as_str() {
        if params
            .get("auto_escape")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            vec![segment("text", json!({ "text": text }))]
        } else {
            parse_cq(text)?
        }
    } else {
        message
            .as_array()
            .cloned()
            .ok_or_else(|| "message 必须是字符串或消息段数组".to_string())?
    };
    let mut outgoing = OutgoingMessage {
        parts: Vec::new(),
        quote_id: None,
    };
    for item in segments {
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "消息段缺少 type".to_string())?;
        let data = item
            .get("data")
            .ok_or_else(|| "消息段缺少 data".to_string())?;
        match kind {
            "text" => outgoing
                .parts
                .push(OutgoingPart::Text(required_id(data, "text")?)),
            "reply" => outgoing.quote_id = Some(required_id(data, "id")?),
            "at" => outgoing
                .parts
                .push(OutgoingPart::At(required_id(data, "qq")?)),
            "image" => {
                let source = required_id(data, "file")?;
                let sticker = data["sticker"].as_bool() == Some(true)
                    || matches!(data["sticker"].as_str(), Some("true" | "1"))
                    || data["sub_type"].as_str() == Some("sticker");
                outgoing.parts.push(if sticker {
                    OutgoingPart::Sticker(source)
                } else {
                    OutgoingPart::Image(source)
                });
            }
            "record" => outgoing
                .parts
                .push(OutgoingPart::Record(required_id(data, "file")?)),
            _ => return Err(format!("不支持的消息段: {kind}")),
        }
    }
    if outgoing.parts.is_empty() {
        return Err("消息内容为空".into());
    }
    Ok(outgoing)
}

fn parse_cq(text: &str) -> Result<Vec<Value>, String> {
    let mut remaining = text;
    let mut result = Vec::new();
    while let Some(start) = remaining.find("[CQ:") {
        if start > 0 {
            result.push(segment(
                "text",
                json!({ "text": cq_unescape(&remaining[..start]) }),
            ));
        }
        let suffix = &remaining[start + 4..];
        let end = suffix
            .find(']')
            .ok_or_else(|| "CQ 码没有闭合".to_string())?;
        let mut fields = suffix[..end].split(',');
        let kind = fields.next().unwrap_or_default();
        let mut data = serde_json::Map::new();
        for field in fields {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| "无效 CQ 码参数".to_string())?;
            data.insert(key.to_string(), json!(cq_unescape(value)));
        }
        result.push(segment(kind, Value::Object(data)));
        remaining = &suffix[end + 1..];
    }
    if !remaining.is_empty() {
        result.push(segment("text", json!({ "text": cq_unescape(remaining) })));
    }
    Ok(result)
}

fn cq_unescape(value: &str) -> String {
    value
        .replace("&#91;", "[")
        .replace("&#93;", "]")
        .replace("&#44;", ",")
        .replace("&amp;", "&")
}

fn cq_escape(value: &str, parameter: bool) -> String {
    let escaped = value
        .replace('&', "&amp;")
        .replace('[', "&#91;")
        .replace(']', "&#93;");
    if parameter {
        escaped.replace(',', "&#44;")
    } else {
        escaped
    }
}

fn quote_content(
    quote: &Value,
    contacts: &[ContactInfo],
    self_id: &str,
    self_name: &str,
) -> String {
    let parsed;
    let segments = if let Some(segments) = quote["message"].as_array() {
        segments.as_slice()
    } else {
        let source = quote["message"]
            .as_str()
            .or_else(|| quote["raw_message"].as_str())
            .unwrap_or("");
        parsed = parse_cq(source).unwrap_or_default();
        &parsed
    };
    segments
        .iter()
        .map(|item| match item["type"].as_str().unwrap_or("") {
            "text" => item["data"]["text"].as_str().unwrap_or("").to_string(),
            "reply" => String::new(),
            "at" => {
                let id = item["data"]["qq"].as_str().unwrap_or("");
                let name = if id == "all" {
                    "全体成员"
                } else if id == self_id {
                    self_name
                } else {
                    contacts
                        .iter()
                        .find(|contact| contact.username == id)
                        .map(|contact| contact.display_name.as_str())
                        .unwrap_or(if id.is_empty() { "群友" } else { id })
                };
                format!("@{name}")
            }
            "image" => "[图片]".into(),
            "record" => "[语音]".into(),
            "video" => "[视频]".into(),
            "face" => "[表情]".into(),
            "file" => "[文件]".into(),
            _ => "[消息]".into(),
        })
        .collect::<String>()
}

#[cfg(test)]
fn quote_preview(
    quote: &Value,
    contacts: &[ContactInfo],
    self_id: &str,
    self_name: &str,
) -> String {
    let content = quote_content(quote, contacts, self_id, self_name);
    let mut characters = content.trim().chars();
    let mut preview: String = characters.by_ref().take(300).collect();
    if preview.is_empty() {
        return "[消息]".into();
    }
    if characters.next().is_some() {
        preview.push('…');
    }
    preview
}

fn native_message_id(message: &Value) -> Option<i64> {
    message["message_id"]
        .as_str()?
        .parse()
        .ok()
        .filter(|id| *id != 0)
}

fn native_quote_target(
    history: crate::db::QuoteHistory,
) -> Result<crate::wechat::NativeQuoteTarget, String> {
    let original = &history.messages[history.target_index];
    let text = match &original.parsed {
        MsgContent::Text { text } => text.clone(),
        MsgContent::App {
            title: Some(title),
            app_type: Some(57),
            ..
        } => title.clone(),
        MsgContent::Image { .. } => "[图片]".into(),
        MsgContent::Voice { .. } => "[语音]".into(),
        MsgContent::Video { .. } => "[视频]".into(),
        MsgContent::Emoji { .. } => "[表情]".into(),
        _ => return Err("这类微信原消息暂无法核对引用内容".into()),
    };
    let (display_name, nickname) = if original.chat.ends_with("@chatroom") || original.is_self {
        (&original.talker_display_name, &original.talker_nick_name)
    } else {
        (&original.chat_display_name, &original.chat_nick_name)
    };
    let sender_name = if display_name.trim().is_empty() {
        nickname
    } else {
        display_name
    };
    if original.server_id == 0 || text.trim().is_empty() || sender_name.trim().is_empty() {
        return Err("被引用消息缺少完整内容或原作者".into());
    }
    Ok(crate::wechat::NativeQuoteTarget {
        server_id: original.server_id.to_string(),
        text,
        sender_name: sender_name.to_string(),
        is_self: original.is_self,
        history,
    })
}

pub(crate) fn native_reply_matches(content: &str, quote_id: &str, text: &str) -> bool {
    xml_text(content, "type").as_deref() == Some("57")
        && xml_text(content, "title").is_some_and(|title| title.trim() == text.trim())
        && parse_quote(content).is_some_and(|quote| quote.svrid == quote_id)
}

fn cq_message(segments: &[Value]) -> String {
    segments
        .iter()
        .map(|item| {
            let kind = item["type"].as_str().unwrap_or("text");
            if kind == "text" {
                return cq_escape(item["data"]["text"].as_str().unwrap_or(""), false);
            }
            let params = item["data"]
                .as_object()
                .map(|data| {
                    data.iter()
                        .filter_map(|(key, value)| {
                            value_string(value)
                                .map(|value| format!(",{key}={}", cq_escape(&value, true)))
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            format!("[CQ:{kind}{params}]")
        })
        .collect()
}

fn ensure_text_sent(response: &api::SendResponse) -> Result<(), String> {
    ensure_sent(response)?;
    if !response.verified && !response.confirmation_pending {
        return Err("尚未在目标会话中确认完整消息，请先检查微信实际发送结果".into());
    }
    Ok(())
}

fn ensure_sent(response: &api::SendResponse) -> Result<(), String> {
    if response.sent {
        Ok(())
    } else {
        Err(response.message.clone())
    }
}

fn ensure_image_sent(response: &api::SendResponse) -> Result<(), String> {
    ensure_sent(response)?;
    if !response.verified && !response.confirmation_pending {
        return Err("尚未在目标会话中确认图片送达，请先检查微信实际发送结果".into());
    }
    Ok(())
}

fn ensure_sticker_sent(response: &api::SendResponse) -> Result<(), String> {
    ensure_sent(response)?;
    if !response.verified && !response.confirmation_pending {
        return Err("尚未在目标会话中确认原生表情送达，请先检查微信实际发送结果".into());
    }
    Ok(())
}

fn ensure_voice_sent(response: &api::SendResponse) -> Result<(), String> {
    ensure_sent(response)?;
    if !response.verified && !response.confirmation_pending {
        return Err("尚未在目标会话中确认语音送达，请先检查微信实际发送结果".into());
    }
    Ok(())
}

fn quote_belongs_to_target(quote: &Value, target: &str) -> bool {
    quote
        .get("chat_id")
        .and_then(Value::as_str)
        .or_else(|| quote.get("group_id").and_then(Value::as_str))
        .is_some_and(|chat| !chat.is_empty() && chat == target)
}

fn value_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn required_id(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(value_string)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("缺少或无效的 {key}"))
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests;
