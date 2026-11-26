//! X11 XTEST 输入引擎
//!
//! 通过 x11rb 使用 X11 XTEST 扩展注入键盘和鼠标事件。
//! 中文输入通过 xclip（剪贴板）+ Ctrl+V 实现。

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, KeyButMask, Keycode,
    MapState,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::typing::{character_delay_ms, typing_prefix, unicode_keysym, TypingConfig};

mod motion;

/// X11 事件类型
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

/// 延迟常量 (ms)
const KEY_HOLD_MS: u64 = 30;
const TYPING_DELAY_MS: u64 = 20;
const CLICK_HOLD_MS: u64 = 50;

/// X11 Keysym 常量
mod keysym {
    pub const XK_SPACE: u32 = 0x0020;
    pub const XK_RETURN: u32 = 0xFF0D;
    pub const XK_ESCAPE: u32 = 0xFF1B;
    pub const XK_TAB: u32 = 0xFF09;
    pub const XK_BACKSPACE: u32 = 0xFF08;
    pub const XK_DELETE: u32 = 0xFFFF;
    pub const XK_HOME: u32 = 0xFF50;
    pub const XK_END: u32 = 0xFF57;
    pub const XK_LEFT: u32 = 0xFF51;
    pub const XK_UP: u32 = 0xFF52;
    pub const XK_RIGHT: u32 = 0xFF53;
    pub const XK_DOWN: u32 = 0xFF54;
    pub const XK_SHIFT_L: u32 = 0xFFE1;
    pub const XK_CONTROL_L: u32 = 0xFFE3;
    pub const XK_ALT_L: u32 = 0xFFE4;
    pub const XK_F1: u32 = 0xFFBE;
    pub const XK_F2: u32 = 0xFFBF;
    pub const XK_F3: u32 = 0xFFC0;
    pub const XK_F4: u32 = 0xFFC1;
    pub const XK_F5: u32 = 0xFFC2;
}

/// X11 XTEST 输入引擎
pub struct InputEngine {
    conn: RustConnection,
    screen_root: u32,
    min_keycode: Keycode,
    max_keycode: Keycode,
    keysyms_per_keycode: u8,
    keysyms: Vec<u32>,
    typing: TypingConfig,
    // 缓存的 X11 Atom (在 X11 Session 内永不变, 启动时一次性 intern)
    atom_net_wm_name: u32,
    atom_utf8_string: u32,
    atom_net_client_list: u32,
    atom_net_active_window: u32,
    atom_net_close_window: u32,
}

impl InputEngine {
    /// 创建输入引擎
    pub fn new(typing: TypingConfig) -> Result<Self> {
        info!("🎮 初始化 X11 XTEST 输入引擎...");

        let display_env = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());
        let (conn, screen_num) = RustConnection::connect(Some(&display_env))
            .context(format!("连接 X11 失败 (DISPLAY={display_env})"))?;

        let screen = &conn.setup().roots[screen_num];
        let screen_root = screen.root;

        // 验证 XTEST 扩展
        x11rb::protocol::xtest::get_version(&conn, 2, 2)
            .context("XTEST 扩展不可用")?
            .reply()
            .context("XTEST 版本查询失败")?;

        // 获取键盘映射
        let setup = conn.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let reply = conn
            .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)?
            .reply()
            .context("获取键盘映射失败")?;

        let keysyms_per_keycode = reply.keysyms_per_keycode;
        let keysyms: Vec<u32> = reply.keysyms.iter().map(|k| (*k).into()).collect();

        // 一次性 intern 所有需要的 Atom (避免每次调用重复查询)
        let atom_net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let atom_utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let atom_net_client_list = conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
        let atom_net_active_window = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        let atom_net_close_window = conn.intern_atom(false, b"_NET_CLOSE_WINDOW")?.reply()?.atom;

        info!("✅ X11 XTEST 就绪 (DISPLAY={display_env}, keycodes={min_keycode}~{max_keycode})");

        Ok(Self {
            conn,
            screen_root,
            min_keycode,
            max_keycode,
            keysyms_per_keycode,
            keysyms,
            typing,
            atom_net_wm_name,
            atom_utf8_string,
            atom_net_client_list,
            atom_net_active_window,
            atom_net_close_window,
        })
    }

    // =================================================================
    // Keysym 查找
    // =================================================================

    fn keysym_to_keycode(&self, keysym: u32) -> Option<(Keycode, bool)> {
        let per = self.keysyms_per_keycode as usize;
        let total = (self.max_keycode - self.min_keycode + 1) as usize;

        for i in 0..total {
            for j in 0..per {
                if self.keysyms[i * per + j] == keysym {
                    let keycode = self.min_keycode + i as u8;
                    let need_shift = j == 1;
                    return Some((keycode, need_shift));
                }
            }
        }
        None
    }

    fn char_to_keysym(ch: char) -> Option<u32> {
        match ch {
            ' ' => Some(keysym::XK_SPACE),
            '\n' => Some(keysym::XK_RETURN),
            '\t' => Some(keysym::XK_TAB),
            c if c.is_ascii() => Some(c as u32),
            _ => None,
        }
    }

    fn key_name_to_keysym(name: &str) -> Option<u32> {
        match name.to_lowercase().as_str() {
            "return" | "enter" => Some(keysym::XK_RETURN),
            "escape" | "esc" => Some(keysym::XK_ESCAPE),
            "tab" => Some(keysym::XK_TAB),
            "backspace" => Some(keysym::XK_BACKSPACE),
            "delete" => Some(keysym::XK_DELETE),
            "space" => Some(keysym::XK_SPACE),
            "home" => Some(keysym::XK_HOME),
            "end" => Some(keysym::XK_END),
            "left" => Some(keysym::XK_LEFT),
            "right" => Some(keysym::XK_RIGHT),
            "up" => Some(keysym::XK_UP),
            "down" => Some(keysym::XK_DOWN),
            "shift" => Some(keysym::XK_SHIFT_L),
            "ctrl" | "control" => Some(keysym::XK_CONTROL_L),
            "alt" => Some(keysym::XK_ALT_L),
            "f1" => Some(keysym::XK_F1),
            "f2" => Some(keysym::XK_F2),
            "f3" => Some(keysym::XK_F3),
            "f4" => Some(keysym::XK_F4),
            "f5" => Some(keysym::XK_F5),
            s if s.len() == 1 => Self::char_to_keysym(s.chars().next()?),
            _ => None,
        }
    }

    // =================================================================
    // 底层 XTEST 操作
    // =================================================================

    fn raw_key_press(&self, keycode: Keycode) -> Result<()> {
        self.conn
            .xtest_fake_input(KEY_PRESS, keycode, 0, self.screen_root, 0, 0, 0)?
            .check()
            .context("XTEST key press was rejected")?;
        self.conn.flush()?;
        Ok(())
    }

    fn raw_key_release(&self, keycode: Keycode) -> Result<()> {
        self.conn
            .xtest_fake_input(KEY_RELEASE, keycode, 0, self.screen_root, 0, 0, 0)?
            .check()
            .context("XTEST key release was rejected")?;
        self.conn.flush()?;
        Ok(())
    }

    // =================================================================
    // 键盘操作
    // =================================================================

    /// 模拟单次按键
    pub async fn press_key(&mut self, key_name: &str) -> Result<()> {
        let ks = Self::key_name_to_keysym(key_name)
            .ok_or_else(|| anyhow::anyhow!("未知按键: {key_name}"))?;
        let (keycode, need_shift) = self
            .keysym_to_keycode(ks)
            .ok_or_else(|| anyhow::anyhow!("按键无映射: {key_name}"))?;

        // Shift
        let shift_kc = if need_shift {
            self.keysym_to_keycode(keysym::XK_SHIFT_L).map(|(kc, _)| kc)
        } else {
            None
        };
        if let Some(skc) = shift_kc {
            self.raw_key_press(skc)?;
        }

        self.raw_key_press(keycode)?;
        tokio::time::sleep(std::time::Duration::from_millis(KEY_HOLD_MS)).await;
        self.raw_key_release(keycode)?;

        if let Some(skc) = shift_kc {
            self.raw_key_release(skc)?;
        }

        debug!("⌨️ press_key: {key_name}");
        Ok(())
    }

    /// 组合键 (如 "ctrl+f", "ctrl+v", "ctrl+a")
    pub async fn key_combo(&mut self, combo: &str) -> Result<()> {
        let parts: Vec<&str> = combo.split('+').collect();
        let mut keycodes = Vec::new();

        for part in &parts {
            let ks = Self::key_name_to_keysym(part.trim())
                .ok_or_else(|| anyhow::anyhow!("未知按键: {part}"))?;
            let (kc, _) = self
                .keysym_to_keycode(ks)
                .ok_or_else(|| anyhow::anyhow!("按键无映射: {part}"))?;
            keycodes.push(kc);
        }

        // 按顺序按下
        for &kc in &keycodes {
            self.raw_key_press(kc)?;
            tokio::time::sleep(std::time::Duration::from_millis(KEY_HOLD_MS)).await;
        }
        // 逆序释放
        for &kc in keycodes.iter().rev() {
            self.raw_key_release(kc)?;
        }

        debug!("⌨️ key_combo: {combo}");
        Ok(())
    }

    /// 逐字输入 ASCII 文本 (中文请用 paste_text)
    pub async fn type_text(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            let ks = Self::char_to_keysym(ch)
                .ok_or_else(|| anyhow::anyhow!("字符无映射: '{ch}' — 请用 paste_text"))?;
            let (keycode, need_shift) = self
                .keysym_to_keycode(ks)
                .ok_or_else(|| anyhow::anyhow!("字符无 keycode: '{ch}'"))?;

            let shift_kc = if need_shift {
                self.keysym_to_keycode(keysym::XK_SHIFT_L).map(|(kc, _)| kc)
            } else {
                None
            };
            if let Some(skc) = shift_kc {
                self.raw_key_press(skc)?;
            }

            self.raw_key_press(keycode)?;
            tokio::time::sleep(std::time::Duration::from_millis(KEY_HOLD_MS)).await;
            self.raw_key_release(keycode)?;

            if let Some(skc) = shift_kc {
                self.raw_key_release(skc)?;
            }
            tokio::time::sleep(std::time::Duration::from_millis(TYPING_DELAY_MS)).await;
        }
        Ok(())
    }

    /// 通过剪贴板粘贴文本 (支持中文、空格等任意字符)
    pub async fn paste_text(&mut self, text: &str) -> Result<()> {
        self.clipboard_paste(text).await
    }

    pub(crate) async fn copy_composer_text(&mut self) -> Result<String> {
        self.key_combo("ctrl+a").await?;
        self.key_combo("ctrl+c").await?;
        let output = tokio::time::timeout(
            std::time::Duration::from_millis(700),
            tokio::process::Command::new("xclip")
                .args(["-selection", "clipboard", "-target", "UTF8_STRING", "-o"])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("读取微信输入框剪贴板超时")??;
        self.key_combo("ctrl+end").await?;
        anyhow::ensure!(output.status.success(), "读取微信输入框剪贴板失败");
        String::from_utf8(output.stdout).context("微信输入框剪贴板内容不是 UTF-8")
    }

    pub async fn input_message_text(&mut self, text: &str) -> Result<()> {
        let count = text.chars().count();
        if !self.typing.should_type(text) {
            info!(mode = "paste", chars = count, "⌨️ 消息输入策略");
            return self.paste_text(text).await;
        }
        let Some(mut mapping) = TemporaryKeymap::new(&self.conn, text)? else {
            warn!(
                mode = "paste",
                chars = count,
                "没有可用空闲键码，改用整段粘贴"
            );
            return self.paste_text(text).await;
        };
        let started = std::time::Instant::now();
        info!(mode = "type", chars = count, "⌨️ 消息输入策略");
        let mut remaining = text;
        let mut index = 0;
        let mut batches = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(35)).await;
            let prefix_bytes = mapping.prefix_bytes;
            for character in remaining[..prefix_bytes].chars() {
                let keycode = mapping.keycode(character);
                let mut randomness = [128_u8; 2];
                let _ = getrandom::fill(&mut randomness);
                self.raw_key_press(keycode)?;
                tokio::time::sleep(std::time::Duration::from_millis(
                    15 + u64::from(randomness[0]) * 9 / 255,
                ))
                .await;
                self.raw_key_release(keycode)?;
                let delay = character_delay_ms(index, count, character, randomness[1]);
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                index += 1;
            }
            tokio::time::sleep(std::time::Duration::from_millis(35)).await;
            mapping.restore()?;
            batches += 1;
            remaining = &remaining[prefix_bytes..];
            if remaining.is_empty() {
                break;
            }
            mapping = TemporaryKeymap::new(&self.conn, remaining)?.ok_or_else(|| {
                anyhow::anyhow!("逐字输入期间空闲键码不可用，已保留草稿并停止发送")
            })?;
        }
        info!(
            mode = "type",
            chars = count,
            batches,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "⌨️ 逐字输入完成"
        );
        Ok(())
    }

    async fn clipboard_paste(&mut self, text: &str) -> Result<()> {
        info!("📋 粘贴文本: {} 字符", text.len());

        // 写入剪贴板 (异步, 不阻塞 tokio 线程)
        let mut child = tokio::process::Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("启动 xclip 失败")?;

        if let Some(ref mut stdin) = child.stdin {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(text.as_bytes()).await?;
        }
        child.wait().await.context("xclip 执行失败")?;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Ctrl+V 粘贴
        self.key_combo("ctrl+v").await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        Ok(())
    }

    /// 通过剪贴板粘贴图片文件 (xclip + Ctrl+V)
    pub async fn paste_image(&mut self, image_path: &str) -> Result<()> {
        info!("🖼️ 粘贴图片: {}", image_path);

        // 检测 MIME 类型
        let mime = if image_path.ends_with(".png") {
            "image/png"
        } else if image_path.ends_with(".jpg") || image_path.ends_with(".jpeg") {
            "image/jpeg"
        } else if image_path.ends_with(".gif") {
            "image/gif"
        } else if image_path.ends_with(".bmp") {
            "image/bmp"
        } else if image_path.ends_with(".webp") {
            "image/webp"
        } else {
            "image/png" // 默认 PNG
        };

        // xclip -selection clipboard -t image/png -i /path/to/image (异步)
        let status = tokio::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", mime, "-i", image_path])
            .status()
            .await
            .context("启动 xclip 失败 (图片)")?;

        if !status.success() {
            anyhow::bail!("xclip 图片复制失败: exit={:?}", status.code());
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Ctrl+V 粘贴
        self.key_combo("ctrl+v").await?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        Ok(())
    }

    pub async fn paste_sticker(&mut self, image_path: &str) -> Result<()> {
        anyhow::ensure!(
            image_path.starts_with("/tmp/mimicwx_img_")
                && image_path.ends_with(".gif")
                && image_path.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'.')
                }),
            "原生表情路径无效"
        );
        let mut child = tokio::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "text/uri-list", "-i"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("启动 xclip 失败 (原生表情)")?;
        let mut stdin = child.stdin.take().context("无法写入表情文件地址")?;
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(format!("file://{image_path}\r\n").as_bytes())
            .await?;
        drop(stdin);
        anyhow::ensure!(child.wait().await?.success(), "复制原生表情文件地址失败");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        self.key_combo("ctrl+v").await?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        Ok(())
    }

    // =================================================================
    // 鼠标操作
    // =================================================================

    /// 鼠标移动到绝对坐标
    pub async fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        let target = (
            i16::try_from(x).context("鼠标 X 坐标超出 XTEST 范围")?,
            i16::try_from(y).context("鼠标 Y 坐标超出 XTEST 范围")?,
        );
        let pointer_request = self.conn.query_pointer(self.screen_root)?;
        let screen_request = self.conn.get_geometry(self.screen_root)?;
        let pointer = pointer_request.reply().context("无法读取鼠标当前位置")?;
        let screen = screen_request.reply().context("无法读取 X11 屏幕尺寸")?;
        anyhow::ensure!(pointer.same_screen, "鼠标不在目标 X11 屏幕上");
        anyhow::ensure!(
            x >= 0 && y >= 0 && x < i32::from(screen.width) && y < i32::from(screen.height),
            "鼠标目标不在屏幕范围内: ({x}, {y})"
        );
        let buttons = KeyButMask::BUTTON1
            | KeyButMask::BUTTON2
            | KeyButMask::BUTTON3
            | KeyButMask::BUTTON4
            | KeyButMask::BUTTON5;
        anyhow::ensure!(
            u16::from(pointer.mask & buttons) == 0,
            "鼠标按键仍被按下，停止移动以避免意外拖动"
        );
        let start = (pointer.root_x, pointer.root_y);
        let mut variation = [128_u8; 2];
        let _ = getrandom::fill(&mut variation);
        let steps = motion::plan_motion(start, target, (screen.width, screen.height), variation);
        let started = tokio::time::Instant::now();
        for step in &steps {
            tokio::time::sleep_until(started + std::time::Duration::from_millis(step.at_ms)).await;
            self.conn
                .xtest_fake_input(MOTION_NOTIFY, 0, 0, self.screen_root, step.x, step.y, 0)?
                .check()
                .context("XTEST pointer motion was rejected")?;
            self.conn.flush()?;
        }
        debug!(
            "🖱️ 平滑移动完成: ({}, {}) -> ({x}, {y}), steps={}, elapsed_ms={}",
            start.0,
            start.1,
            steps.len(),
            started.elapsed().as_millis()
        );
        Ok(())
    }

    pub fn rectangle_on_screen(&self, x: i32, y: i32, width: i32, height: i32) -> Result<bool> {
        let screen = self.conn.get_geometry(self.screen_root)?.reply()?;
        Ok(x >= 0
            && y >= 0
            && width > 0
            && height > 0
            && x.saturating_add(width) <= i32::from(screen.width)
            && y.saturating_add(height) <= i32::from(screen.height))
    }

    /// 鼠标单击
    pub async fn click(&mut self, x: i32, y: i32) -> Result<()> {
        self.move_mouse(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.click_at_pointer().await?;
        debug!("🖱️ click: ({x}, {y})");
        Ok(())
    }

    pub(crate) async fn click_at_pointer(&self) -> Result<()> {
        // 按下左键
        self.conn
            .xtest_fake_input(BUTTON_PRESS, 1, 0, self.screen_root, 0, 0, 0)?
            .check()
            .context("XTEST button press was rejected")?;
        self.conn.flush()?;
        tokio::time::sleep(std::time::Duration::from_millis(CLICK_HOLD_MS)).await;

        // 释放左键
        self.conn
            .xtest_fake_input(BUTTON_RELEASE, 1, 0, self.screen_root, 0, 0, 0)?
            .check()
            .context("XTEST button release was rejected")?;
        self.conn.flush()?;

        Ok(())
    }

    /// 鼠标双击
    pub async fn double_click(&mut self, x: i32, y: i32) -> Result<()> {
        self.click(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        self.click(x, y).await?;
        Ok(())
    }

    /// 鼠标右键点击
    pub async fn right_click(&mut self, x: i32, y: i32) -> Result<()> {
        self.move_mouse(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.conn
            .xtest_fake_input(BUTTON_PRESS, 3, 0, self.screen_root, 0, 0, 0)?
            .check()
            .context("XTEST button press was rejected")?;
        self.conn.flush()?;
        tokio::time::sleep(std::time::Duration::from_millis(CLICK_HOLD_MS)).await;

        self.conn
            .xtest_fake_input(BUTTON_RELEASE, 3, 0, self.screen_root, 0, 0, 0)?
            .check()
            .context("XTEST button release was rejected")?;
        self.conn.flush()?;

        debug!("🖱️ right_click: ({x}, {y})");
        Ok(())
    }

    /// 鼠标滚轮 (正=上, 负=下)
    ///
    /// X11: button 4 = scroll up, button 5 = scroll down
    pub async fn scroll(&mut self, x: i32, y: i32, clicks: i32) -> Result<()> {
        self.move_mouse(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let button: u8 = if clicks > 0 { 4 } else { 5 };
        for _ in 0..clicks.unsigned_abs() {
            self.conn
                .xtest_fake_input(BUTTON_PRESS, button, 0, self.screen_root, 0, 0, 0)?
                .check()
                .context("XTEST scroll button press was rejected")?;
            self.conn
                .xtest_fake_input(BUTTON_RELEASE, button, 0, self.screen_root, 0, 0, 0)?
                .check()
                .context("XTEST scroll button release was rejected")?;
            self.conn.flush()?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        debug!("🖱️ scroll: ({x}, {y}) clicks={clicks}");
        Ok(())
    }

    // =================================================================
    // 窗口管理 (X11 原生, 替代 xdotool)
    // =================================================================

    /// 按标题搜索窗口 (EWMH _NET_CLIENT_LIST + 标题匹配)
    ///
    /// `exact=true`: 精确匹配; `exact=false`: contains 匹配
    /// 返回匹配的 (window_id, window_name) 列表
    pub fn find_windows_by_title(&self, title: &str, exact: bool) -> Result<Vec<(u32, String)>> {
        // 使用缓存的 Atom (启动时已 intern)
        let wm_name_atom = self.atom_net_wm_name;
        let utf8_atom = self.atom_utf8_string;
        let client_list_atom = self.atom_net_client_list;

        // 优先: _NET_CLIENT_LIST (WM 托管的所有顶层窗口)
        let windows: Vec<u32> = if let Ok(reply) = self
            .conn
            .get_property(
                false,
                self.screen_root,
                client_list_atom,
                u32::from(AtomEnum::WINDOW),
                0,
                4096,
            )?
            .reply()
        {
            if reply.format == 32 && !reply.value.is_empty() {
                reply
                    .value
                    .chunks_exact(4)
                    .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                    .collect()
            } else {
                // 回退: query_tree
                self.conn.query_tree(self.screen_root)?.reply()?.children
            }
        } else {
            self.conn.query_tree(self.screen_root)?.reply()?.children
        };

        let mut found = Vec::new();

        for &win in &windows {
            // 尝试 _NET_WM_NAME (UTF-8), 回退 WM_NAME
            let name = if let Ok(reply) = self
                .conn
                .get_property(false, win, wm_name_atom, utf8_atom, 0, 1024)?
                .reply()
            {
                if reply.value.is_empty() {
                    if let Ok(reply2) = self
                        .conn
                        .get_property(
                            false,
                            win,
                            u32::from(AtomEnum::WM_NAME),
                            u32::from(AtomEnum::STRING),
                            0,
                            1024,
                        )?
                        .reply()
                    {
                        String::from_utf8_lossy(&reply2.value).to_string()
                    } else {
                        continue;
                    }
                } else {
                    String::from_utf8_lossy(&reply.value).to_string()
                }
            } else {
                continue;
            };

            let matched = if exact {
                name == title
            } else {
                name.contains(title)
            };
            if matched {
                found.push((win, name));
            }
        }
        Ok(found)
    }

    pub(crate) fn active_window(&self) -> Result<u32> {
        self.conn
            .get_property(
                false,
                self.screen_root,
                self.atom_net_active_window,
                AtomEnum::WINDOW,
                0,
                1,
            )?
            .reply()?
            .value32()
            .and_then(|mut values| values.next())
            .filter(|window| *window != 0)
            .context("无法确认当前 X11 窗口")
    }

    pub(crate) async fn dismiss_wechat_member_popup(&mut self) -> Result<()> {
        let parent = self.active_window()?;
        if self.wechat_member_popup(parent)?.is_some() {
            self.key_combo("ctrl+home").await?;
            anyhow::ensure!(
                self.wechat_member_popup(parent)?.is_none(),
                "微信 @ 候选框仍然打开，已取消发送"
            );
        }
        Ok(())
    }

    pub(crate) fn wechat_member_popup(
        &self,
        parent: u32,
    ) -> Result<Option<(u32, i32, i32, u16, u16)>> {
        let mut result = None;
        for (window, _) in self.find_windows_by_title("wechat", true)? {
            let owner = self.conn.get_property(
                false,
                window,
                AtomEnum::WM_TRANSIENT_FOR,
                AtomEnum::WINDOW,
                0,
                1,
            )?;
            let attributes = self.conn.get_window_attributes(window)?;
            let geometry = self.conn.get_geometry(window)?;
            let coordinates = self
                .conn
                .translate_coordinates(window, self.screen_root, 0, 0)?;
            if !owner
                .reply()
                .ok()
                .and_then(|value| value.value32().and_then(|mut values| values.next()))
                .is_some_and(|owner| owner == parent)
                || !attributes
                    .reply()
                    .is_ok_and(|attributes| attributes.map_state == MapState::VIEWABLE)
            {
                continue;
            }
            let geometry = geometry.reply()?;
            let coordinates = coordinates.reply()?;
            anyhow::ensure!(result.is_none(), "微信出现多个 @ 候选窗口");
            result = Some((
                window,
                i32::from(coordinates.dst_x),
                i32::from(coordinates.dst_y),
                geometry.width,
                geometry.height,
            ));
        }
        Ok(result)
    }

    pub(crate) async fn select_wechat_member(&mut self, parent: u32, window: u32) -> Result<()> {
        let candidate = self
            .wechat_member_popup(parent)?
            .filter(|(id, _, _, width, height)| (*id, *width, *height) == (window, 170, 40))
            .context("微信 @ 候选框已改变")?;
        let (_, x, y, width, height) = candidate;
        anyhow::ensure!(
            self.rectangle_on_screen(x, y, i32::from(width), i32::from(height))?,
            "微信 @ 候选框超出屏幕"
        );
        self.move_mouse(x + i32::from(width) / 2, y + i32::from(height) / 2)
            .await?;
        anyhow::ensure!(
            self.wechat_member_popup(parent)? == Some(candidate),
            "移动鼠标时微信 @ 候选框已改变"
        );
        let frame = self.conn.query_tree(window)?.reply()?.parent;
        let pointer = self.conn.query_pointer(self.screen_root)?.reply()?;
        anyhow::ensure!(
            matches!(pointer.child, child if child == frame || child == window),
            "鼠标下方不是微信 @ 候选框"
        );
        self.click_at_pointer().await
    }

    /// 通过窗口标题激活指定窗口 (X11 _NET_ACTIVE_WINDOW)
    ///
    /// 返回是否成功找到并激活了窗口
    pub fn activate_window_by_title(&self, title: &str, exact: bool) -> Result<bool> {
        let windows = self.find_windows_by_title(title, exact)?;
        if let Some((win, name)) = windows.first() {
            debug!("🖱️ 激活窗口: '{name}' (wid={win})");
            let active_atom = self.atom_net_active_window;
            // _NET_ACTIVE_WINDOW: data[0]=source(1=app), data[1]=timestamp, data[2]=requestor
            let event = ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: *win,
                type_: active_atom,
                data: [1u32, 0, 0, 0, 0].into(),
            };
            self.conn.send_event(
                false,
                self.screen_root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?;
            self.conn.flush()?;
            Ok(true)
        } else {
            debug!("🖱️ 未找到标题匹配 '{title}' 的窗口");
            Ok(false)
        }
    }

    pub(crate) fn maximize_window_by_title(&self, title: &str) -> Result<bool> {
        let windows = self.find_windows_by_title(title, true)?;
        let [(window, _)] = windows.as_slice() else {
            return Ok(false);
        };
        let state = self
            .conn
            .intern_atom(false, b"_NET_WM_STATE")?
            .reply()?
            .atom;
        let horizontal = self
            .conn
            .intern_atom(false, b"_NET_WM_STATE_MAXIMIZED_HORZ")?
            .reply()?
            .atom;
        let vertical = self
            .conn
            .intern_atom(false, b"_NET_WM_STATE_MAXIMIZED_VERT")?
            .reply()?
            .atom;
        let event = ClientMessageEvent {
            response_type: xproto::CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: *window,
            type_: state,
            data: [1u32, horizontal, vertical, 1, 0].into(),
        };
        self.conn
            .send_event(
                false,
                self.screen_root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?
            .check()?;
        self.conn.flush()?;
        Ok(true)
    }

    /// 通过窗口标题关闭指定窗口 (X11 _NET_CLOSE_WINDOW)
    pub fn close_window_by_title(&self, title: &str, exact: bool) -> Result<bool> {
        let windows = self.find_windows_by_title(title, exact)?;
        if let Some((win, name)) = windows.first() {
            info!("🗑️ 关闭窗口: '{name}' (匹配 '{title}')");
            let close_atom = self.atom_net_close_window;
            let event = ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: *win,
                type_: close_atom,
                data: [0u32; 5].into(),
            };
            self.conn.send_event(
                false,
                self.screen_root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?;
            self.conn.flush()?;
            Ok(true)
        } else {
            debug!("🗑️ 未找到标题包含 '{title}' 的窗口");
            Ok(false)
        }
    }

    /// 发送 Enter 键
    pub async fn press_enter(&mut self) -> Result<()> {
        self.press_key("Return").await
    }
}

struct TemporaryKeymap<'connection> {
    conn: &'connection RustConnection,
    root: u32,
    entries: Vec<(char, Keycode, Vec<u32>)>,
    prefix_bytes: usize,
}

impl<'connection> TemporaryKeymap<'connection> {
    fn new(conn: &'connection RustConnection, text: &str) -> Result<Option<Self>> {
        let setup = conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - first + 1;
        let mapping = conn.get_keyboard_mapping(first, count)?.reply()?;
        let modifiers = conn.get_modifier_mapping()?.reply()?;
        let pressed = conn.query_keymap()?.reply()?;
        let width = mapping.keysyms_per_keycode as usize;
        anyhow::ensure!(width > 0, "X11 返回了空键盘映射");
        let available: Vec<_> = mapping
            .keysyms
            .chunks_exact(width)
            .enumerate()
            .filter_map(|(index, row)| {
                let keycode = first + index as u8;
                let down = pressed.keys[keycode as usize / 8] & (1 << (keycode % 8)) != 0;
                (row.iter().all(|keysym| *keysym == 0)
                    && !modifiers.keycodes.contains(&keycode)
                    && !down)
                    .then_some((keycode, row.to_vec()))
            })
            .collect();
        let prefix = typing_prefix(text, available.len());
        if prefix.is_empty() {
            return Ok(None);
        }
        let mut characters = Vec::new();
        for character in prefix.chars() {
            if !characters.contains(&character) {
                characters.push(character);
            }
        }
        let mut temporary = Self {
            conn,
            root: setup.roots[0].root,
            entries: Vec::new(),
            prefix_bytes: prefix.len(),
        };
        for (character, (keycode, original)) in characters.into_iter().zip(available) {
            temporary.entries.push((character, keycode, original));
            conn.change_keyboard_mapping(
                1,
                keycode,
                mapping.keysyms_per_keycode,
                &vec![unicode_keysym(character); width],
            )?
            .check()
            .context("无法创建 Unicode 输入键码")?;
        }
        conn.flush()?;
        Ok(Some(temporary))
    }

    fn keycode(&self, character: char) -> Keycode {
        self.entries
            .iter()
            .find(|entry| entry.0 == character)
            .expect("message characters were mapped before typing")
            .1
    }

    fn restore(&mut self) -> Result<()> {
        while let Some((_, keycode, original)) = self.entries.last() {
            self.conn
                .change_keyboard_mapping(1, *keycode, original.len() as u8, original)?
                .check()
                .context("无法恢复 Unicode 输入键码")?;
            self.entries.pop();
        }
        self.conn.flush()?;
        Ok(())
    }
}

impl Drop for TemporaryKeymap<'_> {
    fn drop(&mut self) {
        for (_, keycode, _) in &self.entries {
            let _ = self
                .conn
                .xtest_fake_input(KEY_RELEASE, *keycode, 0, self.root, 0, 0, 0);
        }
        if let Err(error) = self.restore() {
            warn!("恢复临时键盘映射失败: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{CreateWindowAux, InputFocus, WindowClass};
    use x11rb::protocol::Event;

    #[tokio::test]
    #[ignore = "requires a dedicated X11 display in MIMICWX_TEST_DISPLAY"]
    async fn consecutive_messages_exceeding_spare_keycodes_are_typed_and_restore_mapping() {
        let display = std::env::var("MIMICWX_TEST_DISPLAY").unwrap();
        assert_ne!(display, ":1", "Do not run against the live WeChat display");
        assert_eq!(std::env::var("DISPLAY").unwrap(), display);
        let mut engine = InputEngine::new(TypingConfig::default()).unwrap();
        let setup = engine.conn.setup();
        let minimum = setup.min_keycode;
        let count = setup.max_keycode - minimum + 1;
        let original = engine
            .conn
            .get_keyboard_mapping(minimum, count)
            .unwrap()
            .reply()
            .unwrap();
        let empty_keycodes = original
            .keysyms
            .chunks_exact(original.keysyms_per_keycode as usize)
            .filter(|symbols| symbols.iter().all(|symbol| *symbol == 0))
            .count();
        assert!(empty_keycodes > 0 && empty_keycodes < 40);

        let (observer, screen) = RustConnection::connect(Some(&display)).unwrap();
        let window = observer.generate_id().unwrap();
        observer
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                observer.setup().roots[screen].root,
                0,
                0,
                400,
                200,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE),
            )
            .unwrap()
            .check()
            .unwrap();
        observer.map_window(window).unwrap().check().unwrap();
        observer
            .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();

        let samples = vec![
            "第一条消息你好🌏".to_string(),
            (0x4e00..0x4e28).filter_map(char::from_u32).collect(),
            (0x4e28..0x4e50).filter_map(char::from_u32).collect(),
            "最后一条也逐字输入✨".to_string(),
        ];
        let message_count = samples.len();
        let received = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut messages = Vec::new();
            let mut current = String::new();
            while messages.len() < message_count {
                assert!(
                    std::time::Instant::now() < deadline,
                    "Keyboard event timeout"
                );
                let Some(event) = observer.poll_for_event().unwrap() else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                };
                let Event::KeyPress(event) = event else {
                    continue;
                };
                let symbol = observer
                    .get_keyboard_mapping(event.detail, 1)
                    .unwrap()
                    .reply()
                    .unwrap()
                    .keysyms[0];
                if symbol == keysym::XK_RETURN {
                    messages.push(std::mem::take(&mut current));
                } else {
                    let value = if symbol & 0xff00_0000 == 0x0100_0000 {
                        symbol & 0x00ff_ffff
                    } else {
                        symbol
                    };
                    assert!(value <= 0xff || symbol & 0xff00_0000 == 0x0100_0000);
                    current.push(char::from_u32(value).unwrap());
                }
            }
            messages
        });
        for text in &samples {
            engine.input_message_text(text).await.unwrap();
            engine.press_enter().await.unwrap();
            let restored = engine
                .conn
                .get_keyboard_mapping(minimum, count)
                .unwrap()
                .reply()
                .unwrap();
            assert_eq!(restored.keysyms_per_keycode, original.keysyms_per_keycode);
            assert_eq!(restored.keysyms, original.keysyms);
        }
        assert_eq!(received.await.unwrap(), samples);
    }
}
