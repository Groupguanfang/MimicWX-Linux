use super::{ms, WeChat, STATE_SHOWING};
use crate::atspi::{AtSpi, BBox, NodeRef};
use crate::db::QuoteHistory;
use crate::input::InputEngine;
use anyhow::{bail, Context, Result};

mod history;
use history::QuoteEvidence;

#[derive(Debug, Clone)]
pub struct NativeQuoteTarget {
    pub server_id: String,
    pub text: String,
    pub sender_name: String,
    pub is_self: bool,
    pub(crate) history: QuoteHistory,
}

impl AtSpi {
    async fn visible_control(
        &self,
        root: &NodeRef,
        matches: impl Fn(&str, &str) -> bool,
    ) -> Option<NodeRef> {
        let mut pending = vec![(root.clone(), 0)];
        let mut visited = 0;
        while let Some((node, depth)) = pending.pop() {
            visited += 1;
            if visited > 750 {
                break;
            }
            let showing = self.get_states(&node).await & STATE_SHOWING != 0;
            if depth > 1 && !showing {
                continue;
            }
            let role = self.role(&node).await;
            if showing && matches(&role, &self.name(&node).await) {
                return Some(node);
            }
            if depth >= 32 || role == "list" {
                continue;
            }
            for index in (0..self.child_count(&node).await.min(50)).rev() {
                if let Some(child) = self.child_at(&node, index).await {
                    pending.push((child, depth + 1));
                }
            }
        }
        None
    }

    async fn quote_remove_button(&self, window: &NodeRef) -> Option<NodeRef> {
        self.visible_control(window, |role, name| {
            role == "push button" && matches!(name, "删除引用消息" | "Remove quoted message")
        })
        .await
    }

    pub(crate) async fn ensure_no_logout_dialog(&self, app: &NodeRef) -> Result<()> {
        for index in 0..self.child_count(app).await.min(20) {
            let Some(popup) = self.child_at(app, index).await else {
                continue;
            };
            if self.role(&popup).await != "frame"
                && self
                    .visible_control(&popup, is_logout_confirmation)
                    .await
                    .is_some()
            {
                bail!("远端微信显示退出登录确认框，请点击取消后再发送");
            }
        }
        Ok(())
    }

    pub(crate) async fn ensure_unquoted_composer(&self, window: &NodeRef) -> Result<()> {
        if self.quote_remove_button(window).await.is_some() {
            bail!("发送前微信输入框出现意外引用，已取消发送");
        }
        Ok(())
    }

    pub(crate) async fn clear_quote_composer(
        &self,
        engine: &mut InputEngine,
        app: &NodeRef,
        window: &NodeRef,
    ) -> Result<bool> {
        let Some(button) = self.quote_remove_button(window).await else {
            return Ok(false);
        };
        let bounds = self
            .bbox(&button)
            .await
            .context("无法读取旧引用删除按钮坐标")?;
        if !engine.rectangle_on_screen(bounds.x, bounds.y, bounds.w, bounds.h)? {
            bail!("旧引用删除按钮超出屏幕，已取消发送");
        }
        self.ensure_no_logout_dialog(app).await?;
        let (center_x, center_y) = bounds.center();
        engine.click(center_x, center_y).await?;
        let deadline = tokio::time::Instant::now() + ms(1000);
        while self.quote_remove_button(window).await.is_some() {
            if tokio::time::Instant::now() >= deadline {
                bail!("旧引用仍在微信输入框中，已取消发送");
            }
            tokio::time::sleep(ms(40)).await;
        }
        tracing::info!("已清除微信输入框中残留的引用");
        Ok(true)
    }
}

impl WeChat {
    pub(super) async fn visible_control(
        &self,
        root: &NodeRef,
        matches: impl Fn(&str, &str) -> bool,
    ) -> Option<NodeRef> {
        self.atspi.visible_control(root, matches).await
    }

    pub(super) async fn native_quote_ready(
        &self,
        window: &NodeRef,
        target: &NativeQuoteTarget,
    ) -> bool {
        self.atspi.quote_remove_button(window).await.is_some()
            && self
                .visible_control(window, |_, name| quote_composer_matches(name, target))
                .await
                .is_some()
    }

    pub(super) async fn prepare_native_quote(
        &self,
        engine: &mut InputEngine,
        app: &NodeRef,
        window: &NodeRef,
        to: &str,
        target: &NativeQuoteTarget,
    ) -> Result<()> {
        if self
            .atspi
            .bbox(window)
            .await
            .is_some_and(|bounds| bounds.w < 800)
            && engine.rectangle_on_screen(0, 0, 800, 600)?
        {
            self.atspi.ensure_no_logout_dialog(app).await?;
            if engine.maximize_window_by_title(&self.atspi.name(window).await)? {
                let deadline = tokio::time::Instant::now() + ms(1000);
                while !self
                    .atspi
                    .bbox(window)
                    .await
                    .is_some_and(|bounds| bounds.w >= 800)
                {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(ms(40)).await;
                }
            }
        }
        let evidence = QuoteEvidence::from_history(&target.history);
        let mut search_jump = None;
        let point = match self.quote_point(window, target, &evidence, None).await? {
            Some(point) => point,
            None => {
                search_jump = Some(
                    self.find_quote_in_history(engine, app, window, to, target, &evidence)
                        .await?,
                );
                self.quote_point(window, target, &evidence, search_jump)
                    .await?
                    .context("历史消息定位后无法核对气泡，已取消发送")?
            }
        };
        let (point_x, point_y) = point.coordinates;
        engine.move_mouse(point_x, point_y).await?;
        self.atspi.ensure_no_logout_dialog(app).await?;
        if self.find_chat_edit_box(window, to).await.is_none() {
            bail!("引用前目标会话已改变，已取消发送");
        }
        if !self.quote_window_is_active(engine, window).await?
            || self
                .quote_point(window, target, &evidence, search_jump)
                .await?
                != Some(point)
        {
            bail!("引用前微信气泡已改变，已取消发送");
        }
        engine.right_click(point_x, point_y).await?;
        let deadline = tokio::time::Instant::now() + ms(1200);
        let menu = loop {
            if let Some(menu) = self
                .visible_control(app, |role, name| {
                    role == "menu item" && matches!(name, "引用" | "Quote" | "Reply")
                })
                .await
            {
                break menu;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("微信没有显示原生引用菜单，已取消发送");
            }
            tokio::time::sleep(ms(40)).await;
        };
        let menu_bounds = self
            .atspi
            .bbox(&menu)
            .await
            .ok_or_else(|| anyhow::anyhow!("无法读取原生引用菜单坐标"))?;
        if !engine.rectangle_on_screen(
            menu_bounds.x,
            menu_bounds.y,
            menu_bounds.w,
            menu_bounds.h,
        )? {
            bail!("微信原生引用菜单超出屏幕，已取消发送");
        }
        self.atspi.ensure_no_logout_dialog(app).await?;
        let (center_x, center_y) = menu_bounds.center();
        engine.click(center_x, center_y).await?;
        let deadline = tokio::time::Instant::now() + ms(1200);
        loop {
            if self.native_quote_ready(window, target).await {
                let editor = self
                    .find_chat_edit_box(window, to)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("引用后目标会话已改变"))?;
                if !self.atspi.grab_focus(&editor).await {
                    bail!("无法聚焦原生引用输入框");
                }
                tracing::info!("↩️ 原生引用已就绪: [{to}] → {}", target.server_id);
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("未确认微信引用框的原作者及内容，已取消发送");
            }
            tokio::time::sleep(ms(40)).await;
        }
    }
}

fn is_logout_confirmation(role: &str, name: &str) -> bool {
    role == "label" && name.starts_with("退出登录后将无法收到新消息")
}

fn normalized_quote_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn quote_text_matches(actual: &str, expected: &str) -> bool {
    !expected.trim().is_empty() && normalized_quote_text(actual) == normalized_quote_text(expected)
}

fn quote_composer_matches(name: &str, target: &NativeQuoteTarget) -> bool {
    !target.sender_name.trim().is_empty()
        && quote_text_matches(
            name,
            &format!("引用 {} 的消息 : {}", target.sender_name, target.text),
        )
}

fn quote_click_point(item: BBox, viewport: BBox, is_self: bool) -> Option<(i32, i32)> {
    if item.w < 180 || item.h <= 0 || viewport.w <= 0 || viewport.h <= 0 {
        return None;
    }
    let point_x = if is_self {
        item.x + item.w - 90
    } else {
        item.x + 90
    };
    let point_y = item.y + item.h / 2;
    (point_x > viewport.x
        && point_x < viewport.x + viewport.w
        && point_y > viewport.y
        && point_y < viewport.y + viewport.h)
        .then_some((point_x, point_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logout_confirmation_matches_the_wechat_popup_label() {
        assert!(is_logout_confirmation(
            "label",
            "退出登录后将无法收到新消息，确定退出登录?"
        ));
        assert!(!is_logout_confirmation("menu item", "退出登录"));
        assert!(!is_logout_confirmation(
            "list item",
            "退出登录后将无法收到新消息，确定退出登录?"
        ));
    }

    #[test]
    fn quote_matching_preserves_the_whole_message_and_author() {
        let target = NativeQuoteTarget {
            server_id: "123".into(),
            text: "@机器人\u{2005}完整内容\n下一行".into(),
            sender_name: "张三".into(),
            is_self: false,
            history: QuoteHistory {
                messages: Vec::new(),
                target_index: 0,
                complete_minutes: 0..0,
            },
        };
        assert!(quote_text_matches(
            "@机器人 完整内容\r\n下一行",
            &target.text
        ));
        assert!(!quote_text_matches("@机器人 完整内容", &target.text));
        assert!(!quote_text_matches(
            "前缀 @机器人 完整内容 下一行",
            &target.text
        ));
        assert!(!quote_text_matches("", ""));
        assert!(quote_composer_matches(
            "引用 张三 的消息 : @机器人 完整内容 下一行",
            &target
        ));
        assert!(!quote_composer_matches(
            "引用 李四 的消息 : @机器人 完整内容 下一行",
            &target
        ));
    }

    #[test]
    fn quote_clicks_require_a_message_inside_the_current_viewport() {
        let viewport = BBox {
            x: 300,
            y: 80,
            w: 1100,
            h: 730,
        };
        let item = BBox {
            x: 300,
            y: 200,
            w: 1100,
            h: 80,
        };
        assert_eq!(quote_click_point(item, viewport, false), Some((390, 240)));
        assert_eq!(quote_click_point(item, viewport, true), Some((1310, 240)));
        assert_eq!(
            quote_click_point(BBox { y: -100, ..item }, viewport, false),
            None
        );
        assert_eq!(
            quote_click_point(BBox { y: 900, ..item }, viewport, true),
            None
        );
    }
}
