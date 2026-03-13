use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::crypto::private_box::{box_message, unbox_message};
use crate::error::{EgreError, Result};
use crate::feed::models::Message;
use crate::identity::{Identity, PublicId};

pub const PRIVATE_BOX_SCHEMA_ID: &str = "private_box/v1";
const PRIVATE_BOX_TYPE: &str = "private_box";
const ENCRYPT_FOR_FIELD: &str = "encrypt_for";

#[derive(Debug, Clone)]
pub struct PreparedPrivateBox {
    pub plaintext_content: serde_json::Value,
    pub encrypted_content: serde_json::Value,
    pub schema_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrivateBoxEnvelope {
    #[serde(rename = "type")]
    msg_type: String,
    sender: PublicId,
    #[serde(rename = "box")]
    boxed_payload: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inner_schema_id: Option<String>,
}

pub fn prepare_for_publish(
    sender: &Identity,
    mut content: serde_json::Value,
    original_schema_id: Option<String>,
) -> Result<Option<PreparedPrivateBox>> {
    let Some(recipients) = extract_recipients(&mut content)? else {
        return Ok(None);
    };

    let plaintext = serde_json::to_vec(&content)?;
    let mut recipient_keys = Vec::with_capacity(recipients.len() + 1);
    for recipient in recipients {
        recipient_keys.push(recipient.to_x25519_public_key()?);
    }
    recipient_keys.push(sender.to_x25519_public_key());

    let ciphertext = box_message(sender, &recipient_keys, &plaintext)?;
    let envelope = PrivateBoxEnvelope {
        msg_type: PRIVATE_BOX_TYPE.to_string(),
        sender: sender.public_id(),
        boxed_payload: B64.encode(ciphertext),
        inner_schema_id: original_schema_id,
    };

    Ok(Some(PreparedPrivateBox {
        plaintext_content: content,
        encrypted_content: serde_json::to_value(envelope)?,
        schema_id: Some(PRIVATE_BOX_SCHEMA_ID.to_string()),
    }))
}

pub fn decrypt_for_identity(identity: &Identity, message: &Message) -> Result<Option<Message>> {
    let envelope = parse_envelope(&message.content)?;
    let ciphertext = B64
        .decode(envelope.boxed_payload)
        .map_err(|error| EgreError::Crypto {
            reason: format!("invalid private box payload: {error}"),
        })?;
    let sender_x25519 = envelope.sender.to_x25519_public_key()?;
    let Some(plaintext) = unbox_message(identity, &sender_x25519, &ciphertext)? else {
        return Ok(None);
    };

    let content = serde_json::from_slice(&plaintext)?;
    let mut decrypted = message.clone();
    decrypted.content = content;
    decrypted.schema_id = envelope.inner_schema_id;
    Ok(Some(decrypted))
}

pub fn is_private_box_content(content: &serde_json::Value) -> bool {
    content
        .get("type")
        .and_then(|value| value.as_str())
        .is_some_and(|value| value == PRIVATE_BOX_TYPE)
}

fn extract_recipients(content: &mut serde_json::Value) -> Result<Option<Vec<PublicId>>> {
    let Some(object) = content.as_object_mut() else {
        return Ok(None);
    };

    let Some(raw_recipients) = object.remove(ENCRYPT_FOR_FIELD) else {
        return Ok(None);
    };

    let recipients = raw_recipients
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .map(|value| PublicId(value.to_string()))
        .collect::<Vec<_>>();

    Ok(Some(recipients))
}

fn parse_envelope(content: &serde_json::Value) -> Result<PrivateBoxEnvelope> {
    Ok(serde_json::from_value(content.clone())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_for_publish_strips_encrypt_for_and_wraps_content() {
        let sender = Identity::generate();
        let recipient = Identity::generate();
        let plaintext = serde_json::json!({
            "type": "message",
            "text": "secret",
            "encrypt_for": [recipient.public_id().0],
        });

        let prepared = prepare_for_publish(&sender, plaintext, Some("message/v1".to_string()))
            .unwrap()
            .unwrap();

        assert_eq!(prepared.plaintext_content["type"], "message");
        assert!(prepared.plaintext_content.get("encrypt_for").is_none());
        assert_eq!(prepared.encrypted_content["type"], PRIVATE_BOX_TYPE);
        assert_eq!(prepared.schema_id.as_deref(), Some(PRIVATE_BOX_SCHEMA_ID));
    }

    #[test]
    fn recipient_can_decrypt_private_box_message() {
        let sender = Identity::generate();
        let recipient = Identity::generate();
        let prepared = prepare_for_publish(
            &sender,
            serde_json::json!({
                "type": "message",
                "text": "secret",
                "encrypt_for": [recipient.public_id().0],
            }),
            Some("message/v1".to_string()),
        )
        .unwrap()
        .unwrap();

        let message = Message {
            author: sender.public_id(),
            sequence: 1,
            previous: None,
            timestamp: chrono::Utc::now(),
            content: prepared.encrypted_content,
            schema_id: prepared.schema_id,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
            hash: "hash".to_string(),
            signature: "sig".to_string(),
        };

        let decrypted = decrypt_for_identity(&recipient, &message).unwrap().unwrap();
        assert_eq!(decrypted.content["text"], "secret");
        assert_eq!(decrypted.schema_id.as_deref(), Some("message/v1"));
    }

    #[test]
    fn outsider_cannot_decrypt_private_box_message() {
        let sender = Identity::generate();
        let recipient = Identity::generate();
        let outsider = Identity::generate();
        let prepared = prepare_for_publish(
            &sender,
            serde_json::json!({
                "type": "message",
                "text": "secret",
                "encrypt_for": [recipient.public_id().0],
            }),
            Some("message/v1".to_string()),
        )
        .unwrap()
        .unwrap();

        let message = Message {
            author: sender.public_id(),
            sequence: 1,
            previous: None,
            timestamp: chrono::Utc::now(),
            content: prepared.encrypted_content,
            schema_id: prepared.schema_id,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
            hash: "hash".to_string(),
            signature: "sig".to_string(),
        };

        assert!(decrypt_for_identity(&outsider, &message).unwrap().is_none());
    }
}
