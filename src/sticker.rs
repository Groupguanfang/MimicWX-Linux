use anyhow::{Context, Result};
use serde::Deserialize;
use std::{os::unix::fs::DirBuilderExt, path::PathBuf, time::Duration};

const MAX_STICKER_BYTES: usize = 1024 * 1024;

struct TemporaryDirectory(PathBuf);

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Deserialize)]
struct ImageProbe {
    streams: Vec<ImageStream>,
}

#[derive(Deserialize)]
struct ImageStream {
    width: u32,
    height: u32,
}

pub async fn prepare(bytes: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= 20 * 1024 * 1024,
        "表情源文件为空或超过 20 MiB"
    );
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        let screen = bytes.get(6..13).context("GIF 表情文件不完整")?;
        let width = u16::from_le_bytes([screen[0], screen[1]]);
        let height = u16::from_le_bytes([screen[2], screen[3]]);
        anyhow::ensure!(
            bytes.len() > 13 && bytes.last() == Some(&0x3b),
            "GIF 表情文件不完整"
        );
        anyhow::ensure!(
            bytes.len() <= MAX_STICKER_BYTES
                && (1..=1000).contains(&width)
                && (1..=1000).contains(&height),
            "原生 GIF 表情须不超过 1 MiB，宽高须在 1 到 1000 像素之间"
        );
        return Ok(bytes.to_vec());
    }

    let mut randomness = [0_u8; 16];
    getrandom::fill(&mut randomness).map_err(|_| anyhow::anyhow!("无法生成表情临时目录"))?;
    let suffix = randomness
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let directory =
        TemporaryDirectory(std::env::temp_dir().join(format!("mimicwx_sticker_{suffix}")));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory.0)?;
    let input = directory.0.join("source.image");
    tokio::fs::write(&input, bytes).await?;
    let input = input.to_str().context("表情源文件路径无效")?;
    let metadata = crate::audio::command(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "json",
            input,
        ],
        Duration::from_secs(5),
    )
    .await
    .context("无法读取表情图片")?;
    let metadata: ImageProbe = serde_json::from_slice(&metadata)?;
    let image = metadata.streams.first().context("表情源文件没有图像")?;
    anyhow::ensure!(
        image.width > 0
            && image.height > 0
            && image.width <= 8192
            && image.height <= 8192
            && u64::from(image.width) * u64::from(image.height) <= 40_000_000,
        "表情源图片尺寸超出限制"
    );
    let converted = crate::audio::command(
        "ffmpeg",
        &[
            "-nostdin", "-hide_banner", "-loglevel", "error",
            "-threads", "1", "-filter_complex_threads", "1",
            "-i", input,
            "-filter_complex",
            "scale=w='min(512,iw)':h='min(512,ih)':force_original_aspect_ratio=decrease:flags=lanczos,split[colors][image];[colors]palettegen[palette];[image][palette]paletteuse=dither=bayer",
            "-frames:v", "1", "-f", "gif", "pipe:1",
        ],
        Duration::from_secs(15),
    )
    .await
    .context("静态图片转换为原生表情失败")?;
    anyhow::ensure!(
        converted.len() <= MAX_STICKER_BYTES
            && (converted.starts_with(b"GIF87a") || converted.starts_with(b"GIF89a")),
        "表情转换结果无效或超过 1 MiB"
    );
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    const GIF: &str = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=";

    #[tokio::test]
    async fn preserves_native_gif_bytes_and_checksum() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(GIF)
            .unwrap();
        assert_eq!(prepare(&bytes).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn converts_static_images_to_a_valid_gif_file() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap();
        let converted = prepare(&bytes).await.unwrap();
        assert!(converted.starts_with(b"GIF8"));
        assert_eq!(prepare(&converted).await.unwrap(), converted);
    }

    #[tokio::test]
    async fn rejects_invalid_and_oversized_stickers() {
        assert!(prepare(b"GIF89a0000").await.is_err());
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(GIF)
            .unwrap();
        assert!(prepare(&bytes[..bytes.len() - 1]).await.is_err());
        bytes[6..8].copy_from_slice(&1001_u16.to_le_bytes());
        assert!(prepare(&bytes).await.is_err());
        bytes[6..8].copy_from_slice(&0_u16.to_le_bytes());
        assert!(prepare(&bytes).await.is_err());
        bytes[6..8].copy_from_slice(&1_u16.to_le_bytes());
        bytes.resize(MAX_STICKER_BYTES, 0);
        bytes.push(0x3b);
        assert!(prepare(&bytes).await.is_err());
    }
}
