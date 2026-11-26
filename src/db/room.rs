use super::{wcdb_get_text, DbManager};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug)]
pub(crate) struct GroupMember {
    pub id: String,
    pub card: String,
    pub remark: String,
    pub nickname: String,
    pub alias: String,
}

impl GroupMember {
    pub fn display_name(&self) -> Option<&str> {
        [&self.card, &self.remark, &self.nickname]
            .into_iter()
            .map(|value| value.trim())
            .find(|value| !value.is_empty())
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        [
            &self.card,
            &self.remark,
            &self.nickname,
            &self.alias,
            &self.id,
        ]
        .into_iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    }
}

impl DbManager {
    pub(crate) async fn group_members(&self, group: &str) -> Result<Vec<GroupMember>> {
        let connection = Arc::clone(&self.contact_conn);
        let group = group.to_owned();
        tokio::task::spawn_blocking(move || {
            let guard = connection
                .lock()
                .map_err(|error| anyhow::anyhow!("联系人快照不可用: {error}"))?;
            read_group_members(guard.as_ref().context("联系人快照尚未加载")?, &group)
        })
        .await?
    }
}

fn read_group_members(connection: &Connection, group: &str) -> Result<Vec<GroupMember>> {
    let buffer = connection
        .query_row(
            "SELECT ext_buffer FROM chat_room WHERE username = ?1",
            [group],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()?
        .flatten()
        .unwrap_or_default();
    let mut cards = room_cards(&buffer);
    let mut statement = connection.prepare(
        "SELECT DISTINCT c.username, c.remark, c.nick_name, c.alias
         FROM chat_room r
         JOIN chatroom_member m ON m.room_id = r.id
         JOIN contact c ON c.id = m.member_id
         WHERE r.username = ?1",
    )?;
    let mut members = statement
        .query_map([group], |row| {
            let id = wcdb_get_text(row, 0);
            Ok(GroupMember {
                card: cards.remove(&id).unwrap_or_default(),
                id,
                remark: wcdb_get_text(row, 1),
                nickname: wcdb_get_text(row, 2),
                alias: wcdb_get_text(row, 3),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    members.extend(cards.into_iter().map(|(id, card)| GroupMember {
        id,
        card,
        remark: String::new(),
        nickname: String::new(),
        alias: String::new(),
    }));
    Ok(members)
}

fn room_cards(buffer: &[u8]) -> HashMap<String, String> {
    field_bytes(buffer, 1)
        .filter_map(|entry| {
            let id = std::str::from_utf8(field_bytes(entry, 1).next()?).ok()?;
            if id.is_empty() || id.contains('@') || id.chars().any(char::is_control) {
                return None;
            }
            let card = field_bytes(entry, 2)
                .next()
                .and_then(|value| std::str::from_utf8(value).ok())
                .unwrap_or_default();
            Some((id.to_owned(), card.to_owned()))
        })
        .collect()
}

fn field_bytes(mut input: &[u8], field: u64) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || loop {
        let tag = read_varint(&mut input)?;
        let length = match tag & 7 {
            0 => {
                read_varint(&mut input)?;
                continue;
            }
            1 => 8,
            2 => usize::try_from(read_varint(&mut input)?).ok()?,
            5 => 4,
            _ => return None,
        };
        let (value, rest) = input.split_at_checked(length)?;
        input = rest;
        if tag >> 3 == field && tag & 7 == 2 {
            return Some(value);
        }
    })
}

fn read_varint(input: &mut &[u8]) -> Option<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = input.split_first()?;
        *input = rest;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_names_use_member_cards_and_never_include_other_rooms() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE chat_room (id INTEGER, username TEXT, ext_buffer BLOB);
             CREATE TABLE chatroom_member (room_id INTEGER, member_id INTEGER);
             CREATE TABLE contact (id INTEGER, username TEXT, remark TEXT, nick_name TEXT, alias TEXT);
             INSERT INTO chat_room VALUES (1, 'group@chatroom', X'0A0E0A066D656D626572120463617264'), (2, 'other@chatroom', NULL);
             INSERT INTO contact VALUES (1, 'member', 'remark', 'nickname', 'alias'), (2, 'outside', '', 'outside', '');
             INSERT INTO chatroom_member VALUES (1, 1), (2, 2);",
        )
        .unwrap();
        let members = read_group_members(&db, "group@chatroom").unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id, "member");
        assert_eq!(members[0].display_name(), Some("card"));
        assert!(members[0].names().any(|name| name == "nickname"));
    }
}
