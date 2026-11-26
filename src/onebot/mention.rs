use super::OutgoingPart;
use crate::db::GroupMember;
use std::collections::HashMap;

pub(super) fn resolve_mentions(
    parts: Vec<OutgoingPart>,
    members: &[GroupMember],
    self_id: &str,
    infer_names: bool,
) -> Result<(Vec<OutgoingPart>, HashMap<String, String>), String> {
    let mut resolved_parts = Vec::new();
    let mut names = HashMap::new();
    for part in parts {
        if infer_names {
            if let OutgoingPart::Text(text) = part {
                split_text_mentions(text, members, self_id, &mut resolved_parts);
                continue;
            }
        }
        resolved_parts.push(part);
    }
    for part in &resolved_parts {
        let OutgoingPart::At(id) = part else {
            continue;
        };
        if id == "all" || id == self_id {
            return Err("微信原生 @ 需要指定当前群里的另一位成员".into());
        }
        let member = members
            .iter()
            .find(|member| member.id == *id)
            .ok_or("要 @ 的人不在当前微信群成员列表中")?;
        let name =
            unique_display_name(member, members).ok_or("要 @ 的群成员没有可唯一确认的显示名")?;
        names.insert(id.clone(), name.to_owned());
    }
    Ok((resolved_parts, names))
}

fn split_text_mentions(
    text: String,
    members: &[GroupMember],
    self_id: &str,
    output: &mut Vec<OutgoingPart>,
) {
    let mut cursor = 0;
    for (start, marker) in text.match_indices(['@', '＠']) {
        if start < cursor
            || text[..start].chars().next_back().is_some_and(|before| {
                before.is_ascii_alphanumeric()
                    || matches!(
                        before,
                        '/' | '\\' | '@' | '.' | '_' | '-' | '+' | '=' | '%' | '#'
                    )
            })
        {
            continue;
        }
        let name_start = start + marker.len();
        let Some((length, member)) = match_member(&text[name_start..], members) else {
            continue;
        };
        if member.id == self_id || unique_display_name(member, members).is_none() {
            continue;
        }
        if cursor < start {
            output.push(OutgoingPart::Text(text[cursor..start].to_owned()));
        }
        output.push(OutgoingPart::At(member.id.clone()));
        cursor = name_start + length;
    }
    if cursor == 0 {
        output.push(OutgoingPart::Text(text));
    } else if cursor < text.len() {
        output.push(OutgoingPart::Text(text[cursor..].to_owned()));
    }
}

fn unique_display_name<'a>(member: &'a GroupMember, members: &[GroupMember]) -> Option<&'a str> {
    member.display_name().filter(|name| {
        !name.chars().any(char::is_control)
            && !members.iter().any(|other| {
                other.id != member.id && other.names().any(|alias| alias.eq_ignore_ascii_case(name))
            })
    })
}

fn match_member<'a>(input: &str, members: &'a [GroupMember]) -> Option<(usize, &'a GroupMember)> {
    let mut best_length = 0;
    let mut best = None;
    for member in members {
        for name in member.names() {
            if name.len() < best_length
                || !input.strip_prefix(name).is_some_and(|tail| {
                    tail.chars().next().is_none_or(|after| {
                        if after == '.' {
                            tail[1..].chars().next().is_none_or(char::is_whitespace)
                        } else {
                            after.is_whitespace()
                                || "，。！？、；：,!?;:）)】]」”@＠".contains(after)
                        }
                    })
                })
            {
                continue;
            }
            if name.len() > best_length {
                best_length = name.len();
                best = Some(member);
            } else if best.is_some_and(|other| other.id != member.id) {
                best = None;
            }
        }
    }
    best.map(|member| (best_length, member))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, card: &str, nickname: &str) -> GroupMember {
        GroupMember {
            id: id.into(),
            card: card.into(),
            nickname: nickname.into(),
            remark: String::new(),
            alias: String::new(),
        }
    }

    #[test]
    fn exact_group_names_become_mentions_while_other_at_signs_remain_text() {
        let members = [
            member("short", "小周", ""),
            member("long", "小周 同学", "GroupMember"),
            member("company", "example", ""),
        ];
        let input = "你好@小周 同学，联系 me@example.com 或 https://example.test/@example。";
        let (parts, names) = resolve_mentions(
            vec![OutgoingPart::Text(input.into())],
            &members,
            "bot",
            true,
        )
        .unwrap();
        assert_eq!(
            parts,
            [
                OutgoingPart::Text("你好".into()),
                OutgoingPart::At("long".into()),
                OutgoingPart::Text(
                    "，联系 me@example.com 或 https://example.test/@example。".into()
                ),
            ]
        );
        assert_eq!(names["long"], "小周 同学");
    }

    #[test]
    fn ambiguous_names_and_explicitly_plain_text_do_not_notify_people() {
        let members = [member("first", "小周", ""), member("second", "", "小周")];
        for infer in [true, false] {
            let (parts, names) = resolve_mentions(
                vec![OutgoingPart::Text("@小周".into())],
                &members,
                "bot",
                infer,
            )
            .unwrap();
            assert_eq!(parts, [OutgoingPart::Text("@小周".into())]);
            assert!(names.is_empty());
        }
        for id in ["first", "unknown", "all"] {
            assert!(
                resolve_mentions(vec![OutgoingPart::At(id.into())], &members, "bot", true).is_err()
            );
        }
    }
}
