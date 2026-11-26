use quick_xml::{events::Event, Reader};
use serde_json::{json, Value};
use std::collections::HashMap;

use super::{segment, xml_text};

const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;
const MAX_NODE_TEXT_BYTES: usize = 4 * 1024;
const MAX_NODES: usize = 100;
const MAX_CONTEXT_BYTES: usize = 24 * 1024;

pub(super) struct ForwardRecord {
    title: String,
    nodes: Vec<ForwardNode>,
    truncated: bool,
}

struct ForwardNode {
    sender: String,
    timestamp: i64,
    display_time: String,
    text: String,
    image: Option<ForwardImage>,
}

pub(super) struct ForwardImage {
    pub index: usize,
    pub checksum: Option<String>,
}

pub(crate) fn has_forward_record(content: &str) -> bool {
    ForwardRecord::parse(content).is_some()
}

impl ForwardRecord {
    pub(super) fn parse(content: &str) -> Option<Self> {
        if content.len() > MAX_RECORD_BYTES {
            return None;
        }
        let app = raw_element(content, b"appmsg")?;
        let raw_record = raw_element(&app, b"recorditem")?;
        let decoded;
        let record = if raw_record.trim_start().starts_with("<recordinfo") {
            raw_record.as_ref()
        } else {
            decoded = xml_text(&app, "recorditem")?;
            decoded.as_str()
        };
        let info = raw_element(record, b"recordinfo")?;
        let list = raw_element(&info, b"datalist")?;
        let title = xml_text(record, "title")
            .or_else(|| xml_text(&app, "title"))
            .map(|text| bounded(text.trim(), 240).to_owned())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "聊天记录".to_string());
        let mut record = Self {
            title,
            nodes: Vec::new(),
            truncated: false,
        };
        let mut reader = Reader::from_str(&list);
        let mut bytes = 0;
        loop {
            match reader.read_event().ok()? {
                Event::Start(item) if item.name().as_ref() == b"dataitem" => {
                    let kind = item
                        .try_get_attribute("datatype")
                        .ok()
                        .flatten()
                        .and_then(|attribute| attribute.unescape_value().ok()?.parse::<u32>().ok());
                    let body = reader.read_text(item.name()).ok()?;
                    let node = ForwardNode::parse(kind, &body, record.nodes.len());
                    bytes += node.sender.len() + node.display_time.len() + node.text.len() + 16;
                    if record.nodes.len() == MAX_NODES || bytes > MAX_CONTEXT_BYTES {
                        record.truncated = true;
                        break;
                    }
                    record.nodes.push(node);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        (!record.nodes.is_empty()).then_some(record)
    }

    pub(super) fn images(&self) -> impl Iterator<Item = &ForwardImage> {
        self.nodes.iter().filter_map(|node| node.image.as_ref())
    }

    pub(super) fn context(&self, images: &HashMap<usize, Value>) -> String {
        let mut text = format!("[合并转发｜{}]", self.title);
        let mut image_number = 0;
        for (index, node) in self.nodes.iter().enumerate() {
            text.push('\n');
            if !node.display_time.is_empty() {
                text.push_str(&node.display_time);
                text.push(' ');
            }
            text.push_str(&node.sender);
            text.push_str("：");
            text.push_str(&node.text);
            if images.contains_key(&index) {
                image_number += 1;
                text.push_str(&format!("（图 {image_number}）"));
            }
        }
        if self.truncated {
            text.push_str("\n[其余消息已省略]");
        }
        text.push_str("\n[合并转发结束]");
        text
    }

    pub(super) fn onebot(&self, images: &HashMap<usize, Value>) -> Value {
        let nodes = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let mut content = vec![segment("text", json!({ "text": node.text }))];
                content.extend(images.get(&index).cloned());
                segment(
                    "node",
                    json!({
                        "user_id": "0",
                        "nickname": node.sender,
                        "time": node.timestamp,
                        "content": content,
                    }),
                )
            })
            .collect::<Vec<_>>();
        json!({
            "message": nodes,
            "title": self.title,
            "truncated": self.truncated,
        })
    }
}

impl ForwardNode {
    fn parse(kind: Option<u32>, body: &str, index: usize) -> Self {
        let sender = xml_text(body, "sourcename")
            .map(|text| single_line(&text, 120))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "未知发言人".to_string());
        let timestamp = xml_text(body, "srcMsgCreateTime")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        let display_time = xml_text(body, "sourcetime")
            .map(|text| single_line(&text, 64))
            .unwrap_or_default();
        let description = xml_text(body, "datadesc")
            .filter(|text| !text.trim().is_empty())
            .or_else(|| xml_text(body, "datatitle").filter(|text| !text.trim().is_empty()));
        let label = match kind {
            Some(1) => "",
            Some(2) => "[图片]",
            Some(3) => "[语音]",
            Some(4) => "[视频]",
            Some(5) => "[链接]",
            Some(8) => "[文件]",
            Some(17) => "[内层聊天记录]",
            _ => "[其他消息]",
        };
        let mut text = label.to_owned();
        if let Some(description) = description {
            if !text.is_empty() {
                text.push(' ');
            }
            let description = description.trim();
            text.push_str(bounded(description, MAX_NODE_TEXT_BYTES));
            if description.len() > MAX_NODE_TEXT_BYTES {
                text.push('…');
            }
        }
        if text.is_empty() {
            text.push_str("[空消息]");
        }
        Self {
            sender,
            timestamp,
            display_time,
            text,
            image: (kind == Some(2)).then(|| ForwardImage {
                index,
                checksum: xml_text(body, "fullmd5")
                    .map(|value| value.trim().to_ascii_lowercase())
                    .filter(|value| super::inbound_media::is_md5(value)),
            }),
        }
    }
}

fn raw_element<'a>(xml: &'a str, tag: &[u8]) -> Option<std::borrow::Cow<'a, str>> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event().ok()? {
            Event::Start(element) if element.name().as_ref() == tag => {
                return reader.read_text(element.name()).ok();
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

fn bounded(value: &str, limit: usize) -> &str {
    &value[..value.floor_char_boundary(limit.min(value.len()))]
}

fn single_line(value: &str, limit: usize) -> String {
    bounded(value.trim(), limit).replace(['\r', '\n'], " ")
}
