use base64::Engine;

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub(super) fn is_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
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
        let output = tokio::process::Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-time",
                "30",
                "--proto",
                "=http,https",
                "--proto-redir",
                "=http,https",
                "--max-filesize",
                "20971520",
                "--url",
                source,
            ])
            .output()
            .await
            .map_err(|_| "无法启动图片下载".to_string())?;
        if !output.status.success() {
            return Err("图片下载失败".into());
        }
        output.stdout
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

fn is_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"BM")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
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
}
