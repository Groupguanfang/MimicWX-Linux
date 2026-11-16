mod media;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use quick_xml::{events::Event, Reader};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, warn};

use crate::{
    api::{self, AppState},
    db::{ContactInfo, DbMessage, MsgContent},
};

pub const WS_PATH: &str = "/onebot/ws";
const CACHE_LIMIT: usize = 4096;
static SEND_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct MessageCache {
    values: HashMap<String, Value>,
    order: VecDeque<String>,
}

impl MessageCache {
    fn insert(&mut self, id: String, value: Value) -> bool {
        let fresh = !self.values.contains_key(&id);
        if fresh {
            self.order.push_back(id.clone());
        }
        self.values.insert(id, value);
        while self.order.len() > CACHE_LIMIT {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
        fresh
    }
}

pub struct OneBotRuntime {
    pub token: Option<String>,
    pub self_id: String,
    pub self_name: String,
    pub events: broadcast::Sender<String>,
    messages: Mutex<MessageCache>,
    seen: Mutex<MessageCache>,
}

impl OneBotRuntime {
    pub fn new(self_id: String, self_name: String, token: Option<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            token,
            self_id,
            self_name,
            events,
            messages: Mutex::new(MessageCache::default()),
            seen: Mutex::new(MessageCache::default()),
        })
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
        if !self
            .seen
            .lock()
            .await
            .insert(format!("{}:{id}", message.chat), Value::Null)
        {
            return None;
        }
        if matches!(message.parsed, MsgContent::System { .. }) {
            return None;
        }
        let is_group = message.chat.ends_with("@chatroom");
        let sender_id = if message.is_self {
            &self.self_id
        } else if is_group {
            &message.talker
        } else {
            &message.chat
        };
        let sender_name = if message.is_self {
            &self.self_name
        } else if is_group {
            &message.talker_display_name
        } else {
            &message.chat_display_name
        };
        let mut segments = Vec::new();
        if let Some(quote) = parse_quote(&message.content) {
            segments.push(segment("reply", json!({ "id": quote.svrid })));
            let quoted = quote_message_value(&quote, is_group, &message.chat);
            let mut cache = self.messages.lock().await;
            if !cache.values.contains_key(&quote.svrid) {
                cache.insert(quote.svrid, quoted);
            }
        }
        segments.extend(message_segments(message, &self.self_id, &self.self_name));
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
                "user_id": sender_id, "nickname": sender_name,
                "card": if is_group { sender_name.as_str() } else { "" },
                "sex": "unknown", "age": 0,
            },
            "message_seq": message.local_id,
        });
        if is_group {
            event["group_id"] = json!(message.chat);
            event["group_name"] = json!(message.chat_display_name);
        } else {
            event["target_id"] = json!(self.self_id);
        }
        self.messages.lock().await.insert(id, event.clone());
        if message.is_self {
            None
        } else {
            Some(event)
        }
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
            "get_stranger_info" | "get_group_member_info" => {
                let id = required_id(params, "user_id")?;
                let name = self.display_name(state, &id).await;
                Ok(json!({ "user_id": id, "nickname": name, "card": name, "sex": "unknown", "age": 0 }))
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
                self.messages.lock().await.values.get(&id).cloned()
                    .ok_or_else(|| format!("消息不在缓存中: {id}"))
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
            "can_send_record" => Ok(json!({ "yes": false })),
            _ => Err(format!("不支持的动作: {action}")),
        }
    }

    async fn send_message(
        &self,
        state: &Arc<AppState>,
        target: &str,
        params: &Value,
    ) -> Result<Value, String> {
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
        let mut parts = outgoing.parts;
        if let Some(quote_id) = outgoing.quote_id {
            let quote = self
                .messages
                .lock()
                .await
                .values
                .get(&quote_id)
                .cloned()
                .ok_or_else(|| "被引用的消息不在缓存中".to_string())?;
            if let Some(group_id) = quote.get("group_id").and_then(Value::as_str) {
                if !group_id.is_empty() && group_id != target {
                    return Err("不能引用其他会话中的消息".into());
                }
            }
            let preview = quote
                .get("raw_message")
                .and_then(Value::as_str)
                .unwrap_or("");
            let preview: String = preview.chars().take(300).collect();
            parts.insert(0, OutgoingPart::Text(format!("「引用：{preview}」\n")));
        }
        let mut text = String::new();
        let mut receipt = None;
        let mut sent_segments = Vec::new();
        for part in parts {
            match part {
                OutgoingPart::Text(value) => text.push_str(&value),
                OutgoingPart::At(id) => {
                    let name = if id == "all" {
                        "全体成员".to_string()
                    } else {
                        self.display_name(state, &id).await
                    };
                    text.push_str(&format!("@{name}\u{2005}"));
                }
                OutgoingPart::Image(source) => {
                    if !text.is_empty() {
                        let response = api::send_text(state, &contact.display_name, &text).await?;
                        ensure_sent(&response)?;
                        sent_segments.push(segment("text", json!({ "text": text })));
                        text.clear();
                    }
                    let bytes = media::image_bytes(&source).await?;
                    let response =
                        api::send_image_bytes(state, &contact.display_name, bytes).await?;
                    ensure_sent(&response)?;
                    sent_segments.push(segment("image", json!({ "file": source })));
                    receipt = Some(response);
                }
            }
        }
        if !text.is_empty() {
            let response = api::send_text(state, &contact.display_name, &text).await?;
            ensure_sent(&response)?;
            sent_segments.push(segment("text", json!({ "text": text })));
            receipt = Some(response);
        }
        let receipt = receipt.ok_or_else(|| "消息内容为空".to_string())?;
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
        });
        if target.ends_with("@chatroom") {
            cached["group_id"] = json!(target);
        }
        self.messages.lock().await.insert(id.clone(), cached);
        Ok(json!({ "message_id": id, "verified": receipt.verified }))
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
}

pub async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(runtime) = state.onebot.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "OneBot disabled").into_response();
    };
    if !runtime.authorized(&headers, &query) {
        return (StatusCode::UNAUTHORIZED, "invalid OneBot token").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(state, runtime, socket))
        .into_response()
}

async fn handle_socket(state: Arc<AppState>, runtime: Arc<OneBotRuntime>, mut socket: WebSocket) {
    let mut events = runtime.events.subscribe();
    let lifecycle = json!({
        "time": now_seconds(), "self_id": runtime.self_id,
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
    heartbeat.tick().await;
    loop {
        let payload = tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(payload) => payload,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("OneBot 事件队列落后 {skipped} 条，关闭连接以便客户端重新同步");
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = socket.recv() => {
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
                        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
                        let result = if !params.is_object() {
                            Err("params 必须是对象".into())
                        } else if let Some(action) = request.get("action").and_then(Value::as_str) {
                            runtime.handle_action(&state, action, &params).await
                        } else { Err("缺少 action".into()) };
                        action_response(request.get("echo"), result).to_string()
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
                let status = runtime.handle_action(&state, "get_status", &json!({})).await
                    .unwrap_or_else(|_| json!({ "online": false, "good": false }));
                json!({
                    "time": now_seconds(), "self_id": runtime.self_id,
                    "post_type": "meta_event", "meta_event_type": "heartbeat",
                    "status": status, "interval": 30000,
                }).to_string()
            }
        };
        if socket.send(Message::Text(payload.into())).await.is_err() {
            break;
        }
    }
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

fn message_id(message: &DbMessage) -> String {
    if message.server_id != 0 {
        message.server_id.to_string()
    } else {
        format!("local_{}_{}", message.chat, message.local_id)
    }
}

fn message_segments(message: &DbMessage, self_id: &str, self_name: &str) -> Vec<Value> {
    match &message.parsed {
        MsgContent::Text { text } => text_segments(text, &message.content, self_id, self_name),
        MsgContent::Image { path: Some(path) } if media::is_url(path) => {
            vec![segment("image", json!({ "file": path, "url": path }))]
        }
        MsgContent::Emoji { url: Some(url) } if media::is_url(url) => {
            vec![segment("image", json!({ "file": url, "url": url }))]
        }
        MsgContent::App {
            title,
            app_type: Some(57),
            ..
        } => text_segments(
            title.as_deref().unwrap_or(""),
            &message.content,
            self_id,
            self_name,
        ),
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

fn text_segments(text: &str, source: &str, self_id: &str, self_name: &str) -> Vec<Value> {
    let mut ids = xml_text(source, "atuserlist")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
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
        parsed: crate::db::parse_msg_content(quote.kind, &quote.content),
        msg_type: quote.kind,
        talker: quote.sender.clone(),
        talker_display_name: quote.displayname.clone(),
        chat: chat_id.to_string(),
        chat_display_name: String::new(),
        is_self: false,
    };
    let segments = message_segments(&quoted, "", "");
    let mut value = json!({
        "message_id": quote.svrid, "real_id": quote.svrid, "time": quote.createtime,
        "message_type": if is_group { "group" } else { "private" },
        "sender": { "user_id": quote.sender, "nickname": quote.displayname },
        "message": segments, "raw_message": cq_message(&segments),
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
            "image" => outgoing
                .parts
                .push(OutgoingPart::Image(required_id(data, "file")?)),
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

fn ensure_sent(response: &api::SendResponse) -> Result<(), String> {
    if response.sent {
        Ok(())
    } else {
        Err(response.message.clone())
    }
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
