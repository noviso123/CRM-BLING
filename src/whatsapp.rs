use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::{AppState, db};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Deserialize)]
pub struct WebhookQuery {
    #[serde(rename = "hub.mode")]
    mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    challenge: Option<String>,
}

pub async fn verify_webhook(
    State(state): State<AppState>,
    Query(query): Query<WebhookQuery>,
) -> Response {
    let Some(expected) = state.config.wa_verify_token.as_deref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let supplied = query.verify_token.as_deref().unwrap_or_default();
    let matches = supplied.len() == expected.len()
        && bool::from(supplied.as_bytes().ct_eq(expected.as_bytes()));
    if query.mode.as_deref() == Some("subscribe") && matches {
        if let Some(challenge) = query.challenge {
            return (StatusCode::OK, challenge).into_response();
        }
    }
    StatusCode::FORBIDDEN.into_response()
}

pub async fn receive_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(app_secret) = state.config.wa_app_secret.as_deref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(signature) = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("sha256="))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(signature_bytes) = hex_decode(signature) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(app_secret.as_bytes()) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    mac.update(&body);
    if mac.verify_slice(&signature_bytes).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if process_webhook(&state, &payload).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    StatusCode::OK.into_response()
}

pub async fn send_text(state: &AppState, phone: &str, text: &str) -> anyhow::Result<()> {
    send_payload(
        state,
        phone,
        "text",
        json!({ "preview_url": false, "body": text }),
        text,
    )
    .await
}

pub async fn send_template(
    state: &AppState,
    phone: &str,
    name: &str,
    language: &str,
    parameters: &[String],
) -> anyhow::Result<()> {
    let template = if parameters.is_empty() {
        json!({"name": name, "language": {"code": language}})
    } else {
        json!({
            "name": name,
            "language": {"code": language},
            "components": [{
                "type": "body",
                "parameters": parameters.iter().map(|value| json!({"type":"text", "text":value})).collect::<Vec<_>>()
            }]
        })
    };
    let preview = if parameters.is_empty() {
        format!("[Modelo aprovado: {name}]")
    } else {
        format!("[Modelo aprovado: {name}] {}", parameters.join(" · "))
    };
    send_payload(state, phone, "template", template, &preview).await
}

async fn send_payload(
    state: &AppState,
    phone: &str,
    kind: &str,
    content: Value,
    stored_body: &str,
) -> anyhow::Result<()> {
    let token = state
        .config
        .wa_access_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("WhatsApp access token is not configured"))?;
    let phone_number_id = state
        .config
        .wa_phone_number_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("WhatsApp phone number ID is not configured"))?;

    let url = format!(
        "https://graph.facebook.com/{}/{}/messages",
        state.config.graph_api_version, phone_number_id
    );
    let mut payload = json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": phone,
        "type": kind
    });
    payload[kind] = content;
    let response = state
        .http
        .post(url)
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Could not reach the WhatsApp Graph API"))?;

    let status = response.status();
    let response_body: Value = response.json().await.unwrap_or_else(|_| Value::Null);
    if !status.is_success() {
        let error_code = response_body
            .pointer("/error/code")
            .and_then(Value::as_i64)
            .map(|code| format!(" (Meta error {code})"))
            .unwrap_or_default();
        anyhow::bail!("WhatsApp API returned HTTP {status}{error_code}");
    }

    let wa_id = response_body
        .pointer("/messages/0/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("WhatsApp API response did not contain a message ID"))?;
    db::save_message(
        &state.db,
        wa_id,
        phone,
        "outbound",
        stored_body,
        "sent",
        None,
    )
    .await?;
    Ok(())
}

async fn process_webhook(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    for entry in payload
        .pointer("/entry")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for change in entry
            .pointer("/changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let value = &change["value"];
            for contact in value
                .get("contacts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(phone) = contact.get("wa_id").and_then(Value::as_str) else {
                    continue;
                };
                if !(8..=15).contains(&phone.len())
                    || !phone.bytes().all(|byte| byte.is_ascii_digit())
                {
                    continue;
                }
                let Some(name) = contact
                    .pointer("/profile/name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty() && name.chars().count() <= 80)
                else {
                    continue;
                };
                db::upsert_contact(&state.db, phone, name, "whatsapp").await?;
            }

            for message in value
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(wa_id) = message.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(phone) = message.get("from").and_then(Value::as_str) else {
                    continue;
                };
                let Some(body) = message_body(message) else {
                    continue;
                };
                let timestamp = message
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<i64>().ok());
                db::save_message(
                    &state.db, wa_id, phone, "inbound", &body, "received", timestamp,
                )
                .await?;
            }

            for status in value
                .get("statuses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let (Some(wa_id), Some(status)) = (
                    status.get("id").and_then(Value::as_str),
                    status.get("status").and_then(Value::as_str),
                ) {
                    db::update_status(&state.db, wa_id, status).await?;
                }
            }
        }
    }
    Ok(())
}

fn message_body(message: &Value) -> Option<String> {
    let kind = message
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    let text = match kind {
        "text" => message
            .pointer("/text/body")
            .and_then(Value::as_str)
            .map(str::to_owned),
        "image" => message
            .pointer("/image/caption")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some("Imagem recebida".into())),
        "video" => message
            .pointer("/video/caption")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some("Vídeo recebido".into())),
        "audio" => Some("Áudio recebido".into()),
        "document" => message
            .pointer("/document/filename")
            .and_then(Value::as_str)
            .map(|name| format!("Documento recebido: {name}"))
            .or_else(|| Some("Documento recebido".into())),
        "sticker" => Some("Figurinha recebida".into()),
        "location" => Some("Localização recebida".into()),
        "interactive" => message
            .pointer("/interactive/button_reply/title")
            .and_then(Value::as_str)
            .or_else(|| {
                message
                    .pointer("/interactive/list_reply/title")
                    .and_then(Value::as_str)
            })
            .map(str::to_owned),
        _ => None,
    }?;
    Some(text)
}

fn hex_decode(value: &str) -> Result<Vec<u8>, ()> {
    if value.len() % 2 != 0 {
        return Err(());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0]).ok_or(())?;
            let low = hex_digit(pair[1]).ok_or(())?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
