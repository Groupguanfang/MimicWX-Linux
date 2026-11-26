use super::emoji_media_from_connection;
use super::{
    account_wxid, build_single_table_meta, image_receipt_matches, message_content_pending,
    message_is_self, message_sender, normalize_group_message, parse_msg_content,
    sent_message_matches, session_updates_from_rows, sticker_receipt_matches, DbMessage,
    MsgContent, SessionCursor, SessionMessage,
};
use std::collections::HashMap;

pub(super) fn open_fixture_database(path: &std::path::Path, key: &[u8]) -> rusqlite::Connection {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )
        .unwrap();
    assert_eq!(
        unsafe { super::sqlite3_key(connection.handle().cast(), key.as_ptr(), key.len() as i32) },
        0
    );
    connection
        .execute_batch("PRAGMA cipher_compatibility=4;")
        .unwrap();
    connection
}

#[test]
fn forward_cache_id_requires_the_record_field_and_a_safe_unique_name() {
    use super::history::record_cache_id;

    let valid = b"\x08\x03\x10\x02\x4a\x12\x0a\x10abcd0123456789ef\x58\x00";
    assert_eq!(record_cache_id(valid), Some("abcd0123456789ef"));
    assert!(record_cache_id(b"abcd0123456789ef").is_none());
    assert!(record_cache_id(b"\x4a\x12\x0a\x10../../0123456789").is_none());
    assert!(record_cache_id(&[valid.as_slice(), valid].concat()).is_none());
}

#[tokio::test]
async fn history_pages_across_shards_with_overlapping_local_ids() {
    use super::{DbManager, HistoryBefore};
    use std::sync::Arc;

    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let directory =
        std::env::temp_dir().join(format!("mimicwx-history-{}", u64::from_le_bytes(random)));
    let database_dir = directory.join("message");
    std::fs::create_dir_all(&database_dir).unwrap();
    let chat = "room@chatroom";
    let table = format!("Msg_{:x}", md5::compute(chat));
    let schema = format!(
        "CREATE TABLE Name2Id (user_name TEXT UNIQUE);
         INSERT INTO Name2Id VALUES ('wxid_sender'), ('wxid_friend');
         CREATE TABLE [{table}] (
             local_id INTEGER PRIMARY KEY, server_id INTEGER, create_time INTEGER,
             message_content TEXT, local_type INTEGER, real_sender_id INTEGER
         );
         CREATE TABLE ChatMsg_2 (
             localId INTEGER PRIMARY KEY, msgSvrId INTEGER, createTime INTEGER,
             msgContent TEXT, msgType INTEGER, talkerId TEXT
         );"
    );
    let key = [0u8; 32];
    let first = open_fixture_database(&database_dir.join("message_0.db"), &key);
    first
        .execute_batch(&format!(
            "{schema}
             INSERT INTO [{table}] VALUES
                 (1, 101, NULL, 'oldest', 1, 1),
                 (2, 102, 20, 'wxid_sender:\nsame time', 1, 1),
                 (3, 103, 30, 'latest', 1, 1);
             INSERT INTO ChatMsg_2 VALUES (1, 301, 5, 'private', 1, 'wxid_friend');"
        ))
        .unwrap();
    let database = Arc::new(DbManager::new("00".repeat(32), directory.clone()).unwrap());
    let mut received = database.subscribe_received();
    let second = open_fixture_database(&database_dir.join("message_1.db"), &key);
    second
        .execute_batch(&format!(
            "{schema}
             INSERT INTO [{table}] VALUES
                 (1, 201, 10, 'older', 1, 1),
                 (2, 202, 20, 'overlapping sequence', 1, 1);"
        ))
        .unwrap();

    let mut before = None;
    let mut ids = Vec::new();
    for expected in [[202, 103].as_slice(), &[201, 102], &[101]] {
        let page = database
            .message_history(chat.into(), before, 2)
            .await
            .unwrap();
        assert_eq!(page.messages.len(), expected.len());
        ids.extend(page.messages.iter().map(|(_, message)| message.server_id));
        assert_eq!(&ids[ids.len() - expected.len()..], expected);
        assert!(page.messages.iter().all(|(_, message)| {
            message.chat == chat && message.talker == "wxid_sender" && !message.is_self
        }));
        before = page.next_cursor.map(HistoryBefore::Cursor);
    }
    assert!(before.is_none());
    let older = database
        .message_history(chat.into(), Some(HistoryBefore::Sequence(2)), 10)
        .await
        .unwrap();
    assert_eq!(older.messages.len(), 2);
    assert_eq!(older.messages[1].1.server_id, 201);
    let private = database
        .message_history("wxid_friend".into(), None, 10)
        .await
        .unwrap();
    assert_eq!(private.messages.len(), 1);
    assert_eq!(private.messages[0].1.server_id, 301);
    let quote = database.quote_history(chat.into(), 102).await.unwrap();
    assert_eq!(quote.messages[quote.target_index].server_id, 102);
    assert_eq!(quote.messages[quote.target_index + 1].server_id, 202);
    assert!(database.quote_history(chat.into(), 301).await.is_err());
    assert!(database
        .message_history("different@chatroom".into(), None, 10)
        .await
        .unwrap()
        .messages
        .is_empty());
    assert!(database.watermarks.lock().await.is_empty());
    assert!(matches!(
        received.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    drop(database);
    drop(first);
    drop(second);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn snapshots_do_not_map_or_modify_live_shared_memory() {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "mimicwx-snapshot-isolation-{}",
        u64::from_le_bytes(random)
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("fixture.db");
    let key = [0u8; 32];
    let writer = open_fixture_database(&path, &key);
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE fixture (value INTEGER);
             INSERT INTO fixture VALUES (1);",
        )
        .unwrap();
    let shared_memory = directory.join("fixture.db-shm");
    let before = std::fs::read(&shared_memory).unwrap();
    let snapshots = super::snapshot::SnapshotReader::new(key.to_vec(), directory.clone()).unwrap();
    let reader = snapshots.open("fixture.db").unwrap();
    let database_path: String = reader
        .query_row("PRAGMA database_list", [], |row| row.get(2))
        .unwrap();
    assert!(database_path.contains("mimicwx-db-snapshots-"));
    assert_ne!(database_path, path.to_string_lossy());
    assert_eq!(
        reader
            .query_row("SELECT sum(value) FROM fixture", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(reader);
    assert_eq!(std::fs::read(&shared_memory).unwrap(), before);

    writer
        .execute("INSERT INTO fixture VALUES (2)", [])
        .unwrap();
    let reader = snapshots.open("fixture.db").unwrap();
    assert_eq!(
        reader
            .query_row("SELECT sum(value) FROM fixture", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
    drop(reader);
    drop(snapshots);
    drop(writer);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn snapshots_refresh_when_wal_frames_are_rewritten_without_file_growth() {
    use std::os::unix::fs::MetadataExt;

    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "mimicwx-snapshot-reused-wal-{}",
        u64::from_le_bytes(random)
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("fixture.db");
    let key = [0u8; 32];
    let writer = open_fixture_database(&path, &key);
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE fixture (value INTEGER);
             INSERT INTO fixture VALUES (1);
             PRAGMA wal_checkpoint(RESTART);",
        )
        .unwrap();
    let wal = directory.join("fixture.db-wal");
    let allocated = std::fs::metadata(&wal).unwrap();
    let snapshots = super::snapshot::SnapshotReader::new(key.to_vec(), directory.clone()).unwrap();
    for value in 2..=4 {
        writer
            .execute("INSERT INTO fixture VALUES (?1)", [value])
            .unwrap();
        let current = std::fs::metadata(&wal).unwrap();
        assert_eq!(allocated.ino(), current.ino());
        assert_eq!(allocated.len(), current.len());
        let reader = snapshots.open("fixture.db").unwrap();
        let actual: i64 = reader
            .query_row("SELECT sum(value) FROM fixture", [], |row| row.get(0))
            .unwrap();
        assert_eq!(actual, value * (value + 1) / 2);
    }
    drop(snapshots);
    drop(writer);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn read_only_connections_observe_live_wal_without_checkpointing_or_writing() {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "mimicwx-read-only-wal-{}",
        u64::from_le_bytes(random)
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("fixture.db");
    let key = [0u8; 32];
    let writer = open_fixture_database(&path, &key);
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE fixture (value INTEGER);
             INSERT INTO fixture VALUES (1);",
        )
        .unwrap();
    let snapshots = super::snapshot::SnapshotReader::new(key.to_vec(), directory.clone()).unwrap();
    let reader = snapshots.open("fixture.db").unwrap();
    assert!(reader.is_readonly(rusqlite::DatabaseName::Main).unwrap());
    assert_eq!(
        reader
            .query_row("SELECT sum(value) FROM fixture", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(reader);
    writer
        .execute("INSERT INTO fixture VALUES (2)", [])
        .unwrap();
    let before =
        ["fixture.db", "fixture.db-wal"].map(|name| std::fs::read(directory.join(name)).unwrap());
    let reader = snapshots.open("fixture.db").unwrap();
    assert_eq!(
        reader
            .query_row("SELECT sum(value) FROM fixture", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
    reader.execute_batch("PRAGMA query_only=OFF;").unwrap();
    assert_eq!(
        reader
            .execute("INSERT INTO fixture VALUES (3)", [])
            .unwrap_err()
            .sqlite_error_code(),
        Some(rusqlite::ErrorCode::ReadOnly)
    );
    drop(reader);
    for _iteration in 0..16 {
        let reader = snapshots.open("fixture.db").unwrap();
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM fixture", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
    assert_eq!(
        ["fixture.db", "fixture.db-wal"].map(|name| std::fs::read(directory.join(name)).unwrap()),
        before
    );
    assert!(directory.join("fixture.db-shm").metadata().unwrap().len() >= 32768);
    drop(snapshots);
    drop(writer);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn resolves_local_images_by_server_id_within_the_requested_conversation() {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "mimicwx-image-metadata-{}",
        u64::from_le_bytes(random)
    ));
    std::fs::create_dir_all(directory.join("message")).unwrap();
    let writer = rusqlite::Connection::open(directory.join("message/message_0.db")).unwrap();
    let key = [0u8; 32];
    assert_eq!(
        unsafe { super::sqlite3_key(writer.handle().cast(), key.as_ptr(), key.len() as i32) },
        0
    );
    let group = "44465171684@chatroom";
    let table = format!("Msg_{:x}", md5::compute(group));
    writer.execute_batch(&format!(
        "PRAGMA cipher_compatibility=4;
         CREATE TABLE [{table}] (server_id INTEGER, local_type INTEGER, create_time INTEGER, packed_info_data BLOB)"
    )).unwrap();
    let packed = b"\x0a\x20daab4a66b2d68deb5e34a662a158660e\x00";
    writer
        .execute(
            &format!("INSERT INTO [{table}] VALUES (?1, 3, 1794883218, ?2)"),
            rusqlite::params![7775901706581341840i64, packed.as_slice()],
        )
        .unwrap();
    let manager = super::DbManager::new("0".repeat(64), directory.clone()).unwrap();
    let metadata = manager
        .local_image_metadata(group, 7775901706581341840)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(metadata.hashes, vec!["daab4a66b2d68deb5e34a662a158660e"]);
    assert_eq!(metadata.chat, group);
    assert_eq!(metadata.create_time, 1794883218);
    assert!(manager
        .local_image_metadata("another@chatroom", 7775901706581341840)
        .await
        .unwrap()
        .is_none());
    assert!(manager
        .local_image_metadata(group, 123)
        .await
        .unwrap()
        .is_none());
    drop(manager);
    drop(writer);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn text_receipt_refreshes_and_accepts_a_delayed_database_acknowledgement() {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "mimicwx-delayed-receipt-{}",
        u64::from_le_bytes(random)
    ));
    std::fs::create_dir(&directory).unwrap();
    let manager = super::DbManager::new("0".repeat(64), directory.clone()).unwrap();
    let mut refresh = manager.subscribe_wal_events();
    let receipt = manager.subscribe_sent();
    let verify = manager.verify_sent(
        "sandbox@chatroom",
        "📣 测试课程\n第二行",
        None,
        &[],
        receipt,
    );
    let database_write = async {
        tokio::time::timeout(std::time::Duration::from_secs(2), refresh.recv())
            .await
            .unwrap()
            .unwrap();
        let mut message = image_message("other@chatroom", 1, true);
        message.content = "📣 测试课程\n第二行".into();
        manager.sent_content_tx.send(message.clone()).unwrap();
        message.chat = "sandbox@chatroom".into();
        message.content = "📣 测试课程".into();
        manager.sent_content_tx.send(message.clone()).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        message.content = "📣 测试课程\n第二行".into();
        manager.sent_content_tx.send(message).unwrap();
    };
    let (verified, ()) = tokio::join!(verify, database_write);
    assert!(verified.unwrap());
    drop(manager);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn concurrent_sends_do_not_share_a_single_receipt_or_duplicate_refreshes() {
    for image in [false, true] {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "mimicwx-concurrent-receipts-{}",
            u64::from_le_bytes(random)
        ));
        std::fs::create_dir(&directory).unwrap();
        let manager =
            std::sync::Arc::new(super::DbManager::new("0".repeat(64), directory.clone()).unwrap());
        let mut refresh = manager.subscribe_wal_events();
        let mut tasks = Vec::new();
        for _index in 0..2 {
            let manager = manager.clone();
            let receiver = if image {
                manager.subscribe_received()
            } else {
                manager.subscribe_sent()
            };
            tasks.push(tokio::spawn(async move {
                if image {
                    manager
                        .verify_image_sent("sandbox@chatroom", 100, receiver)
                        .await
                        .unwrap()
                } else {
                    manager
                        .verify_sent("sandbox@chatroom", "重复文字", None, &[], receiver)
                        .await
                        .unwrap()
                }
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), refresh.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), refresh.recv())
                .await
                .is_err()
        );
        let sender = if image {
            &manager.received_message_tx
        } else {
            &manager.sent_content_tx
        };
        let mut message = image_message("sandbox@chatroom", 100, true);
        message.content = "重复文字".into();
        sender.send(message.clone()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while tasks.iter().all(|task| !task.is_finished()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(tasks.iter().filter(|task| task.is_finished()).count(), 1);
        message.local_id += 1;
        message.server_id += 1;
        sender.send(message).unwrap();
        for task in tasks {
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap()
            );
        }
        drop(manager);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn native_reply_confirmation_rejects_plain_text_and_the_wrong_reference() {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "mimicwx-native-reply-receipt-{}",
        u64::from_le_bytes(random)
    ));
    std::fs::create_dir(&directory).unwrap();
    let manager = super::DbManager::new("0".repeat(64), directory.clone()).unwrap();
    let receipt = manager.subscribe_sent();
    let verify = manager.verify_sent("sandbox@chatroom", "完整回复", Some("123"), &[], receipt);
    let write = async {
        let mut message = image_message("sandbox@chatroom", 100, true);
        message.msg_type = 1;
        message.content = "完整回复".into();
        manager.sent_content_tx.send(message.clone()).unwrap();
        tokio::task::yield_now().await;
        message.msg_type = 49;
        message.content = "<msg><appmsg><title>完整回复</title><type>57</type>\
            <refermsg><svrid>999</svrid></refermsg></appmsg></msg>"
            .into();
        manager.sent_content_tx.send(message.clone()).unwrap();
        tokio::task::yield_now().await;
        message.content = message
            .content
            .replace("<svrid>999</svrid>", "<svrid>123</svrid>");
        message.chat = "other@chatroom".into();
        manager.sent_content_tx.send(message.clone()).unwrap();
        tokio::task::yield_now().await;
        message.chat = "sandbox@chatroom".into();
        message.is_self = false;
        manager.sent_content_tx.send(message.clone()).unwrap();
        tokio::task::yield_now().await;
        message.is_self = true;
        manager.sent_content_tx.send(message).unwrap();
    };
    let (verified, ()) = tokio::join!(verify, write);
    assert!(verified.unwrap());
    drop(manager);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn snapshots_refresh_after_wal_growth_main_updates_and_rotation() {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "mimicwx-wal-rotation-{}",
        u64::from_le_bytes(random)
    ));
    std::fs::create_dir_all(directory.join("message")).unwrap();
    let path = directory.join("message/message_0.db");
    let writer = rusqlite::Connection::open(&path).unwrap();
    let key = [0u8; 32];
    assert_eq!(
        unsafe { super::sqlite3_key(writer.handle().cast(), key.as_ptr(), key.len() as i32) },
        0
    );
    writer
        .execute_batch(
            "PRAGMA cipher_compatibility=4;
         PRAGMA journal_mode=WAL;
         CREATE TABLE Name2Id (user_name TEXT NOT NULL);
         INSERT INTO Name2Id (rowid, user_name) VALUES (1, 'sandbox@chatroom'), (2, 'wxid_sender');
         CREATE TABLE ChatMsg_1 (
             local_id INTEGER, server_id INTEGER, create_time INTEGER,
             message_content TEXT, local_type INTEGER, real_sender_id INTEGER, status INTEGER
         );
         INSERT INTO ChatMsg_1 VALUES (1, 501, 1, '轮换前', 1, 2, 3);
         PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
    let manager = super::DbManager::new("0".repeat(64), directory.clone()).unwrap();
    manager.mark_all_read().await.unwrap();
    drop(writer);
    for suffix in ["-wal", "-shm"] {
        let source = directory.join(format!("message/message_0.db{suffix}"));
        if source.exists() {
            std::fs::rename(source, directory.join(format!("message/retired{suffix}"))).unwrap();
        }
    }
    assert!(manager.get_new_messages().await.unwrap().is_empty());
    let writer = open_fixture_database(&path, &key);
    let before_write = super::message_database_identity(&directory, "message/message_0.db");
    writer
        .execute_batch(
            "PRAGMA query_only=OFF;
         INSERT INTO ChatMsg_1 VALUES (2, 502, 2, '轮换后', 1, 2, 3);",
        )
        .unwrap();
    let after_write = super::message_database_identity(&directory, "message/message_0.db");
    assert_eq!(before_write.files[0], after_write.files[0]);
    assert_ne!(before_write.revisions[1], after_write.revisions[1]);
    let messages = manager.get_new_messages().await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].local_id, 2);
    assert_eq!(messages[0].content, "轮换后");
    assert_eq!(
        manager.msg_file_ids.lock().unwrap()["message/message_0.db"].revisions[1],
        after_write.revisions[1]
    );
    assert!(manager.get_new_messages().await.unwrap().is_empty());
    let before_append = super::message_database_identity(&directory, "message/message_0.db");
    writer
        .execute_batch("INSERT INTO ChatMsg_1 VALUES (3, 503, 3, '仅 WAL 追加', 1, 2, 3);")
        .unwrap();
    let after_append = super::message_database_identity(&directory, "message/message_0.db");
    assert_eq!(before_append.files, after_append.files);
    assert_eq!(before_append.revisions[0], after_append.revisions[0]);
    assert_ne!(before_append.revisions[1], after_append.revisions[1]);
    let messages = manager.get_new_messages().await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].local_id, 3);
    assert_eq!(messages[0].content, "仅 WAL 追加");
    assert!(manager.get_new_messages().await.unwrap().is_empty());
    writer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    assert!(manager.get_new_messages().await.unwrap().is_empty());
    let before_main_write = super::message_database_identity(&directory, "message/message_0.db");
    let replacement_path = directory.join("replacement.db");
    let replacement = open_fixture_database(&replacement_path, &key);
    replacement
        .execute_batch(
            "PRAGMA query_only=OFF;
         CREATE TABLE Name2Id (user_name TEXT NOT NULL);
         INSERT INTO Name2Id (rowid, user_name) VALUES (1, 'sandbox@chatroom'), (2, 'wxid_sender');
         CREATE TABLE ChatMsg_1 (
             local_id INTEGER, server_id INTEGER, create_time INTEGER,
             message_content TEXT, local_type INTEGER, real_sender_id INTEGER, status INTEGER
         );
         INSERT INTO ChatMsg_1 VALUES (1, 501, 1, '轮换前', 1, 2, 3);
         INSERT INTO ChatMsg_1 VALUES (2, 502, 2, '轮换后', 1, 2, 3);
         INSERT INTO ChatMsg_1 VALUES (3, 503, 3, '仅 WAL 追加', 1, 2, 3);
         INSERT INTO ChatMsg_1 VALUES (4, 504, 4, '主库直接更新', 1, 2, 3);",
        )
        .unwrap();
    drop(replacement);
    std::fs::copy(replacement_path, &path).unwrap();
    let after_main_write = super::message_database_identity(&directory, "message/message_0.db");
    assert_eq!(before_main_write.files, after_main_write.files);
    assert_eq!(
        before_main_write.revisions[1],
        after_main_write.revisions[1]
    );
    assert_ne!(
        before_main_write.revisions[0],
        after_main_write.revisions[0]
    );
    let messages = manager.get_new_messages().await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].local_id, 4);
    assert_eq!(messages[0].content, "主库直接更新");
    assert!(manager.get_new_messages().await.unwrap().is_empty());
    drop(writer);
    drop(manager);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn media_rows_wait_for_wechat_to_finish_writing_before_advancing_the_cursor() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE Name2Id (user_name TEXT NOT NULL);
         INSERT INTO Name2Id (rowid, user_name) VALUES (1, 'sandbox@chatroom'), (2, 'wxid_self');
         CREATE TABLE ChatMsg_1 (
             local_id INTEGER, server_id INTEGER, create_time INTEGER,
             message_content TEXT, local_type INTEGER, real_sender_id INTEGER, status INTEGER
         );",
        )
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    connection
        .execute(
            "INSERT INTO ChatMsg_1 VALUES (1, 0, ?1, '', 47, 2, 3)",
            [now],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO ChatMsg_1 VALUES (2, 502, ?1, '下一条消息', 1, 2, 3)",
            [now],
        )
        .unwrap();
    let connection = std::sync::Arc::new(std::sync::Mutex::new(connection));
    let manager = super::DbManager::new(
        "0".repeat(64),
        std::path::PathBuf::from("wxid_self_0000/db_storage"),
    )
    .unwrap();
    manager
        .msg_conns
        .lock()
        .unwrap()
        .insert("message/message_0.db".into(), connection.clone());

    assert!(manager.get_new_messages().await.unwrap().is_empty());
    assert!(manager.watermarks.lock().await.is_empty());
    connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE ChatMsg_1 SET server_id=501, message_content=?1 WHERE local_id=1",
            ["<msg><emoji md5=\"d89746888da2d9510b64a9f031eaecd5\"/></msg>"],
        )
        .unwrap();
    let messages = manager.get_new_messages().await.unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.local_id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(messages[0].server_id, 501);
    assert!(messages[0].content.contains("<emoji"));
    assert!(manager.get_new_messages().await.unwrap().is_empty());

    connection
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO ChatMsg_1 VALUES (3, 0, ?1, '', 47, 2, 3)",
            [now - 60],
        )
        .unwrap();
    connection
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO ChatMsg_1 VALUES (4, 504, ?1, '未完成的旧表情不阻塞会话', 1, 2, 3)",
            [now],
        )
        .unwrap();
    let messages = manager.get_new_messages().await.unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.local_id)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
}

#[test]
fn merged_forward_preview_detection_ignores_links_and_replies() {
    let link = "<msg><appmsg><title>群聊的聊天记录</title><type>5</type>\
        <url>https://example.test/article</url></appmsg></msg>";
    assert!(!message_content_pending(49, link, false));
    let other_template = "<msg><appmsg><title>群聊的聊天记录</title><type>40</type>\
        <url>https://support.weixin.qq.com/security/readtemplate?t=other</url></appmsg></msg>";
    assert!(!message_content_pending(49, other_template, false));
    let other_host = "<msg><appmsg><title>群聊的聊天记录</title><type>40</type>\
        <url>https://support.weixin.qq.com.example.test/security/readtemplate?t=w_security_center_website/upgrade\
        </url></appmsg></msg>";
    assert!(!message_content_pending(49, other_host, false));
    let reply = "<msg><appmsg><title>看看他的聊天记录</title><type>57</type></appmsg></msg>";
    assert!(!message_content_pending(49, reply, false));
}

#[test]
fn merged_forward_preview_accepts_wechat_fallback_query_variants() {
    for (app_type, url) in [
        (5, "https://support.weixin.qq.com/security/readtemplate?t=w_security_center_website/upgrade"),
        (40, "https://support.weixin.qq.com/security/readtemplate?wechat_real_lang=zh_CN&amp;t=w_security_center_website%2Fupgrade"),
        (40, "https://support.weixin.qq.com/cgi-bin/mmsupport-bin/readtemplate?t=page/favorite_record__w_unsupport&amp;wechat_real_lang=zh_CN"),
    ] {
        let content = format!(
            "<msg><appmsg><title>群聊的聊天记录</title><type>{app_type}</type>\
             <url>{url}</url></appmsg></msg>"
        );
        assert!(message_content_pending(49, &content, false), "{url}");
    }
}

#[tokio::test]
async fn merged_forward_preview_waits_for_the_complete_record() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE Name2Id (user_name TEXT NOT NULL);
             INSERT INTO Name2Id (rowid, user_name) VALUES (1, 'wxid_friend');
             CREATE TABLE ChatMsg_1 (
                 local_id INTEGER PRIMARY KEY, server_id INTEGER, create_time INTEGER,
                 message_content TEXT, local_type INTEGER, real_sender_id INTEGER
             );",
        )
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let preview_kind = (40_i64 << 32) | 49;
    let forward_kind = (19_i64 << 32) | 49;
    let preview = "<msg><appmsg><title>群聊的聊天记录</title><des>只有预览</des><type>40</type>\
        <url><![CDATA[https://support.weixin.qq.com/security/readtemplate?t=w_security_center_website/upgrade&wechat_real_lang=zh_CN]]>\
        </url></appmsg></msg>";
    assert!(matches!(
        super::parse_msg_content(preview_kind, preview),
        super::MsgContent::App {
            app_type: Some(40),
            ..
        }
    ));
    connection
        .execute(
            "INSERT INTO ChatMsg_1 VALUES (1, 501, ?1, ?2, ?3, 1)",
            rusqlite::params![now, preview, preview_kind],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO ChatMsg_1 VALUES (2, 502, ?1, '后续发言', 1, 1)",
            [now],
        )
        .unwrap();
    let connection = std::sync::Arc::new(std::sync::Mutex::new(connection));
    let manager = super::DbManager::new(
        "0".repeat(64),
        std::path::PathBuf::from("wxid_self_0000/db_storage"),
    )
    .unwrap();
    manager
        .msg_conns
        .lock()
        .unwrap()
        .insert("message/message_0.db".into(), connection.clone());

    assert!(manager.get_new_messages().await.unwrap().is_empty());
    assert!(manager.watermarks.lock().await.is_empty());

    let nodes: String = (1..=97)
        .map(|index| {
            format!(
                "<dataitem datatype=\"1\"><sourcename>群友</sourcename>\
                 <datadesc>第{index}条正文</datadesc></dataitem>"
            )
        })
        .collect();
    let record = format!(
        "<msg><appmsg><title>群聊的聊天记录</title>\
         <recorditem><![CDATA[<recordinfo><datalist count=\"97\">{nodes}</datalist>\
         </recordinfo>]]></recorditem></appmsg></msg>"
    );
    connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE ChatMsg_1 SET message_content=?1, local_type=?2 WHERE local_id=1",
            rusqlite::params![record, forward_kind],
        )
        .unwrap();
    let messages = manager.get_new_messages().await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].server_id, 501);
    assert_eq!(messages[1].server_id, 502);
    let runtime = crate::onebot::OneBotRuntime::new("wxid_self".into(), "机器人".into(), None);
    let event = runtime.event_from_message(&messages[0]).await.unwrap();
    assert_eq!(event["message_type"], "private");
    assert_eq!(event["message"][0]["type"], "forward");
    let context = event["message"][1]["data"]["text"].as_str().unwrap();
    assert_eq!(context.lines().count(), 99);
    assert!(context.contains("第97条正文"));
    assert!(manager.get_new_messages().await.unwrap().is_empty());

    connection
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO ChatMsg_1 VALUES (3, 503, ?1, ?2, ?3, 1)",
            rusqlite::params![now - 60, preview, preview_kind],
        )
        .unwrap();
    let messages = manager.get_new_messages().await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].server_id, 503);
    let expired_preview = runtime.event_from_message(&messages[0]).await.unwrap();
    let text = expired_preview["message"][0]["data"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("[聊天记录正文暂不可用]"));
    assert!(!text.contains("https://"));
}

#[test]
fn reads_favorite_emoji_media_by_checksum_without_changing_the_database() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE kNonStoreEmoticonTable (
         md5 TEXT, aes_key TEXT, cdn_url TEXT, extern_url TEXT,
         extern_md5 TEXT, encrypt_url TEXT, thumb_url TEXT);
         INSERT INTO kNonStoreEmoticonTable VALUES (
         'd89746888da2d9510b64a9f031eaecd5', '000102030405060708090a0b0c0d0e0f',
         '', '', '', 'https://example.test/encrypted?x=1&filekey=a%2Bb', '');
         PRAGMA query_only=ON;",
        )
        .unwrap();
    let metadata = emoji_media_from_connection(&connection, "D89746888DA2D9510B64A9F031EAECD5")
        .unwrap()
        .unwrap();
    assert_eq!(metadata["aeskey"], "000102030405060708090a0b0c0d0e0f");
    assert_eq!(
        metadata["encrypturl"],
        "https://example.test/encrypted?x=1&filekey=a%2Bb"
    );
    assert!(!metadata.contains_key("cdnurl"));
    assert!(emoji_media_from_connection(&connection, &"0".repeat(32))
        .unwrap()
        .is_none());
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM kNonStoreEmoticonTable", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

fn image_message(chat: &str, create_time: i64, is_self: bool) -> DbMessage {
    DbMessage {
        local_id: 1,
        server_id: 1,
        create_time,
        content: "<msg><img/></msg>".into(),
        source: String::new(),
        parsed: MsgContent::Image { path: None },
        msg_type: 3,
        talker: "wxid_sandbox_self".into(),
        talker_display_name: "机器人".into(),
        talker_nick_name: "机器人".into(),
        talker_remark: String::new(),
        chat: chat.into(),
        chat_display_name: "测试会话".into(),
        chat_nick_name: "测试会话".into(),
        chat_remark: String::new(),
        is_self,
    }
}

#[test]
fn account_identity_excludes_wechat_directory_suffix() {
    assert_eq!(account_wxid("wxid_sandbox123_14fd"), "wxid_sandbox123");
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
        "wxid_sandbox_friend",
        "测试",
        "sandbox@chatroom",
        "测试",
    ));
    assert!(!sent_message_matches(
        "sandbox@chatroom",
        "完整测试消息",
        "sandbox@chatroom",
        "测试",
    ));
    assert!(sent_message_matches(
        "sandbox@chatroom",
        "完整测试消息\n",
        "sandbox@chatroom",
        "完整测试消息",
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
fn sticker_confirmation_requires_the_native_type_and_matching_checksum() {
    let started_at = 1_700_000_000;
    let checksum = "abcdef0123456789abcdef0123456789";
    let mut message = image_message("sandbox@chatroom", started_at + 5, true);
    assert!(!sticker_receipt_matches(
        "sandbox@chatroom",
        started_at,
        checksum,
        &message,
    ));
    message.msg_type = 47;
    message.content = format!("<msg><emoji md5=\"{}\" /></msg>", checksum.to_uppercase());
    message.parsed = parse_msg_content(message.msg_type, &message.content);
    assert!(sticker_receipt_matches(
        "sandbox@chatroom",
        started_at,
        checksum,
        &message,
    ));
    assert!(!sticker_receipt_matches(
        "other@chatroom",
        started_at,
        checksum,
        &message,
    ));
    assert!(!sticker_receipt_matches(
        "sandbox@chatroom",
        started_at + 8,
        checksum,
        &message,
    ));
    assert!(!sticker_receipt_matches(
        "sandbox@chatroom",
        started_at,
        "different",
        &message,
    ));
    message.is_self = false;
    assert!(!sticker_receipt_matches(
        "sandbox@chatroom",
        started_at,
        checksum,
        &message,
    ));
    message.is_self = true;
    message.content = "<msg><emoji md5=\"\" type=\"2\" len=\"0\" /></msg>".into();
    assert!(!sticker_receipt_matches(
        "sandbox@chatroom",
        started_at,
        checksum,
        &message,
    ));
    message.content = "<msg><emoji /></msg>".into();
    assert!(!sticker_receipt_matches(
        "sandbox@chatroom",
        started_at,
        checksum,
        &message,
    ));
}

#[test]
fn outgoing_sticker_placeholders_wait_for_the_file_to_be_imported() {
    let placeholder = "<msg><emoji md5=\"\" type=\"2\" len=\"0\" /></msg>";
    let complete = "<msg><emoji md5=\"abcdef0123456789abcdef0123456789\" /></msg>";
    assert!(message_content_pending(47, placeholder, true));
    assert!(message_content_pending(47, "<msg><emoji /></msg>", true));
    assert!(!message_content_pending(47, complete, true));
    assert!(!message_content_pending(47, placeholder, false));
    assert!(message_content_pending(3, "", true));
    assert!(message_content_pending(47, "", false));
    assert!(!message_content_pending(1, "", true));
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
    )
    .unwrap();
    let meta = build_single_table_meta(&conn, "Msg_sandbox").unwrap();
    let mut statement = conn.prepare(&meta.select_sql).unwrap();
    let results: Vec<(String, bool)> = statement
        .query_map([0], |row| {
            let sender = message_sender(&conn, row, 5)?;
            let is_self = message_is_self("wxid_self", &sender, row.get(6)?, row.get(4)?);
            Ok((sender, is_self))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        results,
        vec![("wxid_self".into(), true), ("wxid_friend".into(), false)]
    );
}

#[test]
fn textual_senders_and_legacy_statuses_remain_supported() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let sender = conn
        .query_row("SELECT 'wxid_friend'", [], |row| {
            message_sender(&conn, row, 0)
        })
        .unwrap();
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

#[test]
fn media_attributes_decode_xml_entities() {
    for kind in [3, 47] {
        for separator in ["&amp;", "&#38;", "&#x26;"] {
            let url = format!("https://example.test/a?x=1{separator}filekey=a%26b%3Dc");
            let content = if kind == 3 {
                format!("<msg><img cdnmidimgurl=\"{url}\"/></msg>")
            } else {
                format!("<msg><emoji cdnurl=\"{url}\"></emoji></msg>")
            };
            let path = match parse_msg_content(kind, &content) {
                MsgContent::Image { path } => path,
                MsgContent::Emoji { url } => url,
                _ => unreachable!(),
            };
            assert_eq!(
                path.as_deref(),
                Some("https://example.test/a?x=1&filekey=a%26b%3Dc"),
            );
        }
    }
}

#[test]
fn media_attributes_decode_once_and_reject_invalid_entities() {
    let parsed = parse_msg_content(
        47,
        "<msg><emoji cdnurl=\"https://example.test/a?filekey=a&amp;amp;b\"/></msg>",
    );
    assert!(matches!(
        parsed,
        MsgContent::Emoji { url: Some(url) } if url == "https://example.test/a?filekey=a&amp;b",
    ));
    let parsed = parse_msg_content(
        3,
        "<msg><img cdnmidimgurl=\"https://example.test/a?filekey=a&unknown;b\"/></msg>",
    );
    assert!(matches!(parsed, MsgContent::Image { path: None }));
}

#[test]
fn session_updates_emit_only_changes_after_monitor_start() {
    let row = |chat: &str, local_id: i64, timestamp: i64| SessionMessage {
        chat: chat.into(),
        summary: "测试".into(),
        timestamp,
        local_id,
        server_id: 0,
        unread: true,
        msg_type: 1,
        sub_type: 0,
        ext_type: 0,
        sender: "wxid_friend".into(),
        sender_display_name: "群友".into(),
    };
    let mut cursors = HashMap::new();
    let first = session_updates_from_rows(
        vec![row("old@chatroom", 7, 99), row("room@chatroom", 8, 101)],
        &mut cursors,
        100,
    );
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].local_id, 8);
    assert_eq!(
        cursors["old@chatroom"],
        SessionCursor {
            local_id: 7,
            timestamp: 99,
        },
    );
    assert!(
        session_updates_from_rows(vec![row("room@chatroom", 8, 101)], &mut cursors, 100,)
            .is_empty()
    );
    let next = session_updates_from_rows(vec![row("room@chatroom", 9, 102)], &mut cursors, 100);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].local_id, 9);
}

#[test]
fn session_fast_path_rejects_truncated_summaries_and_non_text_messages() {
    let mut snapshot = SessionMessage {
        chat: "room@chatroom".into(),
        summary: "@机器人 你好".into(),
        timestamp: 100,
        local_id: 42,
        server_id: 0,
        unread: true,
        msg_type: 1,
        sub_type: 0,
        ext_type: 0,
        sender: "wxid_friend".into(),
        sender_display_name: "群友".into(),
    };
    assert!(snapshot.can_dispatch_text("wxid_bot"));
    for summary in ["a".repeat(192), "甲".repeat(64), "🙂".repeat(48)] {
        snapshot.summary = summary;
        assert!(!snapshot.can_dispatch_text("wxid_bot"));
    }
    snapshot.summary = "完整短消息".into();
    snapshot.msg_type = 49;
    assert!(!snapshot.can_dispatch_text("wxid_bot"));
    snapshot.msg_type = 1;
    snapshot.sub_type = 57;
    assert!(!snapshot.can_dispatch_text("wxid_bot"));
    snapshot.sub_type = 0;
    snapshot.ext_type = 1;
    assert!(!snapshot.can_dispatch_text("wxid_bot"));
    snapshot.ext_type = 0;
    snapshot.sender = "wxid_bot".into();
    assert!(!snapshot.can_dispatch_text("wxid_bot"));
    snapshot.sender.clear();
    assert!(!snapshot.can_dispatch_text("wxid_bot"));
    snapshot.sender = "wxid_friend".into();
    snapshot.chat = "wxid_friend".into();
    assert!(snapshot.can_dispatch_text("wxid_bot"));
    snapshot.unread = false;
    assert!(!snapshot.can_dispatch_text("wxid_bot"));
    snapshot.unread = true;
    snapshot.sender.clear();
    assert!(snapshot.can_dispatch_text("wxid_bot"));
    snapshot.chat = "wxid_bot".into();
    assert!(!snapshot.can_dispatch_text("wxid_bot"));
}
