use super::account_wxid;

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
