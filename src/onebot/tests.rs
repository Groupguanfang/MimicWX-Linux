use super::*;

#[tokio::test]
async fn history_cache_fallback_preserves_sequence_paging_and_rejects_database_cursors() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "Bot".into(), None);
    let chat = "room@chatroom";
    for sequence in 1..=3 {
        let mut item = message(chat, "fixture", 1);
        item.local_id = sequence;
        item.server_id = sequence + 10;
        runtime.event_from_message(&item).await.unwrap();
    }
    let page = runtime
        .message_history(None, true, &json!({"group_id": chat, "count": 2}))
        .await
        .unwrap();
    assert_eq!(page["history_source"], "cache");
    assert_eq!(page["has_more"], true);
    assert_eq!(page["messages"][0]["message_seq"], 2);
    assert_eq!(page["messages"][1]["message_seq"], 3);

    let older = runtime
        .message_history(None, true, &json!({"group_id": chat, "message_seq": 2}))
        .await
        .unwrap();
    assert_eq!(older["messages"].as_array().unwrap().len(), 1);
    assert_eq!(older["messages"][0]["message_seq"], 1);
    assert_eq!(older["has_more"], false);

    let params =
        json!({"group_id": chat, "cursor": {"time": 1, "message_seq": 2, "source": "fixture"}});
    assert!(runtime.message_history(None, true, &params).await.is_err());
}

#[tokio::test]
#[ignore = "requires an explicitly supplied remote account fixture"]
async fn local_voice_fixture_survives_onebot_and_media_download() {
    let database_dir = std::env::var("MIMICWX_VOICE_DATABASE_DIR").unwrap();
    let key_file = std::env::var("MIMICWX_VOICE_KEY_FILE").unwrap();
    let chat = std::env::var("MIMICWX_VOICE_CHAT").unwrap();
    let server_id: i64 = std::env::var("MIMICWX_VOICE_SERVER_ID")
        .unwrap()
        .parse()
        .unwrap();
    let output_file = std::env::var("MIMICWX_VOICE_OUTPUT").unwrap();
    let key = std::fs::read_to_string(key_file).unwrap();
    let database =
        Arc::new(crate::db::DbManager::new(key.trim().into(), database_dir.into()).unwrap());
    let runtime = OneBotRuntime::with_media_url(
        "wxid_fixture_bot".into(),
        "Fixture".into(),
        Some("test-token".into()),
        "https://example.test/api/onebot/ws".into(),
        Some(database),
    )
    .unwrap();
    let mut original = message(&chat, "<msg><voicemsg voicelength=\"1600\"/></msg>", 34);
    original.server_id = server_id;
    let event = runtime.event_from_message(&original).await.unwrap();
    assert_eq!(event["message"][0]["type"], "record");
    assert_eq!(
        event["message_type"],
        if chat.ends_with("@chatroom") {
            "group"
        } else {
            "private"
        }
    );
    let id = event["message"][0]["data"]["file"].as_str().unwrap();
    let url = event["message"][0]["data"]["url"].as_str().unwrap();
    assert_eq!(
        url,
        format!("https://example.test/api/onebot/ws?media={id}")
    );
    assert!(!url.contains(key.trim()));
    let response = runtime.media_response(id, false).await;
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "audio/silk");
    assert!(bytes.starts_with(b"\x02#!SILK_V3") || bytes.starts_with(b"#!SILK_V3"));
    std::fs::write(output_file, &bytes).unwrap();
    println!(
        "{}",
        json!({
            "incoming_record": true,
            "media_download": true,
            "content_type": content_type,
            "bytes": bytes.len(),
            "real_messages_sent": 0,
        })
    );
}

#[tokio::test]
#[ignore = "requires an explicitly supplied remote account fixture"]
async fn local_image_fixture_survives_onebot_and_quoted_message_media() {
    let database_dir = std::env::var("MIMICWX_IMAGE_DATABASE_DIR").unwrap();
    let key_file = std::env::var("MIMICWX_IMAGE_KEY_FILE").unwrap();
    let xml_file = std::env::var("MIMICWX_IMAGE_XML_FILE").unwrap();
    let group = std::env::var("MIMICWX_IMAGE_GROUP").unwrap();
    let server_id: i64 = std::env::var("MIMICWX_IMAGE_SERVER_ID")
        .unwrap()
        .parse()
        .unwrap();
    let output_file = std::env::var("MIMICWX_IMAGE_OUTPUT").unwrap();
    let key = std::fs::read_to_string(key_file).unwrap();
    let database =
        Arc::new(crate::db::DbManager::new(key.trim().into(), database_dir.into()).unwrap());
    let runtime = OneBotRuntime::with_media_url(
        "wxid_fixture_bot".into(),
        "Fixture".into(),
        Some("test-token".into()),
        "https://example.test/api/onebot/ws".into(),
        Some(database),
    )
    .unwrap();
    let xml = std::fs::read_to_string(xml_file).unwrap();
    let mut original = message(&group, &xml, 3);
    original.server_id = server_id;
    let event = runtime.event_from_message(&original).await.unwrap();
    assert_eq!(event["message"][0]["type"], "image");
    let id = event["message"][0]["data"]["file"].as_str().unwrap();
    let response = runtime.media_response(id, false).await;
    let status = response.status();
    let source = response
        .headers()
        .get("x-mimicwx-media-source")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(response.into_body(), 20 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
    );
    assert_eq!(source, "original");
    assert!(bytes.len() > 1024);
    std::fs::write(output_file, &bytes).unwrap();
    runtime
        .messages
        .lock()
        .await
        .values
        .remove(&server_id.to_string());
    let quote = format!(
        "<msg><appmsg><title>这张图片是什么</title><type>57</type><refermsg>\
         <type>3</type><svrid>{server_id}</svrid><fromusr>wxid_fixture</fromusr>\
         <content>[图片]</content></refermsg></appmsg></msg>"
    );
    let mut quoted = message(&group, &quote, 49);
    quoted.server_id = server_id - 1;
    let quoted_event = runtime.event_from_message(&quoted).await.unwrap();
    assert_eq!(quoted_event["message"][0]["type"], "reply");
    let quoted_id = {
        let cache = runtime.messages.lock().await;
        assert_eq!(
            cache.values[&server_id.to_string()]["message"][0]["type"],
            "image"
        );
        cache.values[&server_id.to_string()]["message"][0]["data"]["file"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let quoted_response = runtime.media_response(&quoted_id, false).await;
    assert_eq!(quoted_response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(quoted_response.into_body(), 20 * 1024 * 1024)
            .await
            .unwrap(),
        bytes,
    );
    println!(
        "{}",
        json!({
            "ordinary_image": true, "quoted_image": true,
            "content_type": content_type, "source": "original", "bytes": bytes.len(),
            "real_messages_sent": 0,
        })
    );
}

fn message(chat: &str, content: &str, kind: i64) -> DbMessage {
    DbMessage {
        local_id: 17,
        server_id: 9_007_199_254_740_993,
        create_time: 1_794_753_850,
        content: content.into(),
        source: String::new(),
        parsed: crate::db::parse_msg_content(kind, content),
        msg_type: kind,
        talker: "wxid_friend".into(),
        talker_display_name: "发送者".into(),
        talker_nick_name: "发送者".into(),
        talker_remark: String::new(),
        chat: chat.into(),
        chat_display_name: "测试会话".into(),
        chat_nick_name: "测试会话".into(),
        chat_remark: String::new(),
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
async fn message_event_separates_actual_nickname_from_remark() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), Some("secret".into()));
    let mut input = message("wxid_friend", "你好", 1);
    input.talker_nick_name = "实际昵称".into();
    input.talker_remark = "备注名".into();
    input.chat_display_name = "备注名".into();
    input.chat_nick_name = "实际昵称".into();
    input.chat_remark = "备注名".into();

    let event = runtime.event_from_message(&input).await.unwrap();
    assert_eq!(event["sender"]["nickname"], "实际昵称");
    assert_eq!(event["sender"]["remark"], "备注名");
    assert_eq!(event["sender"]["display_name"], "备注名");
    assert_eq!(event["sender"]["card"], "");
}

#[tokio::test]
async fn group_event_keeps_remark_out_of_nickname_and_group_card() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), Some("secret".into()));
    let mut input = message("123@chatroom", "你好", 1);
    input.talker_display_name = "备注名".into();
    input.talker_nick_name = "实际昵称".into();
    input.talker_remark = "备注名".into();

    let event = runtime.event_from_message(&input).await.unwrap();
    assert_eq!(event["sender"]["nickname"], "实际昵称");
    assert_eq!(event["sender"]["card"], "");
    assert_eq!(event["sender"]["remark"], "备注名");
    assert_eq!(event["sender"]["display_name"], "备注名");
}

#[tokio::test]
async fn provisional_text_is_dispatched_once_and_finished_by_database_without_duplicate() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let mut provisional = message("room@chatroom", "@机器人 测试", 1);
    provisional.local_id = 42;
    provisional.server_id = 0;
    provisional.talker = "wxid_friend".into();
    let event = runtime.event_from_provisional(&provisional).await.unwrap();
    assert_eq!(event["message_id"], "fast_room@chatroom_42");
    assert_eq!(event["message_seq"], 42);
    assert_eq!(event["message"][0]["type"], "at");
    assert_eq!(event["message"][0]["data"]["qq"], "wxid_bot");
    assert!(runtime.event_from_provisional(&provisional).await.is_none());

    let mut completed = provisional.clone();
    completed.server_id = 9007199254740993;
    assert!(runtime.event_from_message(&completed).await.is_none());
    let cache = runtime.messages.lock().await;
    assert_eq!(
        cache.values["fast_room@chatroom_42"]["message_id"],
        "9007199254740993",
    );
    assert_eq!(cache.values["9007199254740993"]["message_seq"], 42,);
    drop(cache);
    assert_eq!(
        runtime
            .messages
            .lock()
            .await
            .history("room@chatroom", None, 10)
            .len(),
        1,
    );
}

#[tokio::test]
async fn provisional_text_uses_a_known_server_id_without_a_placeholder_alias() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let mut provisional = message("wxid_friend", "私聊快通道", 1);
    provisional.local_id = 46;
    provisional.server_id = 9007199254741001;
    let event = runtime.event_from_provisional(&provisional).await.unwrap();
    assert_eq!(event["message_id"], "9007199254741001");
    assert!(runtime.event_from_provisional(&provisional).await.is_none());
    let mut completed = provisional.clone();
    completed.content = "私聊快通道完整正文".into();
    completed.parsed = crate::db::parse_msg_content(1, &completed.content);
    assert!(runtime.event_from_message(&completed).await.is_none());
    assert_eq!(
        runtime
            .messages
            .lock()
            .await
            .history("wxid_friend", None, 10)
            .len(),
        1,
    );
}

#[tokio::test]
async fn provisional_cache_keeps_the_real_body_after_database_completion() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let mut provisional = message("room@chatroom", "短消息", 1);
    provisional.local_id = 44;
    provisional.server_id = 0;
    let fast = runtime.event_from_provisional(&provisional).await.unwrap();
    assert_eq!(fast["message"][0]["data"]["text"], "短消息");
    let mut completed = provisional.clone();
    completed.server_id = 9007199254740995;
    completed.content = "正文到达以后的完整内容".into();
    completed.parsed = crate::db::parse_msg_content(1, &completed.content);
    assert!(runtime.event_from_message(&completed).await.is_none());
    let cache = runtime.messages.lock().await;
    assert_eq!(
        cache.values["fast_room@chatroom_44"]["message_id"],
        "9007199254740995",
    );
    assert_eq!(
        cache.values["9007199254740995"]["message"][0]["data"]["text"],
        "正文到达以后的完整内容",
    );
}

#[tokio::test]
async fn database_arrival_before_session_snapshot_prevents_a_provisional_duplicate() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let mut completed = message("room@chatroom", "原消息", 1);
    completed.local_id = 43;
    completed.server_id = 123456;
    assert!(runtime.event_from_message(&completed).await.is_some());
    let mut provisional = completed.clone();
    provisional.server_id = 0;
    assert!(runtime.event_from_provisional(&provisional).await.is_none());
}

#[tokio::test]
async fn repeated_session_snapshot_does_not_duplicate_a_fast_message() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let mut provisional = message("room@chatroom", "轮询重复", 1);
    provisional.local_id = 45;
    provisional.server_id = 0;
    assert!(runtime.event_from_provisional(&provisional).await.is_some());
    assert!(runtime.event_from_provisional(&provisional).await.is_none());
    let mut completed = provisional.clone();
    completed.server_id = 9007199254740997;
    assert!(runtime.event_from_message(&completed).await.is_none());
    assert_eq!(
        runtime
            .messages
            .lock()
            .await
            .history("room@chatroom", None, 10)
            .len(),
        1,
    );
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
async fn group_messages_without_a_sender_are_not_dispatched() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let mut input = message("room@chatroom", "<msg><img/></msg>", 3);
    input.talker.clear();
    assert!(runtime.event_from_message(&input).await.is_none());
    assert!(runtime.messages.lock().await.values.is_empty());
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
fn native_mention_receipt_requires_the_right_wechat_user_id() {
    let mut input = message("room@chatroom", "你好\u{2005}@小周\u{2005} 请看", 1);
    let expected = vec!["wxid_zhou".to_string()];
    assert!(!native_mentions_match(&input, &expected));
    input.source = "<msgsource><atuserlist>wxid_other</atuserlist></msgsource>".into();
    assert!(!native_mentions_match(&input, &expected));
    input.source = "<msgsource><atuserlist><![CDATA[wxid_zhou]]></atuserlist></msgsource>".into();
    assert!(native_mentions_match(&input, &expected));
    assert_eq!(
        message_segments(&input, "wxid_bot", "机器人"),
        [
            segment("text", json!({ "text": "你好" })),
            segment("at", json!({ "qq": "wxid_zhou", "name": "小周" })),
            segment("text", json!({ "text": " 请看" })),
        ]
    );
}

#[tokio::test]
async fn official_account_article_cards_keep_share_and_readable_segments() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let content = "<msg><appmsg><title>腾讯内部讲话曝光</title>\
        <des>一篇公众号文章摘要</des><type>5</type>\
        <url>https://mp.weixin.qq.com/s?__biz=abc&amp;mid=123#rd</url>\
        <thumburl>https://mmbiz.qpic.cn/example.jpg</thumburl></appmsg></msg>";
    let event = runtime
        .event_from_message(&message("room@chatroom", content, 49))
        .await
        .unwrap();
    assert_eq!(
        event["message"][0],
        segment(
            "share",
            json!({
                "url": "https://mp.weixin.qq.com/s?__biz=abc&mid=123#rd",
                "title": "腾讯内部讲话曝光",
                "content": "一篇公众号文章摘要",
            })
        )
    );
    assert_eq!(
        event["message"][1]["data"]["text"],
        "腾讯内部讲话曝光\n一篇公众号文章摘要\nhttps://mp.weixin.qq.com/s?__biz=abc&mid=123#rd",
    );
}

#[tokio::test]
async fn merged_forward_keeps_the_actual_nodes_in_the_event_and_quote() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let content = r#"<msg><appmsg><type>19</type><title>群聊的聊天记录</title><des>只有预览</des>
        <recorditem><![CDATA[<recordinfo><datalist count="3">
        <dataitem datatype="1"><sourcename>小周</sourcename><sourcetime>20:01</sourcetime>
        <srcMsgCreateTime>1795700000</srcMsgCreateTime><datadesc>第三份 &amp; 第四份</datadesc></dataitem>
        <dataitem datatype="2"><sourcename>小林</sourcename><cdndataurl>仅供微信读取</cdndataurl></dataitem>
        <dataitem datatype="1"><sourcename>小林</sourcename><datadesc>预览没有的暗号</datadesc></dataitem>
        </datalist></recordinfo>]]></recorditem></appmsg></msg>"#;
    let mut original = message("room@chatroom", content, 49);
    original.server_id = 42;
    let event = runtime.event_from_message(&original).await.unwrap();
    assert_eq!(
        event["message"][0],
        segment("forward", json!({ "id": "42" }))
    );
    let context = event["message"][1]["data"]["text"].as_str().unwrap();
    assert!(context.contains("20:01 小周：第三份 & 第四份"));
    assert!(context.contains("小林：[图片]"));
    assert!(context.contains("小林：预览没有的暗号"));
    assert!(!context.contains("只有预览"));
    assert!(!context.contains("仅供微信读取"));
    let records = runtime.forward_records.lock().await;
    assert_eq!(records.values["42"]["message"].as_array().unwrap().len(), 3);
    assert_eq!(
        records.values["42"]["message"][2]["data"]["nickname"],
        "小林"
    );
    drop(records);

    let escaped = content
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let quote = format!(
        "<msg><appmsg><type>57</type><title>总结一下</title><refermsg>\
        <type>49</type><svrid>43</svrid><fromusr>wxid_fixture</fromusr>\
        <content>{escaped}</content></refermsg></appmsg></msg>"
    );
    let quoted = runtime
        .event_from_message(&message("room@chatroom", &quote, 49))
        .await
        .unwrap();
    assert_eq!(
        quoted["message"][0],
        segment("reply", json!({ "id": "43" }))
    );
    let cached = runtime.cached_message("43").await.unwrap();
    assert_eq!(cached["message"][1]["data"]["text"], context);
}

#[tokio::test]
async fn link_cards_with_only_a_title_and_url_are_share_segments() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let content = "<msg><appmsg><title>链接标题</title><type>4</type>\
        <url><![CDATA[https://example.test/article?x=1&y=2]]></url></appmsg></msg>";
    let event = runtime
        .event_from_message(&message("room@chatroom", content, 49))
        .await
        .unwrap();
    assert_eq!(
        event["message"][0],
        segment(
            "share",
            json!({
                "url": "https://example.test/article?x=1&y=2",
                "title": "链接标题",
                "content": "",
            })
        )
    );
    assert_eq!(
        event["message"][1]["data"]["text"],
        "链接标题\nhttps://example.test/article?x=1&y=2",
    );
}

#[tokio::test]
async fn share_cards_without_a_url_fall_back_to_readable_text() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let content =
        "<msg><appmsg><title>没有链接的卡片</title><des>描述</des><type>5</type></appmsg></msg>";
    let event = runtime
        .event_from_message(&message("room@chatroom", content, 49))
        .await
        .unwrap();
    assert_eq!(event["message"].as_array().unwrap().len(), 1);
    assert_eq!(event["message"][0]["type"], "text");
    assert_eq!(event["message"][0]["data"]["text"], "没有链接的卡片\n描述");
}

#[test]
fn quoted_image_is_a_media_segment() {
    let quote = parse_quote("<msg><refermsg><svrid>12</svrid><type>3</type>\
        <fromusr>wxid_friend</fromusr><content><![CDATA[<msg><img cdnmidimgurl=\"https://example.test/a.jpg?x=1&amp;filekey=abc\"/></msg>]]></content>\
        </refermsg></msg>").unwrap();
    let result = quote_message_value(&quote, true, "room@chatroom");
    assert_eq!(result["message"][0]["type"], "image");
    assert_eq!(
        result["message"][0]["data"]["url"],
        "https://example.test/a.jpg?x=1&filekey=abc"
    );
}

#[tokio::test]
async fn outgoing_quote_preview_omits_nested_reply_codes_and_resolves_mentions() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    let content = "<msg><appmsg><title>@机器人 这个写的啥</title><type>57</type>\
        <refermsg><type>3</type><svrid>8366233727440128295</svrid>\
        <fromusr>wxid_friend</fromusr><content>[图片]</content></refermsg>\
        </appmsg></msg>";
    let event = runtime
        .event_from_message(&message("room@chatroom", content, 49))
        .await
        .unwrap();
    assert!(event["raw_message"]
        .as_str()
        .unwrap()
        .contains("[CQ:reply,"));
    assert_eq!(
        quote_preview(&event, &[], "wxid_bot", "机器人"),
        "@机器人 这个写的啥"
    );
}

#[test]
fn outgoing_quote_preview_hides_image_addresses_and_preserves_caption() {
    let quoted = json!({
        "message": [
            segment("image", json!({ "file": "private-media-id", "url": "https://example.test/?media=private-media-id" })),
            segment("text", json!({ "text": " 图片说明" })),
        ],
        "raw_message": "[CQ:image,file=private-media-id,url=https://example.test/?media=private-media-id] 图片说明",
    });
    assert_eq!(
        quote_preview(&quoted, &[], "bot", "机器人"),
        "[图片] 图片说明"
    );
}

#[test]
fn outgoing_quote_preview_decodes_cq_only_when_structured_segments_are_missing() {
    let raw = "[CQ:reply,id=42][CQ:at,qq=wxid_bot] 你好 &amp; &#91;原文&#93;\
        [CQ:image,file=https://example.test/a&#44;b.png]";
    for quoted in [json!({ "raw_message": raw }), json!({ "message": raw })] {
        assert_eq!(
            quote_preview(&quoted, &[], "wxid_bot", "机器人"),
            "@机器人 你好 & [原文][图片]"
        );
    }
    let literal = "[CQ:at,qq=123] &amp; &#91;原文&#93;";
    assert_eq!(
        quote_preview(
            &json!({ "message": [segment("text", json!({ "text": literal }))] }),
            &[],
            "bot",
            "机器人",
        ),
        literal,
    );
}

#[test]
fn outgoing_quote_preview_resolves_contacts_and_labels_other_media() {
    let contacts = vec![ContactInfo {
        username: "wxid_friend".into(),
        nick_name: "群友".into(),
        remark: String::new(),
        alias: String::new(),
        display_name: "群友".into(),
    }];
    let quoted = json!({ "message": [
        segment("at", json!({ "qq": "wxid_friend" })),
        segment("text", json!({ "text": " " })),
        segment("at", json!({ "qq": "all" })),
        segment("record", json!({ "file": "private-voice-url" })),
        segment("video", json!({ "file": "private-video-url" })),
        segment("face", json!({ "id": "123" })),
        segment("file", json!({ "file": "private-file-url" })),
        segment("json", json!({ "data": "private-card-content" })),
    ] });
    assert_eq!(
        quote_preview(&quoted, &contacts, "bot", "机器人"),
        "@群友 @全体成员[语音][视频][表情][文件][消息]"
    );
}

#[test]
fn outgoing_quote_preview_handles_empty_and_malformed_cached_content() {
    for quoted in [
        json!({}),
        json!({ "message": [] }),
        json!({ "message": [segment("reply", json!({ "id": "42" }))] }),
        json!({ "raw_message": "[CQ:image,file=unclosed" }),
    ] {
        assert_eq!(quote_preview(&quoted, &[], "bot", "机器人"), "[消息]");
    }
}

#[test]
fn outgoing_quote_preview_truncates_unicode_after_formatting() {
    let quoted = json!({
        "message": [segment("text", json!({ "text": format!("{}end", "中文🙂".repeat(110)) }))],
    });
    let preview = quote_preview(&quoted, &[], "bot", "机器人");
    assert_eq!(preview, format!("{}…", "中文🙂".repeat(100)));
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
    let voice = outgoing_message(
        &json!({ "message": [{ "type": "record", "data": { "file": "a.wav" } }] }),
    )
    .unwrap();
    assert_eq!(voice.parts, vec![OutgoingPart::Record("a.wav".into())]);
    assert!(outgoing_message(&json!({ "message": [] })).is_err());
}

#[test]
fn outgoing_stickers_preserve_the_native_marker_for_json_and_cq() {
    for data in [
        json!({ "file": "a.gif", "sticker": true }),
        json!({ "file": "a.gif", "sticker": "true" }),
        json!({ "file": "a.gif", "sub_type": "sticker" }),
    ] {
        let outgoing = outgoing_message(&json!({
            "message": [{ "type": "image", "data": data }],
        }))
        .unwrap();
        assert_eq!(outgoing.parts, vec![OutgoingPart::Sticker("a.gif".into())]);
    }
    let outgoing = outgoing_message(&json!({
        "message": "[CQ:image,file=a.gif,sticker=true][CQ:image,file=b.gif,sticker=false]",
    }))
    .unwrap();
    assert_eq!(
        outgoing.parts,
        vec![
            OutgoingPart::Sticker("a.gif".into()),
            OutgoingPart::Image("b.gif".into()),
        ],
    );
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

#[tokio::test]
async fn cached_history_keeps_conversations_cursors_and_self_messages() {
    let runtime = OneBotRuntime::new("wxid_bot".into(), "机器人".into(), None);
    for (sequence, chat, is_self) in [
        (1, "room@chatroom", false),
        (2, "other@chatroom", false),
        (3, "room@chatroom", true),
        (4, "wxid_friend", false),
        (5, "room@chatroom", false),
    ] {
        let mut input = message(chat, "缓存消息", 1);
        input.local_id = sequence;
        input.server_id += sequence;
        input.is_self = is_self;
        runtime.event_from_message(&input).await;
    }
    let mut cache = runtime.messages.lock().await;
    cache.insert(
        "quoted-only".into(),
        json!({
            "chat_id": "room@chatroom", "message_id": "quoted-only",
        }),
    );
    let history = cache.history("room@chatroom", Some(5), 100);
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["message_seq"], 1);
    assert_eq!(history[1]["message_seq"], 3);
    assert_eq!(history[1]["sender"]["user_id"], "wxid_bot");
    assert_eq!(cache.history("room@chatroom", None, 1)[0]["message_seq"], 5);
    assert_eq!(cache.history("wxid_friend", None, 100).len(), 1);
    assert!(cache.history("unknown@chatroom", None, 100).is_empty());
    assert!(cache.history("room@chatroom", Some(1), 100).is_empty());
}

#[test]
fn emoji_url_uses_an_image_segment() {
    let input = message(
        "room@chatroom",
        "<msg><emoji cdnurl=\"https://example.test/a.gif?size=1&amp;filekey=abc\"/></msg>",
        47,
    );
    assert_eq!(
        message_segments(&input, "bot", "机器人")[0]["type"],
        "image"
    );
    assert_eq!(
        message_segments(&input, "bot", "机器人")[0]["data"]["url"],
        "https://example.test/a.gif?size=1&filekey=abc"
    );
    assert_eq!(
        message_segments(&input, "bot", "机器人")[0]["data"]["sticker"],
        true
    );
    assert_eq!(
        message_segments(&input, "bot", "机器人")[0]["data"]["sub_type"],
        "sticker"
    );
}

#[tokio::test]
async fn encrypted_stickers_and_their_quotes_use_media_urls_without_exposing_keys() {
    let runtime = OneBotRuntime::with_media_url(
        "bot".into(),
        "机器人".into(),
        Some("secret".into()),
        "https://example.test/api/onebot/ws".into(),
        None,
    )
    .unwrap();
    let xml = "<msg><emoji encrypturl=\"https://cdn.example.test/encrypted\" aeskey=\"000102030405060708090a0b0c0d0e0f\"/></msg>";
    let input = message("room@chatroom", xml, 47);
    let event = runtime.event_from_message(&input).await.unwrap();
    let data = &event["message"][0]["data"];
    assert_eq!(event["message"][0]["type"], "image");
    assert!(data["url"]
        .as_str()
        .unwrap()
        .starts_with("https://example.test/api/onebot/ws?media="));
    assert_eq!(data["file"].as_str().unwrap().len(), 64);
    assert_eq!(data["sticker"], true);
    assert_eq!(data["sub_type"], "sticker");
    assert!(!event.to_string().contains("00010203"));
    assert!(!event.to_string().contains("cdn.example.test"));

    let xml = format!("<msg><appmsg><title>@机器人 这是什么</title><type>57</type><refermsg><type>47</type><svrid>12</svrid><fromusr>wxid_friend</fromusr><content><![CDATA[{xml}]]></content></refermsg></appmsg></msg>");
    let mut input = message("room@chatroom", &xml, 49);
    input.server_id += 1;
    let event = runtime.event_from_message(&input).await.unwrap();
    assert_eq!(event["message"][0]["type"], "reply");
    assert_eq!(event["message"][1]["type"], "at");
    let cache = runtime.messages.lock().await;
    let quote = &cache.values["12"];
    assert_eq!(quote["message"][0]["type"], "image");
    assert_eq!(quote["message"][0]["data"]["sticker"], true);
    assert_eq!(quote["message"][0]["data"]["sub_type"], "sticker");
    assert!(quote["message"][0]["data"]["url"]
        .as_str()
        .unwrap()
        .contains("?media="));
    assert!(!quote.to_string().contains("00010203"));
    assert_eq!(
        runtime
            .media_response(&"0".repeat(64), false)
            .await
            .status(),
        StatusCode::NOT_FOUND
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
    assert!(!quote_belongs_to_target(&json!({}), "any"));
}

#[test]
fn native_reply_receipts_require_the_original_id_and_entire_body() {
    let content = "<msg><appmsg><title><![CDATA[完整回复 <测试>\n第二行]]></title>\
        <type>57</type><refermsg><svrid>123456</svrid><type>1</type>\
        <content>原消息</content></refermsg></appmsg></msg>";
    assert!(native_reply_matches(
        content,
        "123456",
        "完整回复 <测试>\n第二行"
    ));
    assert!(!native_reply_matches(
        content,
        "999",
        "完整回复 <测试>\n第二行"
    ));
    assert!(!native_reply_matches(content, "123456", "完整回复"));
    assert!(!native_reply_matches(
        "完整回复 <测试>\n第二行",
        "123456",
        "完整回复 <测试>\n第二行"
    ));
    assert!(!native_reply_matches(
        &content.replace("<type>57</type>", "<type>5</type>"),
        "123456",
        "完整回复 <测试>\n第二行"
    ));
}

#[test]
fn native_quote_targets_keep_the_source_out_of_the_reply_body() {
    let source = "@机器人\u{2005}".to_string() + &"完整内容".repeat(100);
    let mut history = quote_history_fixture(&source, "张三", "张三");
    let target = native_quote_target(history.clone()).unwrap();
    assert_eq!(target.server_id, "123456");
    assert_eq!(target.text, source);
    assert_eq!(target.sender_name, "张三");
    assert!(!target.is_self);
    history.messages[0].server_id = 0;
    assert!(native_quote_target(history).is_err());
}

#[test]
fn native_quote_targets_use_wechat_display_name_without_changing_public_nickname() {
    let mut history = quote_history_fixture("原消息", "备注名", "实际昵称");
    let target = native_quote_target(history.clone()).unwrap();
    assert_eq!(target.sender_name, "备注名");
    assert_eq!(history.messages[0].talker_nick_name, "实际昵称");
    history.messages[0].talker_display_name = " ".into();
    let target = native_quote_target(history).unwrap();
    assert_eq!(target.sender_name, "实际昵称");
}

fn quote_history_fixture(
    text: &str,
    display_name: &str,
    nickname: &str,
) -> crate::db::QuoteHistory {
    crate::db::QuoteHistory {
        messages: vec![DbMessage {
            local_id: 10,
            server_id: 123456,
            create_time: 145,
            content: text.into(),
            source: String::new(),
            parsed: MsgContent::Text { text: text.into() },
            msg_type: 1,
            talker: "wxid_friend".into(),
            talker_display_name: display_name.into(),
            talker_nick_name: nickname.into(),
            talker_remark: String::new(),
            chat: "room@chatroom".into(),
            chat_display_name: "测试群".into(),
            chat_nick_name: "测试群".into(),
            chat_remark: String::new(),
            is_self: false,
        }],
        target_index: 0,
        complete_minutes: 0..300,
    }
}

#[test]
fn unconfirmed_text_is_not_reported_as_success() {
    let response = crate::api::SendResponse {
        sent: true,
        verified: false,
        confirmation_pending: false,
        message: "accepted by the input queue".into(),
    };
    assert!(super::ensure_text_sent(&response).is_err());
    assert!(super::ensure_image_sent(&response).is_err());
    assert!(super::ensure_image_sent(&crate::api::SendResponse {
        sent: true,
        verified: true,
        confirmation_pending: false,
        message: "confirmed in the target chat".into(),
    })
    .is_ok());
}

#[test]
fn submitted_messages_can_return_while_confirmation_is_pending() {
    let mut response = crate::api::SendResponse {
        sent: true,
        verified: false,
        confirmation_pending: true,
        message: "submitted to the target chat".into(),
    };
    assert!(ensure_text_sent(&response).is_ok());
    assert!(ensure_image_sent(&response).is_ok());
    response.sent = false;
    assert!(ensure_text_sent(&response).is_err());
    assert!(ensure_image_sent(&response).is_err());
}

#[tokio::test]
async fn slow_sends_do_not_block_queries_and_queued_sends_remain_ordered() {
    let mut queue = ActionQueue::default();
    let (release, waiting) = tokio::sync::oneshot::channel();
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    queue.push(true, async move {
        waiting.await.unwrap();
        json!("first send")
    });
    let second_started = started.clone();
    queue.push(true, async move {
        second_started.store(true, Ordering::SeqCst);
        json!("second send")
    });
    queue.push(false, async { json!("get_status") });
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(500), queue.next())
            .await
            .unwrap()
            .unwrap(),
        json!("get_status")
    );
    assert!(!started.load(Ordering::SeqCst));
    release.send(()).unwrap();
    assert_eq!(queue.next().await.unwrap(), json!("first send"));
    assert_eq!(queue.next().await.unwrap(), json!("second send"));
    assert_eq!(queue.len(), 0);
}
