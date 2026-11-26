use aes::cipher::{BlockDecrypt, KeyInit};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{media, Image, ImageQuality, ORIGINAL_UNAVAILABLE};
use crate::db::{media_month, ForwardImageCache, LocalImageMetadata};

const MAX_BYTES: usize = 20 * 1024 * 1024;
const V1: &[u8] = b"\x07\x08V1\x08\x07";
const V2: &[u8] = b"\x07\x08V2\x08\x07";
type ImageKey = ([u8; 16], u8);
static RECOVERED_KEYS: Mutex<Vec<(PathBuf, ImageKey)>> = Mutex::new(Vec::new());

fn image_or_wxgf(bytes: &[u8]) -> bool {
    media::image_content_type(bytes).is_some() || bytes.starts_with(b"wxgf")
}

fn decode_dat(bytes: &[u8], key: Option<ImageKey>) -> Result<Vec<u8>, String> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err("本地图片为空或超过 20 MiB".into());
    }
    if image_or_wxgf(bytes) {
        return Ok(bytes.to_vec());
    }
    if bytes.starts_with(V1) || bytes.starts_with(V2) {
        if bytes.len() < 31 {
            return Err("本地图片文件不完整".into());
        }
        let aes_size = u32::from_le_bytes(bytes[6..10].try_into().unwrap()) as usize;
        let xor_size = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;
        if aes_size == 0 || aes_size > MAX_BYTES || xor_size > MAX_BYTES {
            return Err("本地图片分段长度无效".into());
        }
        let encrypted_size = (aes_size / 16 + 1) * 16;
        let end = 15 + encrypted_size;
        if end > bytes.len() || xor_size > bytes.len() - end {
            return Err("本地图片文件不完整".into());
        }
        let (aes_key, mut xor_key) = if bytes.starts_with(V1) {
            (*b"cfcd208495d565ef", key.map(|(_, xor)| xor))
        } else {
            let (aes, xor) = key.ok_or("当前账号图片密钥尚未就绪")?;
            (aes, Some(xor))
        };
        let cipher = aes::Aes128::new_from_slice(&aes_key).map_err(|_| "图片密钥长度无效")?;
        let mut head = bytes[15..end].to_vec();
        for block in head.chunks_exact_mut(16) {
            cipher.decrypt_block(block.into());
        }
        let padding = *head.last().ok_or("图片 AES 分段为空")? as usize;
        if !(1..=16).contains(&padding)
            || !head[head.len() - padding..]
                .iter()
                .all(|byte| *byte as usize == padding)
            || head.len() - padding != aes_size
        {
            return Err("本地图片 AES 校验失败".into());
        }
        head.truncate(aes_size);
        if !image_or_wxgf(&head) {
            return Err("本地图片密钥不匹配".into());
        }
        if xor_size > 0 && xor_key.is_none() && head.starts_with(b"\xff\xd8\xff") && xor_size >= 2 {
            let candidate = bytes[bytes.len() - 2] ^ 0xff;
            if bytes[bytes.len() - 1] ^ candidate == 0xd9 {
                xor_key = Some(candidate);
            }
        }
        let raw_end = bytes.len() - xor_size;
        head.extend_from_slice(&bytes[end..raw_end]);
        if xor_size > 0 {
            let xor_key = xor_key.ok_or("本地图片尾部密钥缺失")?;
            head.extend(bytes[raw_end..].iter().map(|byte| byte ^ xor_key));
        }
        return Ok(head);
    }
    for magic in [
        b"\xff\xd8\xff".as_slice(),
        b"\x89PNG\r\n\x1a\n",
        b"GIF89a",
        b"GIF87a",
        b"RIFF",
    ] {
        let xor = bytes[0] ^ magic[0];
        if bytes.len() >= magic.len()
            && bytes
                .iter()
                .zip(magic)
                .all(|(byte, expected)| byte ^ xor == *expected)
        {
            let plain = bytes.iter().map(|byte| byte ^ xor).collect::<Vec<_>>();
            if image_or_wxgf(&plain) {
                return Ok(plain);
            }
        }
    }
    Err("不支持的本地图片格式".into())
}

fn keys(account: &Path, wxid: &str) -> Vec<ImageKey> {
    let Some(home) = account.parent().and_then(Path::parent) else {
        return Vec::new();
    };
    let directory = home.join(".xwechat/net/kvcomm");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let account_name = account
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let mut keys = Vec::new();
    for entry in entries.take(512).flatten() {
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        let Some(filename) = filename.strip_suffix(".statistic") else {
            continue;
        };
        let filename = filename.strip_prefix("key_").unwrap_or(filename);
        for code in filename.split('_').take(8) {
            let Ok(number) = code.parse::<u64>() else {
                continue;
            };
            for identity in [wxid, account_name] {
                let digest = format!("{:x}", md5::compute(format!("{code}{identity}")));
                let mut aes = [0; 16];
                aes.copy_from_slice(&digest.as_bytes()[..16]);
                let key = (aes, number as u8);
                if !keys.contains(&key) {
                    keys.push(key);
                }
                if keys.len() >= 256 {
                    return keys;
                }
            }
        }
    }
    keys
}

fn recover_key(bytes: &[u8], account: &Path, wxid: &str) -> Option<ImageKey> {
    if !bytes.starts_with(V2) || bytes.len() < 31 {
        return None;
    }
    let filename = account.file_name()?.to_str()?;
    let (identity, suffix) = filename.rsplit_once('_')?;
    if identity != wxid || suffix.len() != 4 {
        return None;
    }
    let suffix = u16::from_str_radix(suffix, 16).ok()?;
    let mut cache = RECOVERED_KEYS.lock().ok()?;
    if let Some((_, key)) = cache
        .iter()
        .find(|(path, key)| path == account && decode_dat(bytes, Some(*key)).is_ok())
    {
        return Some(*key);
    }
    let xor_size = u32::from_le_bytes(bytes[10..14].try_into().ok()?) as usize;
    if xor_size < 2 || xor_size > bytes.len() - 31 {
        return None;
    }
    let xor = bytes[bytes.len() - 2] ^ 0xff;
    if bytes[bytes.len() - 1] ^ xor != 0xd9 {
        return None;
    }
    for candidate in (u64::from(xor)..=u64::from(u32::MAX)).step_by(256) {
        let digest = md5::compute(candidate.to_string());
        if u16::from_be_bytes([digest.0[0], digest.0[1]]) != suffix {
            continue;
        }
        let digest = format!("{:x}", md5::compute(format!("{candidate}{wxid}")));
        let mut aes = [0u8; 16];
        aes.copy_from_slice(&digest.as_bytes()[..16]);
        let key = (aes, xor);
        if decode_dat(bytes, Some(key)).is_ok() {
            cache.retain(|(path, _)| path != account);
            if cache.len() >= 16 {
                cache.remove(0);
            }
            cache.push((account.to_path_buf(), key));
            return Some(key);
        }
    }
    None
}

fn paths(metadata: &LocalImageMetadata, standard_is_full: bool) -> Vec<(PathBuf, ImageQuality)> {
    let Some(month) = media_month(metadata.create_time) else {
        return Vec::new();
    };
    let chat = format!("{:x}", md5::compute(&metadata.chat));
    let images = metadata
        .account
        .join("msg/attach")
        .join(&chat)
        .join(&month)
        .join("Img");
    let bubble = metadata
        .account
        .join("cache")
        .join(month)
        .join("Message")
        .join(chat)
        .join("Bubble");
    let mut paths = Vec::new();
    for (suffix, quality) in [
        ("_h.dat", ImageQuality::Original),
        (
            ".dat",
            if standard_is_full {
                ImageQuality::Full
            } else {
                ImageQuality::Preview
            },
        ),
        ("_b.dat", ImageQuality::Preview),
        ("_t.dat", ImageQuality::Thumbnail),
    ] {
        for hash in &metadata.hashes {
            if !super::is_md5(hash) {
                continue;
            }
            let directory = if suffix == "_b.dat" { &bubble } else { &images };
            paths.push((directory.join(format!("{hash}{suffix}")), quality));
        }
    }
    paths
}

fn read_image(path: &Path, account: &Path) -> Result<Vec<u8>, String> {
    let account = account.canonicalize().map_err(|_| "微信账号目录不可用")?;
    let path = path
        .canonicalize()
        .map_err(|_| "图片尚未下载到微信客户端")?;
    if !path.starts_with(account) {
        return Err("图片路径不属于当前微信账号".into());
    }
    let file: File = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "本地图片暂不可读")?;
    if !file.metadata().map_err(|_| "无法读取图片属性")?.is_file() {
        return Err("本地图片不是普通文件".into());
    }
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "读取本地图片失败")?;
    if bytes.len() > MAX_BYTES {
        return Err("本地图片超过 20 MiB".into());
    }
    if bytes.is_empty() {
        return Err("本地图片文件不完整".into());
    }
    Ok(bytes)
}

async fn wxgf_to_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let start = bytes[..bytes.len().min(1024)]
        .windows(6)
        .position(|window| window == b"\0\0\0\x01\x40\x01")
        .ok_or("WXGF 中没有可解码的 HEVC 图像")?;
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-threads",
            "1",
            "-max_alloc",
            "67108864",
            "-max_pixels",
            "25000000",
            "-f",
            "hevc",
            "-i",
            "pipe:0",
            "-frames:v",
            "1",
            "-threads",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "WXGF 转码需要 ffmpeg")?;
    let mut input = child.stdin.take().ok_or("无法写入转码器")?;
    let output = child.stdout.take().ok_or("无法读取转码器")?;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let write = async {
            input
                .write_all(&bytes[start..])
                .await
                .map_err(|_| "WXGF 转码输入失败")?;
            input.shutdown().await.map_err(|_| "WXGF 转码输入失败")?;
            drop(input);
            Ok::<_, String>(())
        };
        let read = async {
            let mut result = Vec::new();
            output
                .take((MAX_BYTES + 1) as u64)
                .read_to_end(&mut result)
                .await
                .map_err(|_| "WXGF 转码输出失败")?;
            Ok::<_, String>(result)
        };
        let (written, output) = tokio::join!(write, read);
        written?;
        let output = output?;
        if output.len() > MAX_BYTES {
            return Err("WXGF 转码结果超过 20 MiB".into());
        }
        let status = child.wait().await.map_err(|_| "WXGF 转码进程失败")?;
        if !status.success() || media::image_content_type(&output) != Some("image/png") {
            return Err("WXGF 图像转码失败".into());
        }
        Ok(output)
    })
    .await
    .map_err(|_| "WXGF 转码超时")?;
    result
}

pub(super) async fn image(
    metadata: LocalImageMetadata,
    original_md5: Option<&str>,
    standard_is_full: bool,
    original_only: bool,
) -> Result<Image, String> {
    let mut candidates = paths(&metadata, standard_is_full);
    if original_only {
        candidates.retain(|(_, quality)| {
            quality.is_full() || (original_md5.is_some() && *quality == ImageQuality::Preview)
        });
    }
    if candidates.is_empty() {
        return Err("图片尚未下载到微信客户端".into());
    }
    let account = metadata.account.clone();
    let wxid = metadata.wxid.clone();
    let keys = tokio::task::spawn_blocking(move || keys(&metadata.account, &metadata.wxid))
        .await
        .map_err(|_| "读取账号图片密钥失败")?;
    let checksum_quality = original_md5.map(|checksum| {
        let quality = if standard_is_full {
            ImageQuality::Full
        } else {
            ImageQuality::Original
        };
        (checksum, quality)
    });
    for attempt in 0..9 {
        let result = read_candidates(
            &candidates,
            &account,
            &wxid,
            &keys,
            checksum_quality,
            original_only,
        )
        .await;
        let pending = result.as_ref().err().is_some_and(|error| {
            matches!(
                error.as_str(),
                "图片尚未下载到微信客户端" | "本地图片文件不完整"
            ) || error == ORIGINAL_UNAVAILABLE
        });
        if !pending || attempt == 8 {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    unreachable!()
}

pub(super) async fn forward_image(
    cache: &ForwardImageCache,
    index: usize,
    checksum: Option<&str>,
) -> Result<Image, String> {
    let path = cache.image_path(index);
    let account = cache.account.clone();
    let wxid = cache.wxid.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        let bytes = read_image(&path, &account)?;
        let keys = if bytes.starts_with(V2) {
            keys(&account, &wxid)
        } else {
            Vec::new()
        };
        decode_local_image(&bytes, &account, &wxid, &keys)
    })
    .await
    .map_err(|_| "转发图片解码任务失败")?
    .map_err(|error| {
        if matches!(
            error.as_str(),
            "图片尚未下载到微信客户端" | "本地图片文件不完整"
        ) {
            ORIGINAL_UNAVAILABLE.to_owned()
        } else {
            error
        }
    })?;
    let quality =
        if checksum.is_some_and(|expected| format!("{:x}", md5::compute(&bytes)) == expected) {
            ImageQuality::Original
        } else {
            ImageQuality::Full
        };
    local_image(bytes, quality).await
}

fn decode_local_image(
    bytes: &[u8],
    account: &Path,
    wxid: &str,
    keys: &[ImageKey],
) -> Result<Vec<u8>, String> {
    if !bytes.starts_with(V2) {
        return decode_dat(bytes, None);
    }
    let layout = decode_dat(bytes, None);
    if layout
        .as_ref()
        .err()
        .is_some_and(|error| error != "当前账号图片密钥尚未就绪")
    {
        return layout;
    }
    keys.iter()
        .find_map(|key| decode_dat(bytes, Some(*key)).ok())
        .or_else(|| {
            recover_key(bytes, account, wxid).and_then(|key| decode_dat(bytes, Some(key)).ok())
        })
        .ok_or_else(|| "本地图片账号密钥不匹配".to_string())
}

async fn local_image(bytes: Vec<u8>, quality: ImageQuality) -> Result<Image, String> {
    let bytes = if bytes.starts_with(b"wxgf") {
        wxgf_to_png(&bytes).await?
    } else {
        bytes
    };
    let content_type = media::image_content_type(&bytes).ok_or("解密结果不是支持的图片")?;
    Ok(Image {
        bytes: bytes.into(),
        content_type,
        quality,
    })
}

async fn read_candidates(
    candidates: &[(PathBuf, ImageQuality)],
    account: &Path,
    wxid: &str,
    keys: &[ImageKey],
    checksum_quality: Option<(&str, ImageQuality)>,
    original_only: bool,
) -> Result<Image, String> {
    let mut last_error = "图片尚未下载到微信客户端".to_string();
    for (path, candidate_quality) in candidates {
        let path = path.clone();
        let account = account.to_path_buf();
        let wxid = wxid.to_string();
        let keys = keys.to_vec();
        let decoded = tokio::task::spawn_blocking(move || {
            let bytes = read_image(&path, &account)?;
            decode_local_image(&bytes, &account, &wxid, &keys)
        })
        .await
        .map_err(|_| "本地图片解密任务失败")?;
        let result = async {
            let bytes = decoded?;
            let quality = if *candidate_quality == ImageQuality::Preview {
                checksum_quality
                    .filter(|(expected, _)| format!("{:x}", md5::compute(&bytes)) == *expected)
                    .map_or(*candidate_quality, |(_, quality)| quality)
            } else {
                *candidate_quality
            };
            if original_only && !quality.is_full() {
                return Err(ORIGINAL_UNAVAILABLE.into());
            }
            local_image(bytes, quality).await
        }
        .await;
        match result {
            Ok(image) => return Ok(image),
            Err(error) if error != "图片尚未下载到微信客户端" => {
                if last_error == "图片尚未下载到微信客户端" {
                    last_error = error;
                }
            }
            Err(_) => {}
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests;
