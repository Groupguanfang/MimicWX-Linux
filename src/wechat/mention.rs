use super::{ms, NativeQuoteTarget, WeChat};
use crate::{
    atspi::{AtSpi, NodeRef},
    input::InputEngine,
};
use anyhow::{ensure, Context, Result};
use tracing::warn;

const MENTION_OBJECT: char = '\u{fffc}';

#[derive(Debug)]
pub(crate) enum TextPart {
    Text(String),
    Mention { id: String, name: String },
}

pub(crate) fn rendered_text(parts: &[TextPart]) -> String {
    let mut text = String::new();
    for part in parts {
        match part {
            TextPart::Text(value) => text.push_str(value),
            TextPart::Mention { name, .. } => {
                text.push_str("\u{2005}@");
                text.push_str(name);
                text.push('\u{2005}');
            }
        }
    }
    text
}

impl AtSpi {
    pub(crate) async fn input_literal_at_text(
        &self,
        engine: &mut InputEngine,
        editor: &NodeRef,
        text: &str,
    ) -> Result<()> {
        if !self.set_text(editor, text).await {
            engine.paste_text(text).await?;
        }
        engine.dismiss_wechat_member_popup().await
    }
}

impl WeChat {
    pub(super) async fn input_with_mentions(
        &self,
        engine: &mut InputEngine,
        app: &NodeRef,
        window: &NodeRef,
        to: &str,
        parts: &[TextPart],
        quote: Option<&NativeQuoteTarget>,
    ) -> Result<()> {
        let mut expected = String::new();
        let mut current_query = None;
        let result = async {
            let parent = engine.active_window()?;
            for part in parts {
                current_query = None;
                match part {
                    TextPart::Text(text) => {
                        expected.push_str(text);
                        engine.paste_text(text).await?;
                    }
                    TextPart::Mention { name, .. } => {
                        let typed_at = format!("{expected}@");
                        let typed_query = format!("{typed_at}{name}");
                        current_query = Some(typed_query.clone());
                        expected.push(MENTION_OBJECT);
                        ensure!(
                            engine.wechat_member_popup(parent)?.is_none(),
                            "微信 @ 候选框已被其他操作打开"
                        );
                        engine.type_text("@").await?;
                        let popup = self
                            .wait_for_member_popup(engine, parent, None)
                            .await?
                            .context("微信没有打开 @ 群成员候选框")?;
                        self.ensure_mention_draft(window, to, &typed_at).await?;
                        engine.paste_text(name).await?;
                        self.ensure_mention_draft(window, to, &typed_query).await?;
                        let candidate = self
                            .wait_for_member_popup(engine, parent, Some(popup))
                            .await?
                            .with_context(|| format!("微信无法唯一定位 @ 的群成员 {name}"))?;
                        self.atspi.ensure_no_logout_dialog(app).await?;
                        engine.select_wechat_member(parent, candidate).await?;
                    }
                }
                if expected.trim().is_empty() {
                    self.ensure_mention_draft(window, to, &expected).await?;
                } else {
                    ensure!(
                        self.wait_for_message_input(window, to, &expected).await,
                        "微信输入框内容与本次 @ 消息不一致"
                    );
                }
            }
            engine.dismiss_wechat_member_popup().await?;
            let copied = engine.copy_composer_text().await?;
            ensure!(
                copied.replace("\r\n", "\n").trim_end_matches('\n')
                    == rendered_text(parts)
                        .replace("\r\n", "\n")
                        .trim_end_matches('\n'),
                "微信选中的 @ 昵称与预期不一致"
            );
            ensure!(
                self.wait_for_message_input(window, to, &expected).await,
                "复核 @ 后微信输入框内容已改变"
            );
            if let Some(target) = quote {
                ensure!(
                    self.native_quote_ready(window, target).await,
                    "发送前微信原生引用框已改变"
                );
            } else {
                self.atspi.ensure_unquoted_composer(window).await?;
            }
            self.atspi.ensure_no_logout_dialog(app).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let cleanup = async {
                let seen = self
                    .find_chat_edit_box(window, to)
                    .await
                    .context("目标会话已改变")?;
                let contents = self.atspi.text(&seen).await.context("无法读取残稿")?;
                let owned = current_query
                    .as_deref()
                    .filter(|query| query.starts_with(&contents))
                    .unwrap_or(&expected);
                ensure!(
                    self.clear_partial_draft(engine, app, window, to, owned, quote.is_some())
                        .await?,
                    "无法确认草稿属于本次 @ 请求"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = cleanup {
                warn!(error = %error, "未能清除本次 @ 请求的残稿");
            }
        }
        result
    }

    async fn ensure_mention_draft(&self, window: &NodeRef, to: &str, expected: &str) -> Result<()> {
        let editor = self
            .find_chat_edit_box(window, to)
            .await
            .context("输入 @ 时目标会话已改变")?;
        let actual = self
            .atspi
            .text(&editor)
            .await
            .context("无法读取 @ 输入框")?;
        ensure!(
            actual.replace("\r\n", "\n") == expected.replace("\r\n", "\n"),
            "微信 @ 候选查询与预期不一致"
        );
        Ok(())
    }

    async fn wait_for_member_popup(
        &self,
        engine: &InputEngine,
        parent: u32,
        initial_popup: Option<u32>,
    ) -> Result<Option<u32>> {
        let deadline = tokio::time::Instant::now() + ms(450);
        loop {
            if let Some((current, _, _, width, height)) = engine.wechat_member_popup(parent)? {
                if initial_popup.is_none()
                    || (initial_popup == Some(current) && width == 170 && height == 40)
                {
                    return Ok(Some(current));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(ms(40)).await;
        }
    }
}
