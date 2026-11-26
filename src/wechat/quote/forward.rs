use super::{ms, NativeQuoteTarget, QuoteEvidence, WeChat};
use crate::atspi::NodeRef;
use crate::input::InputEngine;
use anyhow::{ensure, Context, Result};
use std::path::Path;
use tokio::time::{sleep, timeout_at, Instant};

impl WeChat {
    pub(crate) async fn load_forward_images(
        &self,
        engine: &mut InputEngine,
        to: &str,
        target: &NativeQuoteTarget,
        directory: &Path,
        indexes: &[usize],
        deadline: Instant,
    ) -> Result<()> {
        let complete = || {
            indexes.iter().all(|index| {
                directory
                    .join(index.to_string())
                    .metadata()
                    .is_ok_and(|file| file.is_file() && file.len() > 0)
            })
        };
        if complete() {
            return Ok(());
        }

        let title = target.text.as_str();
        ensure!(
            engine.find_windows_by_title(title, true)?.is_empty(),
            "微信已有同名转发窗口"
        );
        self.chat_with(engine, to)
            .await?
            .context("无法打开合并转发所在会话")?;
        let app = self.find_app().await.context("找不到微信应用")?;
        let window = self.main_chat_window(&app).await?;
        let evidence = QuoteEvidence::from_history(&target.history);
        let mut search_jump = None;
        let point = match self.quote_point(&window, target, &evidence, None).await? {
            Some(point) => point,
            None => {
                let navigation_deadline = deadline - ms(5000);
                search_jump = Some(
                    timeout_at(
                        navigation_deadline,
                        self.find_quote_in_history(engine, &app, &window, to, target, &evidence),
                    )
                    .await
                    .context("微信合并转发历史定位超时")??,
                );
                self.quote_point(&window, target, &evidence, search_jump)
                    .await?
                    .context("无法确认合并转发气泡")?
            }
        };
        let (pointer_x, pointer_y) = point.coordinates;
        engine.move_mouse(pointer_x, pointer_y).await?;
        self.atspi.ensure_no_logout_dialog(&app).await?;
        ensure!(
            Instant::now() < deadline
                && self.find_chat_edit_box(&window, to).await.is_some()
                && self.quote_window_is_active(engine, &window).await?
                && self
                    .quote_point(&window, target, &evidence, search_jump)
                    .await?
                    == Some(point),
            "打开前微信会话或合并转发气泡已变化"
        );
        engine.click(pointer_x, pointer_y).await?;

        let opened = self
            .wait_for_forward_window(engine, &app, title, deadline)
            .await;
        let result = match opened {
            Ok((frame, window_id)) => {
                let result = self
                    .scroll_forward_images(engine, &frame, window_id, complete, deadline)
                    .await;
                if matches!(
                    engine.find_windows_by_title(title, true)?.as_slice(),
                    [(id, _)] if *id == window_id
                ) {
                    engine.close_window_by_title(title, true)?;
                }
                result
            }
            Err(error) => Err(error),
        };
        result
    }

    async fn wait_for_forward_window(
        &self,
        engine: &InputEngine,
        app: &NodeRef,
        title: &str,
        deadline: Instant,
    ) -> Result<(NodeRef, u32)> {
        let deadline = deadline.min(Instant::now() + ms(1400));
        loop {
            if let [(window_id, _)] = engine.find_windows_by_title(title, true)?.as_slice() {
                if *window_id == engine.active_window()? {
                    if let Some(frame) = self
                        .visible_control(app, |role, name| role == "frame" && name == title)
                        .await
                    {
                        return Ok((frame, *window_id));
                    }
                }
            }
            ensure!(Instant::now() < deadline, "微信未打开合并转发窗口");
            sleep(ms(50)).await;
        }
    }

    async fn scroll_forward_images(
        &self,
        engine: &mut InputEngine,
        frame: &NodeRef,
        window_id: u32,
        complete: impl Fn() -> bool,
        deadline: Instant,
    ) -> Result<()> {
        let list = self
            .visible_control(frame, |role, _| role == "list")
            .await
            .context("找不到合并转发的消息列表")?;
        let viewport = self
            .atspi
            .bbox(&list)
            .await
            .context("无法确认合并转发列表位置")?;
        ensure!(
            engine.rectangle_on_screen(viewport.x, viewport.y, viewport.w, viewport.h)?,
            "合并转发窗口超出屏幕"
        );
        let (x, y) = viewport.center();
        sleep(ms(350)).await;
        while !complete() && Instant::now() + ms(350) < deadline {
            ensure!(
                engine.active_window()? == window_id,
                "合并转发窗口已失去焦点"
            );
            engine.scroll(x, y, -4).await?;
            sleep(ms(80)).await;
        }
        while !complete() && Instant::now() + ms(50) < deadline {
            sleep(ms(50)).await;
        }
        ensure!(complete(), "部分合并转发图片仍在等待微信下载");
        Ok(())
    }
}
