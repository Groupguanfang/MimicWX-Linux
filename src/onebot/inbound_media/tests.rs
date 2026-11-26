use super::*;
use axum::{http::StatusCode, routing::get, Router};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const CHECKSUM: &str = "d89746888da2d9510b64a9f031eaecd5";
const KEY: &str = "000102030405060708090a0b0c0d0e0f";
const ENCRYPTED: &str = "664b2ab7cccc80d51e1790760cdffab414f2242085d877110d6ab8c124dc46cd2ce50ed79425490132d73fbde648475e";
const PLAIN: &str =
    "47494638396101000100800000000000ffffff21f90401000000002c000000000100010000020144003b";

fn from_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

struct Fixture {
    url: String,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture() -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let app = Router::new()
        .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
        .route(
            "/encrypted",
            get(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    from_hex(ENCRYPTED)
                }
            }),
        )
        .route("/thumb", get(|| async { from_hex(PLAIN) }))
        .route("/preview", get(|| async { from_hex(PLAIN) }))
        .route("/original", get(|| async { from_hex(PLAIN) }))
        .route(
            "/bad-image",
            get(|| async { b"<html>not an image</html>".to_vec() }),
        );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Fixture {
        url,
        requests,
        task,
    }
}

#[test]
fn decrypts_independent_cbc_fixture_and_rejects_invalid_padding() {
    let key = parse_key(KEY).unwrap();
    assert_eq!(
        decrypt(&from_hex(ENCRYPTED), &key).unwrap(),
        from_hex(PLAIN)
    );
    let mut corrupt = from_hex(ENCRYPTED);
    corrupt.truncate(corrupt.len() - 1);
    assert!(decrypt(&corrupt, &key).is_err());
    assert!(decrypt(&from_hex(ENCRYPTED), &[0u8; 16]).is_err());
    assert!(parse_key("invalid").is_none());
    assert!(parse_key(&"z".repeat(32)).is_none());
}

#[test]
fn keeps_encrypted_sources_and_decodes_xml_entities_once() {
    let input = format!("<msg><emoji cdnurl=\"\" encrypturl=\"https://example.test/a?x=1&amp;filekey=a%2Bb&amp;amp;c\" aeskey=\"{KEY}\" md5=\"{CHECKSUM}\"/></msg>");
    let attachment = Attachment::parse(47, &input).unwrap();
    assert_eq!(attachment.sources.len(), 1);
    assert_eq!(
        attachment.sources[0].url,
        "https://example.test/a?x=1&filekey=a%2Bb&amp;c"
    );
    assert!(attachment.sources[0].key.is_some());
    assert!(Attachment::parse(
        47,
        "<msg><emoji encrypturl=\"https://example.test/a\"/></msg>"
    )
    .is_none());
    let opaque = Attachment::parse(3, "<msg><img cdnmidimgurl=\"3057020100\"/></msg>").unwrap();
    assert!(opaque.sources.is_empty());
    assert!(
        Attachment::parse(1, "<msg><emoji cdnurl=\"https://example.test/a\"/></msg>").is_none()
    );
}

#[tokio::test]
async fn falls_back_to_encrypted_source_checks_integrity_and_caches_result() {
    let server = fixture().await;
    let store = Store::new("https://example.test/api/onebot/ws".into()).unwrap();
    let xml = format!("<msg><emoji cdnurl=\"{}/missing\" encrypturl=\"{}/encrypted\" aeskey=\"{KEY}\" md5=\"{CHECKSUM}\"/></msg>", server.url, server.url);
    let id = store.register(47, &xml).await.unwrap();
    let image = store.image(&id).await.unwrap();
    assert_eq!(&image.bytes[..], from_hex(PLAIN));
    assert_eq!(image.content_type, "image/gif");
    assert_eq!(image.quality, ImageQuality::Original);
    assert_eq!(
        store.url(&id),
        format!("https://example.test/api/onebot/ws?media={id}")
    );
    assert_eq!(id.len(), 64);
    assert!(store.image(&"0".repeat(64)).await.is_err());
    assert_eq!(store.image(&id).await.unwrap().bytes, image.bytes);
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);

    let wrong = xml.replace(CHECKSUM, &"0".repeat(32));
    let id = store.register(47, &wrong).await.unwrap();
    assert!(store.image(&id).await.err().unwrap().contains("MD5"));
}

#[tokio::test]
async fn returns_a_marked_thumbnail_when_original_sources_fail() {
    let server = fixture().await;
    let store = Store::new("https://example.test/api/onebot/ws".into()).unwrap();
    let xml = format!(
        "<msg><emoji cdnurl=\"{}/bad-image\" thumburl=\"{}/thumb\"/></msg>",
        server.url, server.url
    );
    let id = store.register(47, &xml).await.unwrap();
    let image = store.image(&id).await.unwrap();
    assert_eq!(image.quality, ImageQuality::Thumbnail);
    assert_eq!(image.content_type, "image/gif");
}

#[tokio::test]
async fn original_requests_never_serve_a_cached_preview() {
    let server = fixture().await;
    let store = Store::new("https://example.test/onebot/ws".into()).unwrap();
    let xml = format!(
        "<msg><img cdnmidimgurl=\"{}/preview\" cdnthumburl=\"{}/thumb\"/></msg>",
        server.url, server.url
    );
    let id = store.register(3, &xml).await.unwrap();
    assert_eq!(
        store.image(&id).await.unwrap().quality,
        ImageQuality::Preview
    );
    assert_eq!(
        store.original(&id).await.err().as_deref(),
        Some(ORIGINAL_UNAVAILABLE)
    );

    let xml = format!(
        "<msg><img cdnmidimgurl=\"{}/thumb\" cdnbigimgurl=\"{}/original\"/></msg>",
        server.url, server.url
    );
    let id = store.register(3, &xml).await.unwrap();
    assert_eq!(
        store.original(&id).await.unwrap().quality,
        ImageQuality::Original
    );
}

#[tokio::test]
async fn full_standard_image_is_accepted_only_when_there_is_no_distinct_hd_source() {
    let server = fixture().await;
    let store = Store::new("https://example.test/onebot/ws".into()).unwrap();
    let xml = format!(
        "<msg><img length=\"120000\" cdnthumblength=\"1000\" cdnmidimgurl=\"{}/original\"",
        server.url
    );
    let standard = store.register(3, &format!("{xml}/></msg>")).await.unwrap();
    assert_eq!(
        store.original(&standard).await.unwrap().quality,
        ImageQuality::Full
    );
    let distinct_hd = store
        .register(
            3,
            &format!("{xml} cdnbigimgurl=\"305702\" hdlength=\"300000\"/></msg>"),
        )
        .await
        .unwrap();
    assert_eq!(
        store.original(&distinct_hd).await.err().as_deref(),
        Some(ORIGINAL_UNAVAILABLE)
    );
    assert_eq!(
        store.image(&distinct_hd).await.unwrap().quality,
        ImageQuality::Preview
    );

    let shared_source = format!(r#"{xml} cdnbigimgurl="{}/original""#, server.url);
    let full = store
        .register(3, &format!(r#"{shared_source} hdlength="120000"/></msg>"#))
        .await
        .unwrap();
    assert_eq!(
        store.original(&full).await.unwrap().quality,
        ImageQuality::Full
    );

    for conflicting in [
        r#"hdlength="300000""#.to_owned(),
        format!(r#"hdmd5="{CHECKSUM}""#),
        format!(r#"cdnthumburl="{}/original""#, server.url),
    ] {
        let id = store
            .register(3, &format!("{shared_source} {conflicting}/></msg>"))
            .await
            .unwrap();
        assert_eq!(
            store.original(&id).await.err().as_deref(),
            Some(ORIGINAL_UNAVAILABLE)
        );
    }
}

#[tokio::test]
async fn links_are_unpredictable_and_old_registrations_expire() {
    let store = Store::new("https://example.test/onebot/ws".into()).unwrap();
    let xml = "<msg><emoji cdnurl=\"https://example.test/a.gif\"/></msg>";
    let oldest = store.register(47, xml).await.unwrap();
    let next = store.register(47, xml).await.unwrap();
    assert_ne!(oldest, next);
    for _ in 0..MAX_ENTRIES - 1 {
        store.register(47, xml).await.unwrap();
    }
    assert!(store.image(&oldest).await.is_err());
    assert_eq!(store.cache.lock().await.entries.len(), MAX_ENTRIES);
}

#[test]
fn media_base_url_does_not_expose_api_credentials() {
    for url in [
        "file:///tmp/image",
        "https://example.test/ws?token=secret",
        "https://user:secret@example.test/ws",
        "https://example.test/ws#secret",
    ] {
        assert!(Store::new(url.into()).is_err());
    }
}

#[test]
fn md5_only_stickers_keep_the_metadata_lookup_identity() {
    let attachment =
        Attachment::parse(47, &format!("<msg><emoji md5=\"{CHECKSUM}\"/></msg>")).unwrap();
    assert!(attachment.sources.is_empty());
    assert_eq!(attachment.emoji_md5.as_deref(), Some(CHECKSUM));
    assert!(Attachment::parse(47, "<msg><emoji md5=\"../../file\"/></msg>").is_none());
}

#[tokio::test]
async fn opaque_images_register_only_with_a_database_and_message_identity() {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "mimicwx-image-context-{}",
        u64::from_le_bytes(random)
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let database = Arc::new(crate::db::DbManager::new("0".repeat(64), directory.clone()).unwrap());
    let xml = "<msg><img cdnmidimgurl=\"305f020100044b3049\"/></msg>";
    let store = Store::new("https://example.test/onebot/ws".into()).unwrap();
    assert!(store
        .register_message(3, xml, "room@chatroom", 123)
        .await
        .is_none());
    let store = store.with_database(Some(database));
    assert!(store
        .register_message(3, xml, "room@chatroom", 0)
        .await
        .is_none());
    let id = store
        .register_message(3, xml, "room@chatroom", 123)
        .await
        .unwrap();
    assert!(store
        .register_message(3, "[图片]", "room@chatroom", 123)
        .await
        .is_some());
    assert_eq!(
        store.cache.lock().await.entries[&id].attachment.local_image,
        Some(("room@chatroom".to_string(), 123)),
    );
    assert!(store
        .register_message(1, xml, "room@chatroom", 123)
        .await
        .is_none());
    assert!(store.image(&id).await.is_err());
    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn recognizes_voice_formats_without_accepting_html() {
    assert_eq!(
        super::voice_content_type(b"\x02#!SILK_V3\x00"),
        Some("audio/silk")
    );
    assert_eq!(
        super::voice_content_type(b"#!SILK_V3\x00"),
        Some("audio/silk")
    );
    assert_eq!(
        super::voice_content_type(b"RIFF0000WAVE"),
        Some("audio/wav")
    );
    assert_eq!(super::voice_content_type(b"<!doctype html>"), None);
}
