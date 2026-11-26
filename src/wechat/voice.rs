use super::{ms, WeChat};
use crate::{atspi::NodeRef, audio, input::InputEngine};
use anyhow::{bail, Context, Result};
use std::time::Duration;

impl WeChat {
    async fn click_voice_control(
        &self,
        engine: &mut InputEngine,
        app: &NodeRef,
        name: &str,
    ) -> Result<()> {
        let button = self
            .visible_control(app, |role, actual| role == "push button" && actual == name)
            .await
            .with_context(|| format!("找不到微信语音控件: {name}"))?;
        if self.atspi.get_states(&button).await & (1 << 8) == 0 {
            bail!("微信语音控件不可用: {name}");
        }
        let bounds = self.atspi.bbox(&button).await.context("语音控件没有坐标")?;
        if !engine.rectangle_on_screen(bounds.x, bounds.y, bounds.w, bounds.h)? {
            bail!("微信语音控件超出远端屏幕，请使用桌面缩放而非缩小远端分辨率");
        }
        let (center_x, center_y) = bounds.center();
        engine.click(center_x, center_y).await
    }

    async fn require_voice_target(&self, app: &NodeRef, to: &str) -> Result<()> {
        let editor = self
            .find_chat_edit_box(app, to)
            .await
            .context("语音目标会话已改变")?;
        if !self
            .atspi
            .text(&editor)
            .await
            .is_some_and(|text| text.trim().is_empty())
        {
            bail!("语音目标会话存在草稿，已取消发送");
        }
        self.atspi.ensure_unquoted_composer(app).await
    }

    pub async fn send_voice(
        &self,
        engine: &mut InputEngine,
        to: &str,
        path: &str,
        duration_ms: u32,
    ) -> Result<(bool, bool, String)> {
        if !audio::voice_available().await || !audio::recording_streams().await?.is_empty() {
            bail!("虚拟麦克风不可用或已有其他录音，已取消发送");
        }
        self.chat_with(engine, to)
            .await?
            .context("找不到语音目标会话")?;
        let app = self.find_app().await.context("找不到微信应用")?;
        let window = self.main_chat_window(&app).await?;
        self.prepare_empty_composer(engine, &app, &window, to)
            .await?;
        self.click_voice_control(engine, &app, "发语音").await?;
        let recording = async {
            let deadline = tokio::time::Instant::now() + ms(2000);
            let stream = loop {
                let streams = audio::recording_streams().await?;
                if streams.len() == 1 {
                    break streams.into_iter().next().unwrap();
                }
                if streams.len() > 1 || tokio::time::Instant::now() >= deadline {
                    bail!("未取得唯一的微信录音流");
                }
                tokio::time::sleep(ms(50)).await;
            };
            let index = stream["index"]
                .as_u64()
                .context("录音流编号无效")?
                .to_string();
            audio::command(
                "pactl",
                &["move-source-output", &index, "mimicwx_voice"],
                Duration::from_secs(3),
            )
            .await?;
            audio::command(
                "paplay",
                &["--device=mimicwx_voice_sink", path],
                Duration::from_millis(u64::from(duration_ms) + 5000),
            )
            .await?;
            tokio::time::sleep(ms(150)).await;
            self.require_voice_target(&window, to).await?;
            let streams = audio::recording_streams().await?;
            if streams.len() != 1 || streams[0]["index"] != stream["index"] {
                bail!("微信录音流在发送前已改变");
            }
            self.atspi.ensure_no_logout_dialog(&app).await?;
            self.click_voice_control(engine, &app, "发送语音").await?;
            Ok::<_, anyhow::Error>(())
        };
        let result = tokio::time::timeout(
            Duration::from_millis(u64::from(duration_ms) + 8000),
            recording,
        )
        .await
        .map_err(|_| anyhow::anyhow!("微信语音录制超时"))
        .and_then(|result| result);
        if let Err(error) = result {
            if self.atspi.ensure_no_logout_dialog(&app).await.is_ok()
                && self.find_chat_edit_box(&window, to).await.is_some()
            {
                let _ = self.click_voice_control(engine, &app, "取消").await;
            }
            return Err(error);
        }
        let deadline = tokio::time::Instant::now() + ms(1500);
        loop {
            if self
                .visible_control(&app, |role, name| {
                    role == "push button" && name == "发送语音"
                })
                .await
                .is_none()
            {
                tracing::info!("🎙️ 微信语音已提交: [{to}]");
                return Ok((true, false, "微信语音已提交，等待数据库确认".into()));
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("已点击发送语音但未确认面板收起，请检查实际结果，勿重复发送");
            }
            tokio::time::sleep(ms(50)).await;
        }
    }
}
