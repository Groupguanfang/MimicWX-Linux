use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct TypingConfig {
    pub enabled: bool,
    pub max_chars: usize,
}

impl Default for TypingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_chars: 40,
        }
    }
}

impl TypingConfig {
    pub fn should_type(&self, text: &str) -> bool {
        let count = text.chars().count();
        self.enabled
            && count > 0
            && count <= self.max_chars.clamp(1, 200)
            && !text.chars().any(|character| {
                character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
            })
            && !text.contains('@')
            && !text.to_ascii_lowercase().contains("://")
            && !text.to_ascii_lowercase().contains("www.")
    }
}

pub fn unicode_keysym(character: char) -> u32 {
    match character as u32 {
        value @ (0x20..=0x7e | 0xa0..=0xff) => value,
        value => 0x0100_0000 | value,
    }
}

pub fn typing_prefix(text: &str, capacity: usize) -> &str {
    let mut characters = Vec::new();
    for (offset, character) in text.char_indices() {
        if !characters.contains(&character) {
            if characters.len() == capacity {
                return &text[..offset];
            }
            characters.push(character);
        }
    }
    text
}

pub fn character_delay_ms(index: usize, count: usize, character: char, jitter: u8) -> u64 {
    if index + 1 >= count {
        return 0;
    }
    let base = 130_u64.saturating_sub(count as u64 * 2).clamp(45, 120);
    let progress = 100 - 35 * index as u64 / count.max(1) as u64;
    let variation = 80 + u64::from(jitter) * 40 / 255;
    let pause = if "，。！？；、,.!?;:：".contains(character) {
        60 + u64::from(jitter) * 60 / 255
    } else {
        0
    };
    base * progress * variation / 10_000 + pause
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_messages_count_unicode_characters() {
        let config = TypingConfig::default();
        assert!(config.should_type(&"中".repeat(40)));
        assert!(!config.should_type(&"中".repeat(41)));
        assert!(config.should_type("你好，世界🌏！"));
        assert!(!config.should_type(""));
        assert!(!TypingConfig {
            enabled: false,
            ..config
        }
        .should_type("你好"));
    }

    #[test]
    fn multiline_controls_and_links_are_pasted() {
        for text in [
            "第一行\n第二行",
            "第一行\r第二行",
            "制表\t文字",
            "分隔\u{2028}文字",
            "@群友 看一下",
            "邮箱 name@example.com",
            "https://example.com",
            "HTTP://example.com",
            "www.example.com",
        ] {
            assert!(!TypingConfig::default().should_type(text), "{text:?}");
        }
    }

    #[test]
    fn keysym_preserves_latin1_and_encodes_unicode() {
        assert_eq!(unicode_keysym('A'), 0x41);
        assert_eq!(unicode_keysym('é'), 0xe9);
        assert_eq!(unicode_keysym('你'), 0x0100_4f60);
        assert_eq!(unicode_keysym('🌏'), 0x0101_f30f);
    }

    #[test]
    fn typing_prefix_reuses_keycodes_for_repeated_characters() {
        assert_eq!(typing_prefix("你你好吗", 2), "你你好");
        assert_eq!(typing_prefix("你好🌏🌏再见", 3), "你好🌏🌏");
        assert_eq!(typing_prefix("你好", 0), "");
        assert_eq!(typing_prefix("", 2), "");
    }

    #[test]
    fn consecutive_messages_with_more_characters_than_keycodes_remain_typable() {
        let config = TypingConfig::default();
        for capacity in [1, 2, 19] {
            for text in [
                "第一条消息你好呀🌏",
                "天地玄黄宇宙洪荒日月盈昃辰宿列张寒来暑往秋收冬藏闰余成岁律吕调阳",
                "收到啦，继续聊吧！✨",
            ] {
                assert!(config.should_type(text));
                let mut remaining = text;
                let mut reconstructed = String::new();
                while !remaining.is_empty() {
                    let prefix = typing_prefix(remaining, capacity);
                    assert!(!prefix.is_empty());
                    let unique: std::collections::HashSet<_> = prefix.chars().collect();
                    assert!(unique.len() <= capacity);
                    reconstructed.push_str(prefix);
                    remaining = &remaining[prefix.len()..];
                }
                assert_eq!(reconstructed, text);
            }
        }
    }

    #[test]
    fn typing_accelerates_with_length_and_progress() {
        assert!(character_delay_ms(0, 8, '字', 128) > character_delay_ms(0, 40, '字', 128));
        assert!(character_delay_ms(0, 40, '字', 128) > character_delay_ms(30, 40, '字', 128));
        assert_eq!(character_delay_ms(39, 40, '。', 255), 0);
    }

    #[test]
    fn jitter_and_punctuation_have_bounded_latency() {
        for count in 1..=40 {
            let mut maximum_total = count as u64 * 24;
            for index in 0..count {
                let shortest = character_delay_ms(index, count, '字', 0);
                let longest = character_delay_ms(index, count, '字', 255);
                assert!(shortest <= longest);
                assert!(longest <= 144);
                let punctuated = character_delay_ms(index, count, '，', 255);
                assert!(punctuated >= longest);
                assert!(punctuated - longest <= 120);
                maximum_total += longest;
            }
            assert!(
                maximum_total < 3_000,
                "count={count}, total={maximum_total}"
            );
        }
    }
}
