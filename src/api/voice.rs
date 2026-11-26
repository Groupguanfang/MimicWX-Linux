use super::{finish_send, AppState, InputCommand, SendResponse};
use std::sync::Arc;
use tokio::sync::oneshot;

pub async fn submit_voice_bytes(
    state: &Arc<AppState>,
    to: &str,
    bytes: Vec<u8>,
) -> Result<SendResponse, String> {
    let database = state.db.as_ref().ok_or("语音发送需要数据库确认")?;
    let matches = database
        .get_contacts()
        .await
        .into_iter()
        .filter(|contact| contact.username == to || contact.display_name == to)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err("无法唯一确认语音目标会话".into());
    }
    let audio = crate::audio::prepare(&bytes)
        .await
        .map_err(|error| error.to_string())?;
    let receipts = database.subscribe_received();
    let chat = matches[0].username.clone();
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (reply, response) = oneshot::channel();
    state
        .input_tx
        .send(InputCommand::SendVoice {
            to: to.to_string(),
            path: audio.path.clone(),
            duration_ms: audio.duration_ms,
            reply,
        })
        .await
        .map_err(|_| "InputEngine actor 已停止")?;
    let (sent, verified, message) = response
        .await
        .map_err(|_| "语音 actor 响应通道已关闭")?
        .map_err(|error| format!("发送语音失败: {error}"))?;
    let duration = audio.duration_ms;
    let database = database.clone();
    let verification = async move {
        database
            .verify_voice_sent(&chat, started_at, duration, receipts)
            .await
            .unwrap_or(false)
    };
    Ok(finish_send(
        SendResponse {
            sent,
            verified,
            confirmation_pending: false,
            message,
        },
        Some(verification),
        false,
        state.tx.clone(),
        serde_json::json!({ "type":"sent", "to":to, "media_type":"record" }),
    )
    .await)
}
