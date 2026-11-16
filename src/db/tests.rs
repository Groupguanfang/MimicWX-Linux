use super::{
    account_wxid, build_single_table_meta, image_receipt_matches, message_is_self,
    message_sender, normalize_group_message, sent_message_matches, DbMessage, MsgContent,
};

fn image_message(chat: &str, create_time: i64, is_self: bool) -> DbMessage {
    DbMessage {
        local_id: 1,
        server_id: 1,
        create_time,
        content: "<msg><img/></msg>".into(),
        parsed: MsgContent::Image { path: None },
        msg_type: 3,
        talker: "wxid_sandbox_self".into(),
        talker_display_name: "机器人".into(),
        chat: chat.into(),
        chat_display_name: "测试会话".into(),
        is_self,
    }
}

#[test]
fn account_identity_excludes_wechat_directory_suffix() {
    assert_eq!(
        account_wxid("wxid_sandbox123_14fd"),
        "wxid_sandbox123"
    );
    assert_eq!(account_wxid("wxid_example_c024"), "wxid_example");
    assert_eq!(account_wxid("wxid_example_c12345"), "wxid_example");
    assert_eq!(account_wxid("wxid_example_AB12"), "wxid_example");
    assert_eq!(account_wxid("wxid_example"), "wxid_example");
    assert_eq!(account_wxid("wxid_example_bot"), "wxid_example_bot");
    assert_eq!(account_wxid("all_users"), "all_users");
}

#[test]
fn send_confirmation_requires_the_recipient_and_entire_message() {
    assert!(!sent_message_matches(
        "wxid_sandbox_friend", "测试", "sandbox@chatroom", "测试",
    ));
    assert!(!sent_message_matches(
        "sandbox@chatroom", "完整测试消息", "sandbox@chatroom", "测试",
    ));
    assert!(sent_message_matches(
        "sandbox@chatroom", "完整测试消息\n", "sandbox@chatroom", "完整测试消息",
    ));
}

#[test]
fn image_confirmation_requires_a_new_self_image_in_the_target_chat() {
    let started_at = 1_700_000_000;
    assert!(image_receipt_matches(
        "sandbox@chatroom",
        started_at,
        &image_message("sandbox@chatroom", started_at + 5, true),
    ));
    assert!(!image_receipt_matches(
        "sandbox@chatroom",
        started_at,
        &image_message("other@chatroom", started_at + 5, true),
    ));
    assert!(!image_receipt_matches(
        "sandbox@chatroom",
        started_at,
        &image_message("sandbox@chatroom", started_at - 3, true),
    ));
    assert!(!image_receipt_matches(
        "sandbox@chatroom",
        started_at,
        &image_message("sandbox@chatroom", started_at + 5, false),
    ));
}

#[test]
fn integer_sender_ids_resolve_before_classifying_outgoing_images() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE Name2Id (user_name TEXT NOT NULL);
         INSERT INTO Name2Id (rowid, user_name) VALUES (1, 'wxid_self'), (2, 'wxid_friend');
         CREATE TABLE Msg_sandbox (
             local_id INTEGER, server_id INTEGER, create_time INTEGER,
             message_content TEXT, local_type INTEGER, real_sender_id INTEGER, status INTEGER
         );
         INSERT INTO Msg_sandbox VALUES (1, 101, 1700000000, '<msg><img/></msg>', 3, 1, 3);
         INSERT INTO Msg_sandbox VALUES (2, 102, 1700000001, '收到的消息', 1, 2, 0);",
    ).unwrap();
    let meta = build_single_table_meta(&conn, "Msg_sandbox").unwrap();
    let mut statement = conn.prepare(&meta.select_sql).unwrap();
    let results: Vec<(String, bool)> = statement.query_map([0], |row| {
        let sender = message_sender(&conn, row, 5)?;
        let is_self = message_is_self("wxid_self", &sender, row.get(6)?, row.get(4)?);
        Ok((sender, is_self))
    }).unwrap().map(Result::unwrap).collect();
    assert_eq!(results, vec![("wxid_self".into(), true), ("wxid_friend".into(), false)]);
}

#[test]
fn textual_senders_and_legacy_statuses_remain_supported() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let sender = conn.query_row("SELECT 'wxid_friend'", [], |row| message_sender(&conn, row, 0)).unwrap();
    assert_eq!(sender, "wxid_friend");
    assert!(message_is_self("wxid_self", "wxid_self", 3, 3));
    assert!(!message_is_self("wxid_self", "wxid_friend", 0, 3));
    assert!(message_is_self("wxid_self", "", 0, 1));
    assert!(!message_is_self("wxid_self", "", 2, 1));
    assert!(!message_is_self("wxid_self", "wxid_self", 0, 10000));
    assert!(!message_is_self("wxid_self", "wxid_self", 0, 10002));
}

#[test]
fn resolved_group_senders_do_not_leak_into_text_or_xml() {
    for separator in [":\n", ":\r\n"] {
        for body in ["@机器人 测试一下，只回复收到", "<msg><img/></msg>"] {
            let raw = format!("wxid_friend{separator}{body}");
            assert_eq!(
                normalize_group_message("room@chatroom", "wxid_friend".into(), raw.clone()),
                ("wxid_friend".into(), body.into()),
            );
            assert_eq!(
                normalize_group_message("room@chatroom", String::new(), raw),
                ("wxid_friend".into(), body.into()),
            );
        }
    }
}

#[test]
fn sender_prefix_normalization_preserves_private_and_unrelated_text() {
    for (chat, sender, content) in [
        ("wxid_friend", "wxid_friend", "wxid_friend:\n私聊原文"),
        ("room@chatroom", "wxid_friend", "wxid_other:\n引用其他人"),
        ("room@chatroom", "wxid_friend", "天气:\n晴"),
        ("room@chatroom", "", "https://example.test:\n普通消息"),
        ("room@chatroom", "", "含 空格:\n普通消息"),
    ] {
        assert_eq!(
            normalize_group_message(chat, sender.into(), content.into()),
            (sender.into(), content.into()),
        );
    }
}
