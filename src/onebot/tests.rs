use super::*;

fn message(chat: &str, content: &str, kind: i64) -> DbMessage {
    DbMessage {
        local_id: 17,
        server_id: 9_007_199_254_740_993,
        create_time: 1_794_753_850,
        content: content.into(),
        parsed: crate::db::parse_msg_content(kind, content),
        msg_type: kind,
        talker: "wxid_friend".into(),
        talker_display_name: "发送者".into(),
        chat: chat.into(),
        chat_display_name: "测试会话".into(),
        is_self: false,
    }
}

#[tokio::test]
async fn private_event_preserves_large_ids_and_deduplicates() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), Some("secret".into()));
    let input = message("wxid_friend", "明天天气怎么样", 1);
    let event = runtime.event_from_message(&input).await.unwrap();
    assert_eq!(event["message_type"], "private");
    assert_eq!(event["message_id"], "9007199254740993");
    assert_eq!(event["target_id"], "wxid_bot");
    assert_eq!(event["message"][0]["data"]["text"], "明天天气怎么样");
    assert!(event.get("group_id").is_none());
    assert!(runtime.event_from_message(&input).await.is_none());
}

#[tokio::test]
async fn self_messages_are_cached_without_being_dispatched() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let mut input = message("room@chatroom", "原消息", 1);
    input.is_self = true;
    assert!(runtime.event_from_message(&input).await.is_none());
    assert_eq!(
        runtime.messages.lock().await.values["9007199254740993"]["sender"]["user_id"],
        "wxid_bot"
    );
}

#[tokio::test]
async fn quote_and_mention_survive_app_message_parsing() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let content = "<msg><appmsg><title><![CDATA[@机器人 这是什么]]></title><type>57</type>\
        <refermsg><type>1</type><svrid>764760267806272435</svrid>\
        <fromusr>wxid_bot</fromusr><chatusr>room@chatroom</chatusr>\
        <displayname>机器人</displayname><content><![CDATA[原消息 & <文本>]]></content>\
        <createtime>1794752300</createtime></refermsg></appmsg></msg>";
    let event = runtime
        .event_from_message(&message("room@chatroom", content, 49))
        .await
        .unwrap();
    assert_eq!(event["group_id"], "room@chatroom");
    assert_eq!(
        event["message"][0],
        segment("reply", json!({ "id": "764760267806272435" }))
    );
    assert_eq!(
        event["message"][1],
        segment("at", json!({ "qq": "wxid_bot" }))
    );
    assert_eq!(event["message"][2]["data"]["text"], " 这是什么");
    let cache = runtime.messages.lock().await;
    let quote = &cache.values["764760267806272435"];
    assert_eq!(quote["sender"]["user_id"], "wxid_bot");
    assert_eq!(quote["message"][0]["data"]["text"], "原消息 & <文本>");
}

#[test]
fn quoted_image_is_a_media_segment() {
    let quote = parse_quote("<msg><refermsg><svrid>12</svrid><type>3</type>\
        <fromusr>wxid_friend</fromusr><content><![CDATA[<msg><img cdnmidimgurl=\"https://example.test/a.jpg\"/></msg>]]></content>\
        </refermsg></msg>").unwrap();
    let result = quote_message_value(&quote, true, "room@chatroom");
    assert_eq!(result["message"][0]["type"], "image");
    assert_eq!(
        result["message"][0]["data"]["url"],
        "https://example.test/a.jpg"
    );
}

#[test]
fn cq_messages_preserve_content_and_order() {
    let outgoing = outgoing_message(&json!({
        "message": "[CQ:reply,id=42]你好 &amp; &#91;测试&#93;[CQ:at,qq=wxid_bot][CQ:image,file=https://example.test/a&#44;b.png]"
    })).unwrap();
    assert_eq!(outgoing.quote_id.as_deref(), Some("42"));
    assert_eq!(
        outgoing.parts,
        vec![
            OutgoingPart::Text("你好 & [测试]".into()),
            OutgoingPart::At("wxid_bot".into()),
            OutgoingPart::Image("https://example.test/a,b.png".into()),
        ]
    );
    let literal =
        outgoing_message(&json!({ "message": "[CQ:at,qq=123]", "auto_escape": true })).unwrap();
    assert_eq!(
        literal.parts,
        vec![OutgoingPart::Text("[CQ:at,qq=123]".into())]
    );
    assert!(outgoing_message(
        &json!({ "message": [{ "type": "record", "data": { "file": "a.wav" } }] })
    )
    .is_err());
    assert!(outgoing_message(&json!({ "message": [] })).is_err());
}

#[test]
fn response_preserves_arbitrary_echo_and_reports_unsupported_actions() {
    let echo = json!({ "id": [1, "x"] });
    assert_eq!(action_response(Some(&echo), Ok(json!(true)))["echo"], echo);
    assert!(action_response(None, Ok(Value::Null)).get("echo").is_none());
    assert_eq!(
        action_response(None, Err("不支持的动作: delete_msg".into()))["retcode"],
        1404
    );
}

#[test]
fn authorization_is_required_and_header_takes_precedence() {
    let runtime = OneBotRuntime::new("bot".into(), "bot".into(), Some("secret".into()));
    assert!(!runtime.authorized(&HeaderMap::new(), &HashMap::new()));
    let mut query = HashMap::from([("access_token".to_string(), "secret".to_string())]);
    assert!(runtime.authorized(&HeaderMap::new(), &query));
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer wrong".parse().unwrap());
    assert!(!runtime.authorized(&headers, &query));
    query.clear();
    headers.insert("authorization", "Bearer secret".parse().unwrap());
    assert!(runtime.authorized(&headers, &query));
    assert!(
        !OneBotRuntime::new("bot".into(), "bot".into(), None).authorized(&HeaderMap::new(), &query)
    );
}

#[test]
fn cache_is_bounded_and_local_ids_are_scoped_to_the_chat() {
    let mut cache = MessageCache::default();
    for index in 0..CACHE_LIMIT + 1 {
        cache.insert(index.to_string(), json!(index));
    }
    assert_eq!(cache.values.len(), CACHE_LIMIT);
    assert!(!cache.values.contains_key("0"));
    let mut first = message("a", "text", 1);
    let mut second = message("b", "text", 1);
    first.server_id = 0;
    second.server_id = 0;
    assert_ne!(message_id(&first), message_id(&second));
}

#[test]
fn emoji_url_uses_an_image_segment() {
    let input = message(
        "room@chatroom",
        "<msg><emoji cdnurl=\"https://example.test/a.gif\"/></msg>",
        47,
    );
    assert_eq!(
        message_segments(&input, "bot", "机器人")[0]["type"],
        "image"
    );
}

#[test]
fn quoted_messages_are_scoped_to_their_conversation() {
    let group = json!({ "chat_id": "room@chatroom", "group_id": "room@chatroom" });
    let private = json!({ "chat_id": "wxid_friend" });
    assert!(quote_belongs_to_target(&group, "room@chatroom"));
    assert!(!quote_belongs_to_target(&group, "other@chatroom"));
    assert!(quote_belongs_to_target(&private, "wxid_friend"));
    assert!(!quote_belongs_to_target(&private, "wxid_other"));
    assert!(quote_belongs_to_target(&json!({}), "any"));
}

#[test]
fn unconfirmed_text_is_not_reported_as_success() {
    let response = crate::api::SendResponse {
        sent: true,
        verified: false,
        message: "accepted by the input queue".into(),
    };
    assert!(super::ensure_text_sent(&response).is_err());
    assert!(super::ensure_image_sent(&response).is_err());
    assert!(super::ensure_image_sent(&crate::api::SendResponse {
        sent: true,
        verified: true,
        message: "confirmed in the target chat".into(),
    })
    .is_ok());
}
