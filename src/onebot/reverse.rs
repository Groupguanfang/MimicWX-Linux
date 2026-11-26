use super::{cq_message, handle_socket, parse_cq, required_id, OneBotRuntime, Socket, CACHE_LIMIT};
use crate::api::AppState;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpStream, sync::broadcast};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{header, HeaderValue, Request},
    },
    MaybeTlsStream, WebSocketStream,
};
use tracing::{info, warn};

pub(super) type WebSocketClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Deserialize)]
pub struct ReverseConfig {
    url: String,
    token: Option<String>,
    #[serde(default)]
    numeric_ids: bool,
    #[serde(default)]
    original_images: bool,
}

impl ReverseConfig {
    pub fn spawn(
        self,
        state: Arc<AppState>,
        runtime: Arc<OneBotRuntime>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        let peer = Arc::new(Peer::new(
            runtime.self_id.clone(),
            self.numeric_ids,
            self.original_images,
        ));
        tokio::spawn(async move {
            let mut delay = Duration::from_secs(2);
            loop {
                let request = match self.request(&peer) {
                    Ok(request) => request,
                    Err(error) => {
                        warn!("OneBot 反向连接配置无效: {error}");
                        return;
                    }
                };
                let host = request.uri().authority().unwrap().to_string();
                let connection = tokio::select! {
                    _ = shutdown.recv() => return,
                    result = tokio::time::timeout(Duration::from_secs(10), connect_async(request)) => result,
                };
                match connection {
                    Ok(Ok((socket, _))) => {
                        info!(%host, "OneBot 反向 WebSocket 已连接");
                        delay = Duration::from_secs(2);
                        tokio::select! {
                            _ = shutdown.recv() => return,
                            _ = handle_socket(state.clone(), runtime.clone(), Socket::Reverse(Box::new(socket)), Some(peer.clone())) => {}
                        }
                        warn!(%host, "OneBot 反向 WebSocket 已断开，将重新连接");
                    }
                    Ok(Err(error)) => warn!(%host, %error, "OneBot 反向 WebSocket 连接失败"),
                    Err(_) => warn!(%host, "OneBot 反向 WebSocket 连接超时"),
                }
                tokio::select! {
                    _ = shutdown.recv() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        });
    }

    fn request(&self, peer: &Peer) -> Result<Request<()>, String> {
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|_| "反向地址必须是有效的 ws:// 或 wss:// URL".to_string())?;
        if !matches!(request.uri().scheme_str(), Some("ws" | "wss"))
            || request.uri().authority().is_none()
        {
            return Err("反向地址必须是有效的 ws:// 或 wss:// URL".into());
        }
        let headers = request.headers_mut();
        let self_id = if peer.numeric_ids {
            alias_number(&peer.wxid).to_string()
        } else {
            peer.wxid.clone()
        };
        headers.insert(
            "x-self-id",
            HeaderValue::from_str(&self_id).map_err(|_| "机器人 ID 不能用作 WebSocket 请求头")?,
        );
        headers.insert("x-client-role", HeaderValue::from_static("Universal"));
        headers.insert(header::USER_AGENT, HeaderValue::from_static("OneBot/11"));
        if let Some(token) = self.token.as_deref().filter(|token| !token.is_empty()) {
            if token.chars().any(char::is_whitespace) {
                return Err("反向 WebSocket Token 不能含空白字符".into());
            }
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| "反向 WebSocket Token 格式无效")?;
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
        }
        Ok(request)
    }
}

#[derive(Default)]
struct Aliases {
    native: HashMap<u64, String>,
    order: VecDeque<u64>,
}

impl Aliases {
    fn remember(&mut self, id: &str) -> Option<u64> {
        let number = alias_number(id);
        if let Some(existing) = self.native.get(&number) {
            return (existing == id).then_some(number);
        }
        self.native.insert(number, id.into());
        self.order.push_back(number);
        if self.order.len() > CACHE_LIMIT {
            if let Some(oldest) = self.order.pop_front() {
                self.native.remove(&oldest);
            }
        }
        Some(number)
    }
}

fn alias_number(id: &str) -> u64 {
    let hash = md5::compute(id);
    let value = u64::from_be_bytes(hash.0[..8].try_into().unwrap());
    (1 << 52) | (value & ((1 << 52) - 1))
}

fn group_number(id: &str) -> Option<u64> {
    let prefix = id.strip_suffix("@chatroom")?;
    let number = prefix.parse::<u64>().ok()?;
    (number < (1 << 53) && number.to_string() == prefix).then_some(number)
}

#[derive(Default)]
struct Ids {
    users: Aliases,
    messages: Aliases,
}

pub(super) struct Peer {
    wxid: String,
    numeric_ids: bool,
    original_images: bool,
    ids: Mutex<Ids>,
}

impl Peer {
    fn new(wxid: String, numeric_ids: bool, original_images: bool) -> Self {
        Self {
            wxid,
            numeric_ids,
            original_images,
            ids: Mutex::default(),
        }
    }

    pub(super) fn self_id(&self) -> Value {
        if self.numeric_ids {
            json!(alias_number(&self.wxid))
        } else {
            json!(self.wxid)
        }
    }

    pub(super) fn event(&self, payload: String) -> Option<String> {
        if !self.numeric_ids && !self.original_images {
            return Some(payload);
        }
        let mut event: Value = serde_json::from_str(&payload).ok()?;
        if self.original_images {
            use_original_image_urls(&mut event);
        }
        if self.numeric_ids {
            Self::encode(&mut event, &mut self.ids.lock().unwrap());
        }
        Some(event.to_string())
    }

    pub(super) fn prepare_action(&self, action: &str, params: &mut Value) -> Result<(), String> {
        if !self.numeric_ids {
            return Ok(());
        }
        if params.get("group_id").is_some() {
            let number = required_id(params, "group_id")?
                .parse::<u64>()
                .map_err(|_| "无效的 OneBot 群 ID")?;
            params["group_id"] = json!(format!("{number}@chatroom"));
        }
        let ids = self.ids.lock().unwrap();
        if params.get("user_id").is_some() {
            let id = required_id(params, "user_id")?;
            params["user_id"] = json!(self.decode_user(&ids, &id)?);
        }
        if action == "get_msg" {
            let id = required_id(params, "message_id")?;
            params["message_id"] = json!(decode_alias(&ids.messages, &id, "消息")?);
        }
        if matches!(action, "send_msg" | "send_group_msg" | "send_private_msg") {
            if let Some(text) = params.get("message").and_then(Value::as_str) {
                if params.get("auto_escape").and_then(Value::as_bool) != Some(true) {
                    params["message"] = json!(parse_cq(text)?);
                }
            }
            if let Some(segments) = params.get_mut("message").and_then(Value::as_array_mut) {
                for segment in segments {
                    let data = &segment["data"];
                    match segment["type"].as_str() {
                        Some("at") => {
                            let id = required_id(data, "qq")?;
                            if id != "all" {
                                segment["data"]["qq"] = json!(self.decode_user(&ids, &id)?);
                            }
                        }
                        Some("reply") => {
                            let id = required_id(data, "id")?;
                            segment["data"]["id"] =
                                json!(decode_alias(&ids.messages, &id, "消息")?);
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn decode_user(&self, ids: &Ids, id: &str) -> Result<String, String> {
        if id == alias_number(&self.wxid).to_string() {
            return Ok(self.wxid.clone());
        }
        decode_alias(&ids.users, id, "用户")
    }

    pub(super) fn action_result(&self, action: &str, mut data: Value) -> Value {
        if self.original_images {
            use_original_image_urls(&mut data);
        }
        if self.numeric_ids {
            if action == "get_msg" {
                data["self_id"] = json!(self.wxid);
                if data.get("user_id").is_none() {
                    data["user_id"] = data["sender"]["user_id"].clone();
                }
            }
            Self::encode(&mut data, &mut self.ids.lock().unwrap());
        }
        data
    }

    fn encode(payload: &mut Value, ids: &mut Ids) {
        if let Some(entries) = payload.as_array_mut() {
            for entry in entries {
                Self::encode(entry, ids);
            }
            return;
        }
        for key in ["self_id", "user_id", "target_id"] {
            if let Some(value) = payload.get_mut(key) {
                encode_alias(value, &mut ids.users);
            }
        }
        if let Some(user) = payload
            .get_mut("sender")
            .and_then(|sender| sender.get_mut("user_id"))
        {
            encode_alias(user, &mut ids.users);
        }
        for key in ["group_id", "chat_id"] {
            if let Some(value) = payload.get_mut(key) {
                if let Some(number) = value.as_str().and_then(group_number) {
                    *value = json!(number);
                } else if key == "chat_id" {
                    encode_alias(value, &mut ids.users);
                }
            }
        }
        if let Some(id) = payload.get_mut("message_id") {
            encode_alias(id, &mut ids.messages);
        }
        if let Some(segments) = payload.get_mut("message").and_then(Value::as_array_mut) {
            for segment in segments.iter_mut() {
                match segment["type"].as_str() {
                    Some("at") if segment["data"]["qq"] != "all" => {
                        encode_alias(&mut segment["data"]["qq"], &mut ids.users);
                    }
                    Some("reply") => {
                        encode_alias(&mut segment["data"]["id"], &mut ids.messages);
                    }
                    _ => {}
                }
            }
            payload["raw_message"] = json!(cq_message(segments));
        }
        if let Some(history) = payload.get_mut("messages") {
            Self::encode(history, ids);
        }
    }
}

fn use_original_image_urls(payload: &mut Value) {
    if let Some(segments) = payload.get_mut("message").and_then(Value::as_array_mut) {
        let mut changed = false;
        for segment in segments.iter_mut() {
            if segment["type"] != "image" || segment["data"]["sticker"] == true {
                continue;
            }
            let data = &mut segment["data"];
            let (Some(id), Some(url)) = (data["file"].as_str(), data["url"].as_str()) else {
                continue;
            };
            if id.len() == 64
                && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && url.ends_with(&format!("?media={id}"))
            {
                data["url"] = json!(format!("{url}&original=1"));
                changed = true;
            }
        }
        if changed {
            payload["raw_message"] = json!(cq_message(segments));
        }
    }
    if let Some(history) = payload.get_mut("messages").and_then(Value::as_array_mut) {
        for message in history {
            use_original_image_urls(message);
        }
    }
}

fn encode_alias(value: &mut Value, aliases: &mut Aliases) {
    if let Some(alias) = value.as_str().and_then(|id| aliases.remember(id)) {
        *value = json!(alias);
    }
}

fn decode_alias(aliases: &Aliases, value: &str, kind: &str) -> Result<String, String> {
    value
        .parse::<u64>()
        .ok()
        .and_then(|id| aliases.native.get(&id).cloned())
        .ok_or_else(|| format!("未找到{kind} ID 的微信映射"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configures_authenticated_universal_handshake() {
        let config: ReverseConfig = toml::from_str(
            r#"url = "wss://astrbot.example.org/ws"
token = "test-token"
numeric_ids = true"#,
        )
        .unwrap();
        let peer = Peer::new("wxid_bot".into(), true, false);
        let request = config.request(&peer).unwrap();
        assert_eq!(request.uri().path(), "/ws");
        assert_eq!(request.headers()["x-client-role"], "Universal");
        assert_eq!(request.headers()["x-self-id"], peer.self_id().to_string());
        assert_eq!(request.headers()["authorization"], "Bearer test-token");
        assert!(request.headers()["authorization"].is_sensitive());
    }

    #[test]
    fn maps_group_mentions_and_fast_message_ids_in_both_directions() {
        let peer = Peer::new("wxid_bot".into(), true, false);
        let event = json!({
            "post_type": "message", "self_id": "wxid_bot",
            "message_type": "group", "group_id": "123@chatroom",
            "user_id": "wxid_friend", "sender": {"user_id": "wxid_friend"},
            "message_id": "fast_123@chatroom_8",
            "message": [
                {"type": "at", "data": {"qq": "wxid_bot"}},
                {"type": "reply", "data": {"id": "7327441047445000023"}},
                {"type": "text", "data": {"text": "你好"}}
            ],
        });
        let wire: Value = serde_json::from_str(&peer.event(event.to_string()).unwrap()).unwrap();
        assert_eq!(wire["group_id"], 123);
        assert_eq!(wire["user_id"], wire["sender"]["user_id"]);
        assert_eq!(wire["self_id"], wire["message"][0]["data"]["qq"]);
        assert!(wire["message_id"].is_u64());
        assert!(!wire["raw_message"].as_str().unwrap().contains("wxid_"));

        let mut quote = json!({"message_id": wire["message"][1]["data"]["id"]});
        peer.prepare_action("get_msg", &mut quote).unwrap();
        assert_eq!(quote["message_id"], "7327441047445000023");

        let mut send = json!({"group_id": 123, "message": format!(
            "[CQ:reply,id={}][CQ:at,qq={}] 你好",
            wire["message_id"], wire["user_id"],
        )});
        peer.prepare_action("send_group_msg", &mut send).unwrap();
        assert_eq!(send["group_id"], "123@chatroom");
        assert_eq!(send["message"][0]["data"]["id"], "fast_123@chatroom_8");
        assert_eq!(send["message"][1]["data"]["qq"], "wxid_friend");
    }

    #[test]
    fn reverse_native_connection_passes_through_private_messages() {
        let peer = Peer::new("wxid_bot".into(), false, false);
        let event = json!({
            "post_type": "message", "message_type": "private", "user_id": "wxid_friend",
        });
        assert_eq!(peer.event(event.to_string()).unwrap(), event.to_string());
        let mut send = json!({"user_id": "wxid_friend"});
        peer.prepare_action("send_private_msg", &mut send).unwrap();
        assert_eq!(send["user_id"], "wxid_friend");
    }

    #[test]
    fn original_urls_apply_to_images_in_events_and_history_but_spare_stickers() {
        let id = "f".repeat(64);
        let url = format!("https://example.test/onebot/ws?media={id}");
        let source = json!({
            "message": [
                {"type": "image", "data": {"file": id, "url": url}},
                {"type": "image", "data": {"file": id, "url": url, "sticker": true}},
            ],
        });
        let peer = Peer::new("wxid_bot".into(), true, true);
        let wire: Value = serde_json::from_str(&peer.event(source.to_string()).unwrap()).unwrap();
        let expected = format!("{url}&original=1");
        assert_eq!(wire["message"][0]["data"]["url"], expected);
        assert_eq!(wire["message"][1]["data"]["url"], url);
        let quoted = peer.action_result("get_msg", source.clone());
        assert_eq!(quoted["message"][0]["data"]["url"], expected);
        let history = peer.action_result("get_group_msg_history", json!({"messages": [source]}));
        assert_eq!(
            history["messages"][0]["message"][0]["data"]["url"],
            expected
        );
    }

    #[test]
    fn reverse_numeric_connection_maps_private_messages_and_replies() {
        let peer = Peer::new("wxid_bot".into(), true, false);
        let event = json!({
            "post_type": "message", "message_type": "private", "sub_type": "friend",
            "self_id": "wxid_bot", "user_id": "wxid_friend", "chat_id": "wxid_friend",
            "sender": {"user_id": "wxid_friend"}, "message_id": "dm_123",
            "message": [{"type": "text", "data": {"text": "/help"}}],
        });
        let wire: Value = serde_json::from_str(&peer.event(event.to_string()).unwrap()).unwrap();
        assert_eq!(wire["user_id"], alias_number("wxid_friend"));
        assert_eq!(wire["chat_id"], wire["user_id"]);
        assert_eq!(wire["raw_message"], "/help");

        let mut direct_send = json!({"user_id": wire["user_id"]});
        peer.prepare_action("send_private_msg", &mut direct_send)
            .unwrap();
        assert_eq!(direct_send["user_id"], "wxid_friend");

        let mut generic_send = json!({"user_id": wire["user_id"], "message_type": "private"});
        peer.prepare_action("send_msg", &mut generic_send).unwrap();
        assert_eq!(generic_send["user_id"], "wxid_friend");

        let mut lookup = json!({"message_id": wire["message_id"]});
        peer.prepare_action("get_msg", &mut lookup).unwrap();
        assert_eq!(lookup["message_id"], "dm_123");
        let cached = peer.action_result("get_msg", event);
        assert_eq!(cached["chat_id"], wire["user_id"]);
    }
}
