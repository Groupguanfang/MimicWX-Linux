use base64::Engine;
use std::process::Stdio;
use tokio::io::AsyncReadExt;

pub(super) const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub(super) fn is_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

pub(super) async fn audio_bytes(source: &str) -> Result<Vec<u8>, String> {
    const LIMIT: usize = 2 * 1024 * 1024;
    let bytes = if let Some(encoded) = source.strip_prefix("base64://").or_else(|| {
        source
            .strip_prefix("data:audio/")
            .and_then(|data| data.split_once(";base64,").map(|(_, value)| value))
    }) {
        if encoded.len() > LIMIT * 4 / 3 + 4 {
            return Err("出站语音超过 2 MiB".into());
        }
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "语音 Base64 无效".to_string())?
    } else if is_url(source) {
        download_bytes(source).await?
    } else {
        return Err("语音必须是 HTTP(S) URL 或 base64://".into());
    };
    if bytes.is_empty() || bytes.len() > LIMIT {
        return Err("出站语音为空或超过 2 MiB".into());
    }
    Ok(bytes)
}

pub(super) async fn image_bytes(source: &str) -> Result<Vec<u8>, String> {
    let bytes = if let Some(encoded) = source.strip_prefix("base64://").or_else(|| {
        source
            .strip_prefix("data:")
            .and_then(|data| data.split_once(";base64,").map(|(_, value)| value))
    }) {
        if encoded.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
            return Err("图片超过 20 MiB".into());
        }
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "图片 Base64 无效".to_string())?
    } else if is_url(source) {
        download_bytes(source).await?
    } else if let Some(path) = source.strip_prefix("file://") {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|_| "图片文件不存在".to_string())?;
        if metadata.len() > MAX_IMAGE_BYTES as u64 {
            return Err("图片超过 20 MiB".into());
        }
        tokio::fs::read(path)
            .await
            .map_err(|_| "无法读取图片文件".to_string())?
    } else {
        return Err("图片必须是 HTTP(S) URL、base64:// 或 file://".into());
    };
    if bytes.len() > MAX_IMAGE_BYTES || !is_image(&bytes) {
        return Err("文件不是支持的图片，或超过 20 MiB".into());
    }
    Ok(bytes)
}

pub(super) async fn download_bytes(source: &str) -> Result<Vec<u8>, String> {
    if !is_url(source) {
        return Err("媒体地址必须是 HTTP(S) URL".into());
    }
    let mut child = tokio::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--location",
            "--connect-timeout",
            "5",
            "--max-time",
            "20",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            "--max-filesize",
            "20971520",
            "--url",
            source,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "无法启动媒体下载".to_string())?;
    let mut bytes = Vec::new();
    let stdout = child.stdout.take().ok_or("无法读取媒体下载")?;
    stdout
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "读取媒体失败")?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("媒体超过 20 MiB".into());
    }
    if !child
        .wait()
        .await
        .map_err(|_| "等待媒体下载失败")?
        .success()
    {
        return Err("媒体下载失败".into());
    }
    Ok(bytes)
}

pub(super) fn image_content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn is_image(bytes: &[u8]) -> bool {
    image_content_type(bytes).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn validates_media_without_network_or_wechat() {
        assert!(!is_image(b"not an image"));
        assert!(is_image(b"GIF89a0000"));
        let data = base64::engine::general_purpose::STANDARD.encode(b"GIF89a0000");
        assert_eq!(
            image_bytes(&format!("base64://{data}")).await.unwrap(),
            b"GIF89a0000"
        );
        assert!(image_bytes("base64://bm90IGFuIGltYWdl").await.is_err());
        assert!(image_bytes("ftp://example.test/image.png").await.is_err());
    }

    #[tokio::test]
    async fn bounds_downloads_without_a_content_length() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/image", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let chunk = [0u8; 65536];
            for _ in 0..MAX_IMAGE_BYTES / chunk.len() + 2 {
                if socket.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        assert!(download_bytes(&url).await.err().unwrap().contains("20 MiB"));
        server.abort();
    }
}
