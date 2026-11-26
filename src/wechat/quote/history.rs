use super::{normalized_quote_text, quote_click_point, NativeQuoteTarget};
use crate::atspi::{BBox, NodeRef};
use crate::db::{DbMessage, MsgContent, QuoteHistory};
use crate::input::InputEngine;
use crate::wechat::{ms, WeChat};
use anyhow::{bail, ensure, Context, Result};
use futures_util::{stream, StreamExt};
use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Instant};

struct HistoryEntry {
    text: String,
    minute: i64,
    time_labels: Vec<String>,
    searchable: bool,
    is_reply: bool,
    is_forward: bool,
}

impl HistoryEntry {
    fn matches_bubble(&self, row: &ListRow) -> bool {
        !self.text.is_empty()
            && (self.text == row.label
                || (self.is_reply && row.reply_body.as_deref() == Some(self.text.as_str()))
                || (self.is_forward
                    && ["聊天记录", "Chat History"].into_iter().any(|prefix| {
                        row.label
                            .strip_prefix(prefix)
                            .is_some_and(|label| label.trim_start().starts_with(&self.text))
                    })))
    }
}

pub(super) struct QuoteEvidence {
    entries: Vec<HistoryEntry>,
    target: usize,
    complete_minutes: Range<i64>,
}

struct ListRow {
    node: NodeRef,
    label: String,
    reply_body: Option<String>,
    bounds: BBox,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct QuotePoint {
    pub(super) coordinates: (i32, i32),
    path: String,
    bounds: BBox,
}

#[derive(Clone, Copy)]
pub(super) struct SearchJump {
    seed: usize,
    exact: bool,
}

struct Bubble<'a> {
    clock: Option<&'a str>,
    row: &'a ListRow,
}

impl QuoteEvidence {
    pub(super) fn from_history(history: &QuoteHistory) -> Self {
        let today = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        Self {
            entries: history
                .messages
                .iter()
                .map(|message| {
                    let (text, searchable) = bubble_text(message);
                    HistoryEntry {
                        text: normalized_quote_text(text),
                        minute: message.create_time.div_euclid(60) * 60,
                        time_labels: timestamp_labels(message.create_time, today),
                        searchable,
                        is_reply: matches!(
                            message.parsed,
                            MsgContent::App {
                                app_type: Some(57),
                                ..
                            }
                        ),
                        is_forward: message.parsed.is_forward_card(),
                    }
                })
                .collect(),
            target: history.target_index,
            complete_minutes: history.complete_minutes.clone(),
        }
    }

    fn candidate(
        &self,
        bubbles: &[Bubble<'_>],
        jump: Option<(usize, SearchJump)>,
    ) -> Option<usize> {
        if bubbles.is_empty() {
            return None;
        }
        let mut winner = None;
        for (offset, entries) in self.entries.windows(bubbles.len()).enumerate() {
            let mut proved_by_clock = 0;
            let follows_history = bubbles.iter().zip(entries).all(|(bubble, entry)| {
                if !entry.matches_bubble(bubble.row) {
                    return false;
                }
                if let Some(clock) = bubble.clock {
                    if !entry.time_labels.iter().any(|label| label == clock) {
                        return false;
                    }
                    proved_by_clock += usize::from(self.complete_minutes.contains(&entry.minute));
                }
                true
            });
            if !follows_history {
                continue;
            }
            let proved_by_jump = jump.is_some_and(|(bubble_index, jump)| {
                let seed = &self.entries[jump.seed];
                entries[bubble_index].minute == seed.minute
                    && entries[bubble_index].text == seed.text
                    && self.complete_minutes.contains(&seed.minute)
                    && (!jump.exact || offset + bubble_index == jump.seed)
            });
            if jump.is_some() && !proved_by_jump || proved_by_clock == 0 && !proved_by_jump {
                continue;
            }
            let index = self.target.checked_sub(offset)?;
            if index >= bubbles.len() || winner.is_some_and(|previous| previous != index) {
                return None;
            }
            winner = Some(index);
        }
        winner
    }

    fn seeds(&self) -> impl Iterator<Item = (usize, &HistoryEntry)> {
        let start = self.target.saturating_sub(4);
        let end = (self.target + 5).min(self.entries.len());
        let mut positions: Vec<_> = (start..end).collect();
        positions.sort_by_key(|index| index.abs_diff(self.target));
        positions
            .into_iter()
            .map(|index| (index, &self.entries[index]))
            .filter(|(index, entry)| {
                entry.searchable
                    && !entry.text.is_empty()
                    && self.complete_minutes.contains(&entry.minute)
                    && (*index == self.target
                        || self
                            .entries
                            .iter()
                            .filter(|other| {
                                other.minute == entry.minute && other.text == entry.text
                            })
                            .count()
                            == 1)
            })
            .take(3)
    }
}

fn bubble_text(message: &DbMessage) -> (&str, bool) {
    match &message.parsed {
        MsgContent::Text { text } => (text, true),
        MsgContent::App {
            title: Some(title),
            app_type: Some(57),
            ..
        } => (title, true),
        MsgContent::App {
            title: Some(title), ..
        } => (title, false),
        MsgContent::Image { .. } => ("图片", false),
        MsgContent::Emoji { .. } => ("动画表情", false),
        MsgContent::Video { .. } => ("视频", false),
        MsgContent::Voice { .. } => ("语音", false),
        MsgContent::System { text } => (text, false),
        _ => ("", false),
    }
}

fn local_time(seconds: i64) -> Option<libc::tm> {
    let seconds = seconds as libc::time_t;
    let mut clock = std::mem::MaybeUninit::uninit();
    unsafe {
        (!libc::localtime_r(&seconds, clock.as_mut_ptr()).is_null()).then(|| clock.assume_init())
    }
}

fn timestamp_labels(seconds: i64, now: i64) -> Vec<String> {
    let Some((message_time, current_time)) = local_time(seconds).zip(local_time(now)) else {
        return Vec::new();
    };
    let local_day = |seconds: i64, clock: libc::tm| (seconds + clock.tm_gmtoff).div_euclid(86400);
    let days_ago = local_day(now, current_time) - local_day(seconds, message_time);
    let (year, month, day) = (
        message_time.tm_year + 1900,
        message_time.tm_mon + 1,
        message_time.tm_mday,
    );
    let clock = format!("{:02}:{:02}", message_time.tm_hour, message_time.tm_min);
    let mut labels = vec![
        format!("{year}年{month}月{day}日 {clock}"),
        format!("{year:04}-{month:02}-{day:02} {clock}"),
        format!("{year:04}/{month:02}/{day:02} {clock}"),
    ];
    if message_time.tm_year == current_time.tm_year {
        labels.push(format!("{month}月{day}日 {clock}"));
        labels.push(format!("{month:02}-{day:02} {clock}"));
    }
    if days_ago == 0 {
        labels.push(clock.clone());
        labels.push(format!("今天 {clock}"));
    }
    if days_ago == 1 {
        labels.push(format!("昨天 {clock}"));
    }
    if (1..7).contains(&days_ago) {
        let weekday = ["日", "一", "二", "三", "四", "五", "六"][message_time.tm_wday as usize];
        labels.push(format!("星期{weekday} {clock}"));
        labels.push(format!("周{weekday} {clock}"));
    }
    labels
}

fn is_time_separator(row: &ListRow) -> bool {
    let last = row.label.split_whitespace().next_back().unwrap_or("");
    let Some((hour, minute)) = last.split_once(':') else {
        return false;
    };
    row.bounds.h == 41
        && hour.len() == 2
        && minute.len() == 2
        && hour.parse::<u8>().is_ok_and(|hour| hour < 24)
        && minute.parse::<u8>().is_ok_and(|minute| minute < 60)
}

fn search_result_matches(label: &str, entry: &HistoryEntry) -> bool {
    entry.time_labels.iter().any(|time| {
        label
            .strip_suffix(time)
            .and_then(|body| body.strip_suffix(' '))
            .is_some_and(|body| normalized_quote_text(body) == entry.text)
    })
}

fn is_history_title(title: &str, chat: &str) -> bool {
    [
        format!("\"{chat}\"的聊天记录"),
        format!("与\"{chat}\"的聊天记录"),
        format!("{chat}的聊天记录"),
    ]
    .into_iter()
    .any(|prefix| {
        title.strip_prefix(&prefix).is_some_and(|suffix| {
            suffix.is_empty()
                || suffix
                    .trim()
                    .strip_prefix('(')
                    .and_then(|count| count.strip_suffix(')'))
                    .is_some_and(|count| {
                        !count.is_empty() && count.bytes().all(|ch| ch.is_ascii_digit())
                    })
        })
    })
}

impl WeChat {
    pub(super) async fn quote_window_is_active(
        &self,
        engine: &InputEngine,
        window: &NodeRef,
    ) -> Result<bool> {
        let title = self.atspi.name(window).await;
        let windows = engine.find_windows_by_title(&title, true)?;
        Ok(matches!(windows.as_slice(), [(id, _)] if *id == engine.active_window()?))
    }

    async fn quote_list_rows(&self, list: &NodeRef, viewport: BBox) -> Result<Vec<ListRow>> {
        let children = self
            .atspi
            .children(list)
            .await
            .context("无法读取微信消息节点")?;
        let rows = stream::iter(children)
            .map(|node| async move {
                let bounds = self.atspi.bbox(&node).await?;
                if bounds.w <= 0
                    || bounds.h <= 0
                    || bounds.y >= viewport.y + viewport.h
                    || bounds.y + bounds.h <= viewport.y
                {
                    return None;
                }
                let name = self.atspi.name(&node).await;
                let label = normalized_quote_text(&name);
                let reply_body = name
                    .rsplit_once("\n引用 ")
                    .filter(|(_, preview)| preview.contains(" 的消息 : "))
                    .map(|(body, _)| normalized_quote_text(body));
                Some(ListRow {
                    node,
                    label,
                    reply_body,
                    bounds,
                })
            })
            .buffered(16)
            .filter_map(|row| async move { row })
            .collect()
            .await;
        Ok(rows)
    }

    pub(super) async fn quote_point(
        &self,
        window: &NodeRef,
        target: &NativeQuoteTarget,
        evidence: &QuoteEvidence,
        search_jump: Option<SearchJump>,
    ) -> Result<Option<QuotePoint>> {
        let list = self
            .visible_control(window, |role, name| {
                role == "list" && matches!(name, "消息" | "Messages")
            })
            .await
            .context("找不到目标会话消息列表")?;
        let viewport = self
            .atspi
            .bbox(&list)
            .await
            .context("无法读取消息列表坐标")?;
        let rows = self.quote_list_rows(&list, viewport).await?;
        let mut clock = None;
        let mut bubbles = Vec::new();
        for row in &rows {
            if is_time_separator(row) {
                clock = Some(row.label.as_str());
            } else {
                bubbles.push(Bubble {
                    clock: clock.take(),
                    row,
                });
            }
        }
        let jump = search_jump.and_then(|jump| {
            let seed = &evidence.entries[jump.seed];
            bubbles
                .iter()
                .position(|bubble| {
                    seed.matches_bubble(bubble.row)
                        && bubble.row.bounds.center().1.abs_diff(viewport.center().1) <= 2
                })
                .map(|position| (position, jump))
        });
        Ok(evidence.candidate(&bubbles, jump).and_then(|index| {
            let row = bubbles[index].row;
            quote_click_point(row.bounds, viewport, target.is_self).map(|coordinates| QuotePoint {
                coordinates,
                path: row.node.path.to_string(),
                bounds: row.bounds,
            })
        }))
    }

    pub(super) async fn find_quote_in_history(
        &self,
        engine: &mut InputEngine,
        app: &NodeRef,
        window: &NodeRef,
        to: &str,
        target: &NativeQuoteTarget,
        evidence: &QuoteEvidence,
    ) -> Result<SearchJump> {
        let existing = self
            .visible_control(app, |role, name| {
                role == "frame" && is_history_title(name, to)
            })
            .await;
        let history = if let Some(history) = existing.as_ref() {
            let title = self.atspi.name(history).await;
            ensure!(
                engine.activate_window_by_title(&title, true)?,
                "无法打开目标聊天记录"
            );
            history.clone()
        } else {
            ensure!(
                self.quote_window_is_active(engine, window).await?,
                "微信主窗口已失去焦点"
            );
            let button = if let Some(button) = self
                .visible_control(window, |role, name| {
                    role == "push button" && matches!(name, "聊天记录" | "Chat History")
                })
                .await
            {
                button
            } else {
                let info = self
                    .visible_control(window, |role, name| {
                        role == "push button" && matches!(name, "聊天信息" | "Chat Info")
                    })
                    .await
                    .context("找不到微信聊天记录入口")?;
                self.click_history_control(engine, &info).await?;
                sleep(ms(200)).await;
                self.visible_control(window, |role, name| {
                    matches!(role, "push button" | "label")
                        && matches!(
                            name,
                            "查找聊天内容" | "查找聊天记录" | "Search Chat History"
                        )
                })
                .await
                .context("聊天信息中找不到查找聊天内容")?
            };
            self.atspi.ensure_no_logout_dialog(app).await?;
            self.click_history_control(engine, &button).await?;
            let deadline = Instant::now() + ms(1500);
            loop {
                if let Some(history) = self
                    .visible_control(app, |role, name| {
                        role == "frame" && is_history_title(name, to)
                    })
                    .await
                {
                    break history;
                }
                ensure!(Instant::now() < deadline, "微信未打开目标会话的聊天记录");
                sleep(ms(50)).await;
            }
        };
        let title = self.atspi.name(&history).await;
        let result = self
            .search_quote_results(engine, app, window, &history, target, evidence)
            .await;
        let windows = engine.find_windows_by_title(&title, true)?;
        if windows.len() == 1 {
            ensure!(
                engine.close_window_by_title(&title, true)?,
                "无法关闭目标聊天记录"
            );
        }
        let index = result?;
        ensure!(
            self.quote_window_is_active(engine, window).await?,
            "聊天记录定位后微信未返回主窗口"
        );
        tracing::info!("🔎 微信聊天记录已定位引用目标 {}", target.server_id);
        Ok(index)
    }

    async fn click_history_control(&self, engine: &mut InputEngine, node: &NodeRef) -> Result<()> {
        let bounds = self
            .atspi
            .bbox(node)
            .await
            .context("无法读取聊天记录控件位置")?;
        ensure!(
            engine.rectangle_on_screen(bounds.x, bounds.y, bounds.w, bounds.h)?,
            "微信聊天记录控件不在屏幕内"
        );
        let (x, y) = bounds.center();
        engine.click(x, y).await
    }

    async fn search_quote_results(
        &self,
        engine: &mut InputEngine,
        app: &NodeRef,
        window: &NodeRef,
        history: &NodeRef,
        target: &NativeQuoteTarget,
        evidence: &QuoteEvidence,
    ) -> Result<SearchJump> {
        let title = self.atspi.name(history).await;
        let query = self
            .visible_control(history, |role, name| {
                matches!(role, "entry" | "text") && matches!(name, "搜索" | "Search")
            })
            .await
            .context("找不到微信聊天记录搜索框")?;
        for (index, seed) in evidence.seeds() {
            let keyword: String = seed
                .text
                .split(|character: char| !(character.is_alphanumeric() || character == '_'))
                .filter(|part| !part.is_empty())
                .max_by_key(|part| part.chars().count())
                .unwrap_or(&seed.text)
                .chars()
                .take(32)
                .collect();
            ensure!(
                engine.activate_window_by_title(&title, true)?,
                "无法聚焦微信聊天记录"
            );
            let deadline = Instant::now() + ms(700);
            while !self.quote_window_is_active(engine, history).await? {
                ensure!(Instant::now() < deadline, "微信聊天记录未获得焦点");
                sleep(ms(40)).await;
            }
            ensure!(
                self.atspi.grab_focus(&query).await
                    && self.atspi.set_text(&query, &keyword).await
                    && self.atspi.text(&query).await.as_deref() == Some(keyword.as_str()),
                "微信聊天记录未接受搜索词"
            );
            sleep(ms(300)).await;
            let deadline = Instant::now() + ms(4200);
            let mut previous_page = None;
            loop {
                ensure!(
                    self.quote_window_is_active(engine, history).await?,
                    "微信聊天记录已失去焦点"
                );
                let list = self
                    .visible_control(history, |role, name| {
                        role == "list" && matches!(name, "聊天记录" | "Chat History")
                    })
                    .await;
                let Some(list) = list else {
                    if Instant::now() >= deadline {
                        break;
                    }
                    sleep(ms(120)).await;
                    continue;
                };
                let viewport = self
                    .atspi
                    .bbox(&list)
                    .await
                    .context("无法读取聊天记录结果范围")?;
                let rows = self.quote_list_rows(&list, viewport).await?;
                let mut hits = rows
                    .iter()
                    .filter(|row| search_result_matches(&row.label, seed));
                if let Some(row) = hits.find(|row| locate_link(row.bounds, viewport).is_some()) {
                    let ranked = ranked_search_result(&rows, viewport, evidence, index);
                    let selected = ranked.unwrap_or(row);
                    let jump = SearchJump {
                        seed: index,
                        exact: ranked.is_some()
                            || evidence
                                .entries
                                .iter()
                                .filter(|entry| {
                                    entry.minute == seed.minute && entry.text == seed.text
                                })
                                .count()
                                == 1,
                    };
                    if locate_link(selected.bounds, viewport).is_some() {
                        self.jump_to_history_row(engine, app, window, history, &list, selected)
                            .await?;
                        if self
                            .quote_point(window, target, evidence, Some(jump))
                            .await?
                            .is_some()
                        {
                            return Ok(jump);
                        }
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    break;
                }
                if rows.is_empty() {
                    sleep(ms(120)).await;
                    continue;
                }
                let page: Vec<_> = rows
                    .into_iter()
                    .map(|row| (row.label, row.bounds.y))
                    .collect();
                if previous_page.as_ref() == Some(&page) {
                    break;
                }
                previous_page = Some(page);
                let (x, y) = viewport.center();
                engine.scroll(x, y, -3).await?;
                sleep(ms(180)).await;
            }
        }
        bail!(
            "无法用微信聊天记录唯一核对引用目标 {}，已取消发送",
            target.server_id
        );
    }

    async fn jump_to_history_row(
        &self,
        engine: &mut InputEngine,
        app: &NodeRef,
        window: &NodeRef,
        history: &NodeRef,
        list: &NodeRef,
        row: &ListRow,
    ) -> Result<()> {
        let viewport = self
            .atspi
            .bbox(list)
            .await
            .context("聊天记录结果列表已改变")?;
        let point = locate_link(row.bounds, viewport).context("定位到聊天位置按钮不在可见区域")?;
        ensure!(
            engine.rectangle_on_screen(point.0, point.1, 1, 1)?,
            "定位到聊天位置按钮不在屏幕内"
        );
        engine.move_mouse(point.0, point.1).await?;
        sleep(ms(120)).await;
        self.atspi.ensure_no_logout_dialog(app).await?;
        ensure!(
            self.quote_window_is_active(engine, history).await?
                && self.atspi.bbox(&row.node).await == Some(row.bounds)
                && normalized_quote_text(&self.atspi.name(&row.node).await) == row.label,
            "微信聊天记录搜索结果已改变"
        );
        engine.click_at_pointer().await?;
        let deadline = Instant::now() + ms(1300);
        while !self.quote_window_is_active(engine, window).await? {
            ensure!(Instant::now() < deadline, "微信搜索结果没有跳回目标聊天");
            sleep(ms(50)).await;
        }
        sleep(ms(150)).await;
        Ok(())
    }
}

fn ranked_search_result<'a>(
    rows: &'a [ListRow],
    viewport: BBox,
    evidence: &QuoteEvidence,
    seed_index: usize,
) -> Option<&'a ListRow> {
    let seed = &evidence.entries[seed_index];
    let matching_entries: Vec<_> = evidence
        .entries
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| entry.minute == seed.minute && entry.text == seed.text)
        .collect();
    if matching_entries.len() < 2 || matching_entries.iter().any(|(_, entry)| !entry.searchable) {
        return None;
    }
    let first = rows
        .iter()
        .position(|row| search_result_matches(&row.label, seed))?;
    let count = rows[first..]
        .iter()
        .take_while(|row| search_result_matches(&row.label, seed))
        .count();
    let starts_after_another_result = first
        .checked_sub(1)
        .and_then(|index| rows.get(index))
        .is_some_and(|previous| previous.bounds.y >= viewport.y);
    let ends_before_another_result = rows
        .get(first + count)
        .is_some_and(|next| next.bounds.y < viewport.y + viewport.h);
    if !starts_after_another_result
        || !ends_before_another_result
        || count != matching_entries.len()
    {
        return None;
    }
    let position = matching_entries
        .iter()
        .position(|(index, _)| *index == seed_index)?;
    rows.get(first + position)
}

fn locate_link(row: BBox, viewport: BBox) -> Option<(i32, i32)> {
    let point = (row.x + row.w - 65, row.y + 46);
    (row.w >= 300
        && row.h > 46
        && point.0 > viewport.x
        && point.0 < viewport.x + viewport.w
        && point.1 > viewport.y
        && point.1 < viewport.y + viewport.h)
        .then_some(point)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(history: &[(&str, i64)], target: usize) -> QuoteEvidence {
        QuoteEvidence {
            entries: history
                .iter()
                .map(|(text, minute)| HistoryEntry {
                    text: (*text).into(),
                    minute: *minute,
                    time_labels: vec![format!("10:{minute:02}")],
                    searchable: true,
                    is_reply: false,
                    is_forward: false,
                })
                .collect(),
            target,
            complete_minutes: 10..20,
        }
    }

    fn row(label: &str, top: i32) -> ListRow {
        ListRow {
            node: NodeRef {
                bus: String::new(),
                path: "/row".try_into().unwrap(),
            },
            label: label.into(),
            reply_body: None,
            bounds: BBox {
                x: 0,
                y: top,
                w: 560,
                h: 68,
            },
        }
    }

    #[test]
    fn history_window_title_identifies_group_and_private_chats() {
        assert!(is_history_title("\"测试裙\"的聊天记录(4)", "测试裙"));
        assert!(is_history_title(
            "与\"文件传输助手\"的聊天记录",
            "文件传输助手"
        ));
        assert!(!is_history_title("\"测试裙2\"的聊天记录(4)", "测试裙"));
    }

    #[test]
    fn forward_cards_with_identical_previews_are_anchored_by_the_chat_timeline() {
        let title = "群聊的聊天记录";
        let mut evidence = entries(&[(title, 12), (title, 14)], 1);
        for entry in &mut evidence.entries {
            entry.is_forward = true;
        }
        let first = row("聊天记录群聊的聊天记录群友: [图片]", 10);
        let second = row(&first.label, 78);
        let bubbles = [
            Bubble {
                clock: Some("10:12"),
                row: &first,
            },
            Bubble {
                clock: Some("10:14"),
                row: &second,
            },
        ];
        assert_eq!(evidence.candidate(&bubbles, None), Some(1));
        assert_eq!(evidence.candidate(&bubbles[..1], None), None);
    }

    #[test]
    fn visible_context_selects_the_requested_occurrence() {
        let evidence = entries(&[("前文", 12), ("好", 12), ("好", 12), ("后文", 12)], 2);
        let rows = [
            row("前文", 10),
            row("好", 78),
            row("好", 146),
            row("后文", 214),
        ];
        let bubbles: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| Bubble {
                clock: (index == 0).then_some("10:12"),
                row,
            })
            .collect();
        assert_eq!(evidence.candidate(&bubbles, None), Some(2));
        let ambiguous = [Bubble {
            clock: None,
            row: &rows[2],
        }];
        assert_eq!(evidence.candidate(&ambiguous, None), None);
        assert_eq!(
            evidence.candidate(
                &ambiguous,
                Some((
                    0,
                    SearchJump {
                        seed: 2,
                        exact: false
                    }
                ))
            ),
            None
        );
    }

    #[test]
    fn same_minute_search_results_use_history_order_only_if_all_are_present() {
        let evidence = entries(&[("好", 12), ("好", 12), ("好", 12)], 1);
        let rows = [
            row("好 10:13", 0),
            row("好 10:12", 68),
            row("好 10:12", 136),
            row("好 10:12", 204),
            row("好 10:11", 272),
        ];
        let viewport = BBox {
            x: 0,
            y: 0,
            w: 560,
            h: 340,
        };
        assert_eq!(
            ranked_search_result(&rows, viewport, &evidence, 0)
                .unwrap()
                .bounds
                .y,
            204
        );
        assert_eq!(
            ranked_search_result(&rows, viewport, &evidence, 1)
                .unwrap()
                .bounds
                .y,
            136
        );
        assert!(ranked_search_result(&rows[1..], viewport, &evidence, 1).is_none());
        assert!(ranked_search_result(&rows[1..4], viewport, &evidence, 1).is_none());
    }

    #[test]
    fn a_historical_jump_uses_the_minute_and_checks_surrounding_text() {
        let evidence = entries(&[("前文", 12), ("好", 12), ("后文", 12), ("好", 13)], 1);
        let row = row("好", 30);
        let bubbles = [Bubble {
            clock: None,
            row: &row,
        }];
        assert_eq!(
            evidence.candidate(
                &bubbles,
                Some((
                    0,
                    SearchJump {
                        seed: 1,
                        exact: true
                    }
                ))
            ),
            Some(0)
        );
        assert_eq!(
            evidence.candidate(
                &bubbles,
                Some((
                    0,
                    SearchJump {
                        seed: 3,
                        exact: true
                    }
                ))
            ),
            None
        );
    }

    #[test]
    fn a_native_reply_in_the_visible_context_preserves_alignment() {
        let mut evidence = entries(&[("问题", 12), ("答复", 13), ("看到了", 13)], 2);
        let mut reply = row("答复 引用 小李 的消息 : 问题", 78);
        reply.reply_body = Some("答复".into());
        let rows = [row("问题", 10), reply, row("看到了", 146)];
        let bubbles: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| Bubble {
                clock: (index == 0).then_some("10:12"),
                row,
            })
            .collect();
        assert_eq!(evidence.candidate(&bubbles, None), None);
        evidence.entries[1].is_reply = true;
        assert_eq!(evidence.candidate(&bubbles, None), Some(2));
    }
}
