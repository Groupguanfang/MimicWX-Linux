use super::{account_wxid, sent_message_matches};

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
