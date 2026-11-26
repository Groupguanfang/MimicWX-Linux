use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{os::unix::fs::DirBuilderExt, path::PathBuf, process::Stdio, time::Duration};
use tokio::process::Command;

pub struct PreparedVoice {
    directory: PathBuf,
    pub path: String,
    pub duration_ms: u32,
}

impl Drop for PreparedVoice {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub async fn command(program: &str, arguments: &[&str], timeout: Duration) -> Result<Vec<u8>> {
    let process = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, process)
        .await
        .with_context(|| format!("{program} 执行超时"))??;
    if !output.status.success() {
        bail!("{program} 执行失败");
    }
    Ok(output.stdout)
}

pub async fn voice_available() -> bool {
    let Ok(bytes) = command(
        "pactl",
        &["--format=json", "list", "sources"],
        Duration::from_secs(3),
    )
    .await
    else {
        return false;
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|sources| sources.as_array().cloned())
        .is_some_and(|sources| {
            sources
                .iter()
                .any(|source| source["name"] == "mimicwx_voice")
        })
}

pub async fn recording_streams() -> Result<Vec<Value>> {
    let bytes = command(
        "pactl",
        &["--format=json", "list", "source-outputs"],
        Duration::from_secs(3),
    )
    .await?;
    let values: Vec<Value> = serde_json::from_slice(&bytes)?;
    Ok(values
        .into_iter()
        .filter(|value| !value["client"].is_null())
        .collect())
}

pub async fn prepare(bytes: &[u8]) -> Result<PreparedVoice> {
    if bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
        bail!("出站语音为空或超过 2 MiB");
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| anyhow::anyhow!("无法生成语音临时目录"))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let directory = std::env::temp_dir().join(format!("mimicwx_voice_{suffix}"));
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let input = directory.join("input.audio");
    let path = directory.join("output.wav");
    let mut voice = PreparedVoice {
        directory,
        path: path.to_string_lossy().into_owned(),
        duration_ms: 0,
    };
    tokio::fs::write(&input, bytes).await?;
    command(
        "ffmpeg",
        &[
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            input.to_str().context("语音路径无效")?,
            "-vn",
            "-ac",
            "1",
            "-ar",
            "24000",
            "-c:a",
            "pcm_s16le",
            "-t",
            "61",
            "-fs",
            "3000000",
            "-y",
            &voice.path,
        ],
        Duration::from_secs(15),
    )
    .await?;
    let duration = command(
        "ffprobe",
        &[
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            &voice.path,
        ],
        Duration::from_secs(3),
    )
    .await?;
    let seconds: f64 = std::str::from_utf8(&duration)?.trim().parse()?;
    if !seconds.is_finite() || !(0.5..=55.0).contains(&seconds) {
        bail!("微信出站语音须在 0.5 到 55 秒之间，请分段发送");
    }
    voice.duration_ms = (seconds * 1000.0).round() as u32;
    Ok(voice)
}
