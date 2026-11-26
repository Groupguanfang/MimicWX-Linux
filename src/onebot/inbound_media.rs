use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use axum::body::Bytes;
use quick_xml::{events::Event, Reader};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

use super::media;

mod local;

const MAX_ENTRIES: usize = 4096;
const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;
pub(super) const ORIGINAL_UNAVAILABLE: &str = "微信尚未提供这条消息的原图";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ImageQuality {
    Thumbnail,
    Preview,
    Full,
    Original,
}

impl ImageQuality {
    fn is_full(self) -> bool {
        self >= Self::Full
    }
}

#[derive(Clone)]
struct Source {
    url: String,
    key: Option<[u8; 16]>,
    checksum: Option<String>,
    quality: ImageQuality,
}

#[derive(Clone)]
struct Attachment {
    sources: Vec<Source>,
    emoji_md5: Option<String>,
    original_md5: Option<String>,
    standard_is_full: bool,
    local_image: Option<(String, i64)>,
    local_voice: Option<(String, i64)>,
}

#[derive(Clone)]
pub(super) struct Image {
    pub bytes: Bytes,
    pub content_type: &'static str,
    pub quality: ImageQuality,
}

struct Entry {
    attachment: Attachment,
    image: Option<Image>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
    bytes: usize,
}

impl Cache {
    fn trim(&mut self) {
        while self.entries.len() > MAX_ENTRIES || self.bytes > MAX_CACHE_BYTES {
            let Some(id) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&id) {
                self.bytes -= entry.image.map_or(0, |image| image.bytes.len());
            }
        }
    }
}

pub(super) struct Store {
    base_url: String,
    cache: Mutex<Cache>,
    downloads: Semaphore,
    database: Option<Arc<crate::db::DbManager>>,
}

impl Store {
    pub fn new(base_url: String) -> Result<Self, String> {
        let uri = base_url
            .parse::<axum::http::Uri>()
            .map_err(|_| "media_url 无效")?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri.authority().is_none()
            || uri
                .authority()
                .is_some_and(|authority| authority.as_str().contains('@'))
            || uri.query().is_some()
            || base_url.contains('#')
        {
            return Err("media_url 必须是不含凭据、查询参数或片段的 HTTP(S) OneBot 入口".into());
        }
        Ok(Self {
            base_url,
            cache: Mutex::new(Cache::default()),
            downloads: Semaphore::new(4),
            database: None,
        })
    }

    pub fn with_database(mut self, database: Option<Arc<crate::db::DbManager>>) -> Self {
        self.database = database;
        self
    }

    #[cfg(test)]
    pub async fn register(&self, kind: i64, content: &str) -> Option<String> {
        self.register_in_context(kind, content, None).await
    }

    pub async fn register_message(
        &self,
        kind: i64,
        content: &str,
        chat: &str,
        server_id: i64,
    ) -> Option<String> {
        let context = (matches!(kind & 0xffff, 3 | 34) && server_id > 0 && self.database.is_some())
            .then(|| (chat.to_string(), server_id));
        self.register_in_context(kind, content, context).await
    }

    async fn register_in_context(
        &self,
        kind: i64,
        content: &str,
        context: Option<(String, i64)>,
    ) -> Option<String> {
        let mut attachment = Attachment::parse(kind, content).or_else(|| {
            context.is_some().then(|| Attachment {
                sources: Vec::new(),
                emoji_md5: None,
                original_md5: None,
                standard_is_full: false,
                local_image: None,
                local_voice: None,
            })
        })?;
        if context.is_none() && attachment.sources.is_empty() && attachment.emoji_md5.is_none() {
            return None;
        }
        if kind & 0xffff == 34 {
            attachment.local_voice = context;
        } else {
            attachment.local_image = context;
        }
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).ok()?;
        let id = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut cache = self.cache.lock().await;
        cache.entries.insert(
            id.clone(),
            Entry {
                attachment,
                image: None,
            },
        );
        cache.order.push_back(id.clone());
        cache.trim();
        Some(id)
    }

    pub fn url(&self, id: &str) -> String {
        format!("{}?media={id}", self.base_url)
    }

    pub async fn contains(&self, id: &str) -> bool {
        self.cache.lock().await.entries.contains_key(id)
    }

    pub async fn image(&self, id: &str) -> Result<Image, String> {
        self.resolve_image(id, false).await
    }

    pub async fn original(&self, id: &str) -> Result<Image, String> {
        self.resolve_image(id, true).await
    }

    async fn resolve_image(&self, id: &str, original_only: bool) -> Result<Image, String> {
        let mut attachment = {
            let cache = self.cache.lock().await;
            let entry = cache
                .entries
                .get(id)
                .ok_or("媒体链接已失效，请重新发送或引用表情")?;
            if let Some(image) = &entry.image {
                if !original_only || image.quality.is_full() {
                    return Ok(image.clone());
                }
            }
            entry.attachment.clone()
        };
        let _permit = self
            .downloads
            .acquire()
            .await
            .map_err(|_| "媒体下载已停止")?;
        if let Some((chat, server_id)) = &attachment.local_voice {
            let database = self.database.as_ref().ok_or("语音数据库尚未就绪")?;
            let mut audio = None;
            for attempt in 0..6 {
                audio = database
                    .voice_data(chat, *server_id)
                    .await
                    .map_err(|_| "读取微信本地语音失败")?;
                if audio.is_some() {
                    break;
                }
                if attempt < 5 {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
            let audio = audio.ok_or("微信尚未下载这条语音的音频")?;
            let content_type = voice_content_type(&audio).ok_or("不支持的微信语音格式")?;
            let image = Image {
                bytes: Bytes::from(audio),
                content_type,
                quality: ImageQuality::Original,
            };
            return Ok(self.cache_image(id, image).await);
        }
        if attachment.sources.is_empty() {
            if let (Some(database), Some(checksum)) = (&self.database, &attachment.emoji_md5) {
                let metadata = database
                    .emoji_media(checksum)
                    .await
                    .map_err(|_| "读取微信表情元数据失败".to_string())?;
                if let Some(metadata) = metadata {
                    if let Some(resolved) = Attachment::from_attributes("emoji", &metadata) {
                        attachment.sources = resolved.sources;
                    }
                }
            }
        }
        let metadata = match (&self.database, &attachment.local_image) {
            (Some(database), Some((chat, server_id))) => database
                .local_image_metadata(chat, *server_id)
                .await
                .map_err(|_| "读取本地图片元数据失败")?,
            _ => None,
        };
        if let Some(metadata) = &metadata {
            if let Some(authoritative) = Attachment::parse(3, &metadata.content) {
                attachment.sources = authoritative.sources;
                attachment.original_md5 = authoritative.original_md5;
                attachment.standard_is_full = authoritative.standard_is_full;
            }
        }
        let mut last_error = "没有可用的媒体来源".to_string();
        for source in attachment
            .sources
            .iter()
            .filter(|source| source.quality.is_full())
        {
            match download(source).await {
                Ok(image) => return Ok(self.cache_image(id, image).await),
                Err(error) => last_error = error,
            }
        }
        let mut best = None;
        if let Some(metadata) = metadata {
            match local::image(
                metadata,
                attachment.original_md5.as_deref(),
                attachment.standard_is_full,
                original_only,
            )
            .await
            {
                Ok(image) if image.quality.is_full() => {
                    return Ok(self.cache_image(id, image).await);
                }
                Ok(image) => best = Some(image),
                Err(error) => last_error = error,
            }
        }
        if original_only {
            return Err(ORIGINAL_UNAVAILABLE.into());
        }
        for source in attachment
            .sources
            .iter()
            .filter(|source| !source.quality.is_full())
        {
            if best
                .as_ref()
                .is_some_and(|image| image.quality >= source.quality)
            {
                continue;
            }
            match download(source).await {
                Ok(image) => best = Some(image),
                Err(error) => last_error = error,
            }
        }
        let image = best.ok_or(last_error)?;
        if attachment.local_image.is_none() {
            return Ok(self.cache_image(id, image).await);
        }
        Ok(image)
    }

    async fn cache_image(&self, id: &str, image: Image) -> Image {
        let mut cache = self.cache.lock().await;
        if let Some(entry) = cache.entries.get_mut(id) {
            let previous = entry.image.replace(image.clone());
            cache.bytes =
                cache.bytes - previous.map_or(0, |old| old.bytes.len()) + image.bytes.len();
            cache.trim();
        }
        image
    }
}

async fn download(source: &Source) -> Result<Image, String> {
    let mut bytes = media::download_bytes(&source.url).await?;
    if let Some(key) = source.key {
        bytes = decrypt(&bytes, &key)?;
    }
    if let Some(expected) = &source.checksum {
        let actual = format!("{:x}", md5::compute(&bytes));
        if !actual.eq_ignore_ascii_case(expected) {
            return Err("媒体 MD5 校验失败".into());
        }
    }
    let content_type = media::image_content_type(&bytes).ok_or_else(|| {
        if bytes.starts_with(b"wxgf") {
            "媒体是 WXGF，仍需转码".to_string()
        } else {
            "下载或解密结果不是支持的图片".to_string()
        }
    })?;
    Ok(Image {
        bytes: bytes.into(),
        content_type,
        quality: source.quality,
    })
}

impl Attachment {
    fn parse(kind: i64, content: &str) -> Option<Self> {
        let tag = match kind & 0xffff {
            47 => "emoji",
            3 => "img",
            _ => return None,
        };
        let attributes = attributes(content, tag)?;
        Self::from_attributes(tag, &attributes)
    }

    fn from_attributes(tag: &str, attributes: &HashMap<String, String>) -> Option<Self> {
        let mut sources = Vec::new();
        let checksum = attributes.get("md5").filter(|value| is_md5(value)).cloned();
        let hd_md5 = attributes.get("hdmd5").filter(|value| is_md5(value));
        let distinct_hd_md5 = hd_md5.is_some_and(|hd| {
            checksum
                .as_ref()
                .is_none_or(|standard| !hd.eq_ignore_ascii_case(standard))
        });
        let standard_is_full = tag == "img" && complete_standard_image(attributes, distinct_hd_md5);
        let original_md5 = (tag == "img")
            .then(|| hd_md5.or_else(|| checksum.as_ref().filter(|_| standard_is_full)))
            .flatten()
            .map(|value| value.to_ascii_lowercase());
        let emoji_md5 = if tag == "emoji" {
            checksum.clone()
        } else {
            None
        };
        let key = attributes.get("aeskey").and_then(|value| parse_key(value));
        if tag == "emoji" {
            add_source(
                &mut sources,
                &attributes,
                "cdnurl",
                None,
                checksum.clone(),
                ImageQuality::Original,
            );
            add_source(
                &mut sources,
                &attributes,
                "externurl",
                None,
                attributes
                    .get("externmd5")
                    .filter(|value| is_md5(value))
                    .cloned(),
                ImageQuality::Original,
            );
            if let Some(key) = key {
                add_source(
                    &mut sources,
                    &attributes,
                    "encrypturl",
                    Some(key),
                    checksum,
                    ImageQuality::Original,
                );
            }
            add_source(
                &mut sources,
                attributes,
                "thumburl",
                None,
                None,
                ImageQuality::Thumbnail,
            );
        } else {
            let big = image_url_attribute(attributes, "cdnbigimgurl");
            let mid = image_url_attribute(attributes, "cdnmidimgurl");
            let thumb = image_url_attribute(attributes, "cdnthumburl");
            if big.is_some() && big != mid && big != thumb {
                add_source(
                    &mut sources,
                    attributes,
                    "cdnbigimgurl",
                    None,
                    hd_md5.cloned(),
                    ImageQuality::Original,
                );
            }
            if let Some(key) = key {
                let quality = if standard_is_full {
                    ImageQuality::Full
                } else if distinct_hd_md5 {
                    ImageQuality::Original
                } else {
                    ImageQuality::Preview
                };
                add_source(
                    &mut sources,
                    attributes,
                    "encrypturl",
                    Some(key),
                    original_md5.clone().or(checksum),
                    quality,
                );
            }
            add_source(
                &mut sources,
                attributes,
                "cdnmidimgurl",
                None,
                None,
                if mid.is_some() && mid == thumb {
                    ImageQuality::Thumbnail
                } else if standard_is_full {
                    ImageQuality::Full
                } else {
                    ImageQuality::Preview
                },
            );
            add_source(
                &mut sources,
                attributes,
                "cdnthumburl",
                None,
                None,
                ImageQuality::Thumbnail,
            );
        }
        (tag == "img" || !sources.is_empty() || emoji_md5.is_some()).then_some(Self {
            sources,
            emoji_md5,
            original_md5,
            standard_is_full,
            local_image: None,
            local_voice: None,
        })
    }
}

fn image_url_attribute<'a>(attributes: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    attributes
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

fn complete_standard_image(
    attributes: &HashMap<String, String>,
    distinct_hd_checksum: bool,
) -> bool {
    let size = |key: &str| {
        attributes
            .get(key)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let length = size("length");
    if length <= size("cdnthumblength") || distinct_hd_checksum {
        return false;
    }
    let hd_length = size("hdlength");
    if let Some(big) = image_url_attribute(attributes, "cdnbigimgurl") {
        return image_url_attribute(attributes, "cdnmidimgurl") == Some(big)
            && (hd_length == 0 || hd_length == length);
    }
    hd_length == 0 && size("cdnhdwidth") == 0 && size("cdnhdheight") == 0
}

fn voice_content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x02#!SILK_V3") || bytes.starts_with(b"#!SILK_V3") {
        Some("audio/silk")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        Some("audio/wav")
    } else if bytes.starts_with(b"#!AMR\n") || bytes.starts_with(b"#!AMR-WB\n") {
        Some("audio/amr")
    } else {
        None
    }
}

fn add_source(
    sources: &mut Vec<Source>,
    attributes: &HashMap<String, String>,
    name: &str,
    key: Option<[u8; 16]>,
    checksum: Option<String>,
    quality: ImageQuality,
) {
    if let Some(url) = attributes.get(name).filter(|url| media::is_url(url)) {
        if !sources
            .iter()
            .any(|source| source.url == *url && source.key == key)
        {
            sources.push(Source {
                url: url.clone(),
                key,
                checksum,
                quality,
            });
        }
    }
}

fn attributes(content: &str, tag: &str) -> Option<HashMap<String, String>> {
    let mut reader = Reader::from_str(content);
    loop {
        match reader.read_event() {
            Ok(Event::Start(element) | Event::Empty(element))
                if element.name().as_ref() == tag.as_bytes() =>
            {
                let mut result = HashMap::new();
                for attribute in element.attributes() {
                    let attribute = attribute.ok()?;
                    let name = std::str::from_utf8(attribute.key.as_ref())
                        .ok()?
                        .to_string();
                    let value = attribute
                        .decode_and_unescape_value(reader.decoder())
                        .ok()?
                        .into_owned();
                    result.insert(name, value);
                }
                return Some(result);
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

fn is_md5(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_key(value: &str) -> Option<[u8; 16]> {
    if !is_md5(value) {
        return None;
    }
    let mut key = [0u8; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        key[index] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(key)
}

fn decrypt(bytes: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    let mut plain = bytes.to_vec();
    let decoded = cbc::Decryptor::<aes::Aes128>::new(key.into(), key.into())
        .decrypt_padded_mut::<Pkcs7>(&mut plain)
        .map_err(|_| "表情 AES-CBC 解密失败：密钥、长度或填充无效")?;
    Ok(decoded.to_vec())
}

#[cfg(test)]
mod tests;
