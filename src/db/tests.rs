use super::{account_wxid, image_receipt_matches, sent_message_matches, DbMessage, MsgContent};

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
