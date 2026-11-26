use super::*;
use serde_json::{json, Value};
use std::time::Duration;

fn submitted_response() -> SendResponse {
    SendResponse {
        sent: true,
        verified: false,
        confirmation_pending: false,
        message: "submitted".into(),
    }
}

#[tokio::test]
async fn deferred_confirmation_returns_before_the_database_and_reports_the_final_result() {
    for verified in [true, false] {
        let (events, mut receiver) = broadcast::channel(4);
        let (complete, receipt) = oneshot::channel();
        let response = tokio::time::timeout(
            Duration::from_millis(500),
            finish_send(
                submitted_response(),
                Some(async move { receipt.await.unwrap() }),
                false,
                events,
                json!({ "type": "sent", "to": "sandbox@chatroom", "text": "测试" }),
            ),
        )
        .await
        .unwrap();
        assert!(response.sent);
        assert!(!response.verified);
        assert!(response.confirmation_pending);
        let pending: Value = serde_json::from_str(&receiver.recv().await.unwrap()).unwrap();
        assert_eq!(pending["type"], "sent");
        assert_eq!(pending["verified"], false);
        assert_eq!(pending["confirmation_pending"], true);
        complete.send(verified).unwrap();
        let confirmation = tokio::time::timeout(Duration::from_millis(500), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let confirmation: Value = serde_json::from_str(&confirmation).unwrap();
        assert_eq!(confirmation["type"], "send_confirmation");
        assert_eq!(confirmation["to"], "sandbox@chatroom");
        assert_eq!(confirmation["text"], "测试");
        assert_eq!(confirmation["verified"], verified);
        assert_eq!(confirmation["confirmation_pending"], false);
    }
}

#[tokio::test]
async fn synchronous_confirmation_still_waits_for_the_actual_receipt() {
    let (events, _) = broadcast::channel(4);
    let (complete, receipt) = oneshot::channel();
    let verification = tokio::spawn(finish_send(
        submitted_response(),
        Some(async move { receipt.await.unwrap() }),
        true,
        events,
        json!({ "type": "sent", "to": "sandbox@chatroom" }),
    ));
    tokio::task::yield_now().await;
    assert!(!verification.is_finished());
    complete.send(true).unwrap();
    let response = verification.await.unwrap();
    assert!(response.verified);
    assert!(!response.confirmation_pending);
}

#[tokio::test]
async fn failed_submissions_do_not_start_a_background_confirmation() {
    let (events, _) = broadcast::channel(4);
    let (complete, receipt) = oneshot::channel::<bool>();
    let mut response = submitted_response();
    response.sent = false;
    let response = finish_send(
        response,
        Some(async move { receipt.await.unwrap() }),
        false,
        events,
        json!({ "type": "sent", "to": "sandbox@chatroom" }),
    )
    .await;
    assert!(!response.sent);
    assert!(!response.verified);
    assert!(!response.confirmation_pending);
    assert!(complete.is_closed());
}

#[tokio::test]
async fn image_files_remain_available_until_the_deferred_confirmation_finishes() {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).unwrap();
    let path = std::env::temp_dir().join(format!(
        "mimicwx-pending-image-test-{}",
        u64::from_le_bytes(random),
    ));
    std::fs::write(&path, b"image data").unwrap();
    let pending_file = PendingImageFile(path.to_string_lossy().into_owned());
    let (events, mut receiver) = broadcast::channel(4);
    let (complete, receipt) = oneshot::channel();
    let response = finish_send(
        submitted_response(),
        Some(async move {
            let _pending_file = pending_file;
            receipt.await.unwrap()
        }),
        false,
        events,
        json!({ "type": "sent", "to": "sandbox@chatroom" }),
    )
    .await;
    assert!(response.confirmation_pending);
    assert!(path.exists());
    receiver.recv().await.unwrap();
    complete.send(true).unwrap();
    tokio::time::timeout(Duration::from_millis(500), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!path.exists());
}
