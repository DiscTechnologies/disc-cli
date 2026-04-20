use std::borrow::Cow;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::select;
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async, tungstenite::client::IntoClientRequest, tungstenite::protocol::Message,
};

use crate::cli::{StreamOptions, WindowSemantics};

const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_millis(1_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubscriptionKind {
    Passive,
    Active,
}

impl SubscriptionKind {
    fn target_result_type(self) -> &'static str {
        match self {
            Self::Passive => "PASSIVE_SIGNAL_RESULT",
            Self::Active => "ACTIVE_SIGNAL_RESULT",
        }
    }

    fn target_status_type(self) -> &'static str {
        match self {
            Self::Passive => "PASSIVE_SIGNAL_STATUS",
            Self::Active => "ACTIVE_SIGNAL_STATUS",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubscriptionSpec {
    pub kind: SubscriptionKind,
    pub signal_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct SubscribePayload {
    #[serde(rename = "actionType")]
    action_type: &'static str,
    targets: Vec<SubscriptionTarget>,
}

#[derive(Debug, Clone, Serialize)]
struct SubscriptionTarget {
    #[serde(rename = "type")]
    target_type: Cow<'static, str>,
    #[serde(rename = "windowSemantics")]
    window_semantics: &'static str,
    #[serde(skip_serializing_if = "Option::is_none", rename = "passiveSignalId")]
    passive_signal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "activeSignalId")]
    active_signal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backfill: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "backfillFromEpochMs"
    )]
    backfill_from_epoch_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "backfillToEpochMs")]
    backfill_to_epoch_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "backfillCount")]
    backfill_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct CompactDataFrame {
    sk: String,
    sq: u64,
    k: String,
    at: u64,
    p: Value,
}

#[derive(Debug, Clone)]
pub enum InboundEvent {
    Data {
        stream_key: String,
        sequence: u64,
        payload_type: String,
        emitted_at_epoch_ms: u64,
        payload: Value,
    },
    Backfill {
        stream_key: String,
        items: Vec<Value>,
        meta: Option<Value>,
    },
    Control(Value),
}

impl InboundEvent {
    pub fn as_json(&self) -> Value {
        match self {
            Self::Data {
                stream_key,
                sequence,
                payload_type,
                emitted_at_epoch_ms,
                payload,
            } => json!({
                "type": "DATA",
                "streamKey": stream_key,
                "sequence": sequence,
                "payloadType": payload_type,
                "emittedAtEpochMs": emitted_at_epoch_ms,
                "payload": payload,
            }),
            Self::Backfill {
                stream_key,
                items,
                meta,
            } => json!({
                "type": "BACKFILL",
                "streamKey": stream_key,
                "items": items,
                "meta": meta,
            }),
            Self::Control(value) => value.clone(),
        }
    }

    pub fn pretty_line(&self) -> String {
        match self {
            Self::Data {
                stream_key,
                sequence,
                payload_type,
                payload,
                ..
            } => format!(
                "DATA {} seq={} kind={} payload={}",
                stream_key,
                sequence,
                payload_type,
                compact_json(payload)
            ),
            Self::Backfill {
                stream_key,
                items,
                meta,
            } => format!(
                "BACKFILL {} items={}{}",
                stream_key,
                items.len(),
                match meta {
                    Some(value) => format!(" meta={}", compact_json(value)),
                    None => String::new(),
                }
            ),
            Self::Control(value) => compact_json(value),
        }
    }

    pub fn is_data_event(&self) -> bool {
        matches!(self, Self::Data { .. } | Self::Backfill { .. })
    }

    pub fn is_control_event(&self) -> bool {
        matches!(self, Self::Control(_))
    }

    pub fn is_status_stream(&self) -> bool {
        match self {
            Self::Data { stream_key, .. } | Self::Backfill { stream_key, .. } => {
                stream_key.contains("_STATUS:")
            }
            Self::Control(value) => {
                if let Some(stream_key) = value.get("streamKey").and_then(Value::as_str) {
                    return stream_key.contains("_STATUS:");
                }
                false
            }
        }
    }
}

pub async fn run_subscription<F>(
    ws_url: &str,
    api_key: &str,
    client_id: Option<&str>,
    spec: &SubscriptionSpec,
    options: &StreamOptions,
    capture_ctrl_c: bool,
    mut on_event: F,
) -> Result<()>
where
    F: FnMut(InboundEvent) -> Result<bool>,
{
    let targets = build_targets(spec, options);
    let payload = SubscribePayload {
        action_type: "SUBSCRIBE",
        targets,
    };
    let encoded_payload = rmp_serde::to_vec_named(&payload)
        .context("Failed to encode websocket subscribe payload.")?;
    let timeout_duration = options.timeout;

    loop {
        let protocols = build_protocols(api_key, client_id);
        let mut request = ws_url
            .into_client_request()
            .context("Failed to build websocket request.")?;
        let protocol_header = protocols.join(",");
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            protocol_header
                .parse()
                .context("Failed to construct websocket auth protocols header.")?,
        );

        let connection_result = if capture_ctrl_c {
            select! {
                result = connect_async(request) => result,
                maybe_signal = tokio::signal::ctrl_c() => {
                    maybe_signal.context("Failed to wait for Ctrl+C.")?;
                    return Ok(());
                }
            }
        } else {
            connect_async(request).await
        };
        let (ws_stream, _) = connection_result
            .with_context(|| format!("Failed to connect to websocket at {ws_url}."))?;
        let (mut writer, mut reader) = ws_stream.split();

        writer
            .send(Message::Binary(encoded_payload.clone().into()))
            .await
            .context("Failed to send websocket subscribe payload.")?;

        loop {
            if capture_ctrl_c {
                select! {
                    _ = async {
                        if let Some(duration) = timeout_duration {
                            sleep(duration).await;
                        }
                    }, if timeout_duration.is_some() => {
                        return Ok(());
                    }
                    maybe_signal = tokio::signal::ctrl_c() => {
                        maybe_signal.context("Failed to wait for Ctrl+C.")?;
                        return Ok(());
                    }
                maybe_message = reader.next() => {
                        let (connection_closed, should_finish) = handle_next_message(maybe_message, &mut on_event)?;
                        if should_finish {
                            return Ok(());
                        }
                        if connection_closed {
                            break;
                        }
                    }
                }
            } else {
                select! {
                    _ = async {
                        if let Some(duration) = timeout_duration {
                            sleep(duration).await;
                        }
                    }, if timeout_duration.is_some() => {
                        return Ok(());
                    }
                maybe_message = reader.next() => {
                        let (connection_closed, should_finish) = handle_next_message(maybe_message, &mut on_event)?;
                        if should_finish {
                            return Ok(());
                        }
                        if connection_closed {
                            break;
                        }
                    }
                }
            }
        }

        if options.no_reconnect {
            return Ok(());
        }

        if capture_ctrl_c {
            select! {
                _ = sleep(DEFAULT_RECONNECT_DELAY) => {}
                maybe_signal = tokio::signal::ctrl_c() => {
                    maybe_signal.context("Failed to wait for Ctrl+C.")?;
                    return Ok(());
                }
            }
        } else {
            sleep(DEFAULT_RECONNECT_DELAY).await;
        }
    }
}

fn handle_next_message<F>(
    maybe_message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    on_event: &mut F,
) -> Result<(bool, bool)>
where
    F: FnMut(InboundEvent) -> Result<bool>,
{
    let message = match maybe_message {
        Some(message) => message.context("Websocket message read failed.")?,
        None => return Ok((true, false)),
    };

    let event = match decode_message(message)? {
        Some(event) => event,
        None => return Ok((false, false)),
    };
    Ok((false, on_event(event)?))
}

fn build_targets(spec: &SubscriptionSpec, options: &StreamOptions) -> Vec<SubscriptionTarget> {
    let mut targets = vec![build_target(spec.kind.target_result_type(), spec, options)];

    if options.include_status {
        targets.push(build_target(spec.kind.target_status_type(), spec, options));
    }

    targets
}

fn build_target(
    target_type: &'static str,
    spec: &SubscriptionSpec,
    options: &StreamOptions,
) -> SubscriptionTarget {
    let passive_signal_id = if matches!(spec.kind, SubscriptionKind::Passive) {
        Some(spec.signal_id.clone())
    } else {
        None
    };
    let active_signal_id = if matches!(spec.kind, SubscriptionKind::Active) {
        Some(spec.signal_id.clone())
    } else {
        None
    };

    SubscriptionTarget {
        target_type: Cow::Borrowed(target_type),
        window_semantics: match options.window_semantics {
            WindowSemantics::Elapsed => "elapsed",
            WindowSemantics::Ordinal => "ordinal",
        },
        passive_signal_id,
        active_signal_id,
        backfill: if options.backfill { Some(true) } else { None },
        backfill_from_epoch_ms: options.backfill_from,
        backfill_to_epoch_ms: options.backfill_to,
        backfill_count: options.backfill_count,
    }
}

fn build_protocols(api_key: &str, client_id: Option<&str>) -> Vec<String> {
    let mut protocols = vec![format!("apiKey-{api_key}")];
    if let Some(value) = client_id {
        protocols.push(format!("clientId-{value}"));
    }
    protocols
}

fn decode_message(message: Message) -> Result<Option<InboundEvent>> {
    match message {
        Message::Binary(bytes) => {
            let decoded: Value = rmp_serde::from_slice(bytes.as_ref())
                .context("Failed to decode MessagePack websocket frame.")?;
            decode_value(decoded)
        }
        Message::Text(text) => {
            let parsed: Value = serde_json::from_str(&text)
                .context("Failed to decode text websocket frame as JSON.")?;
            decode_value(parsed)
        }
        Message::Ping(_) | Message::Pong(_) => Ok(None),
        Message::Close(_) => Ok(None),
        Message::Frame(_) => Ok(None),
    }
}

fn decode_value(value: Value) -> Result<Option<InboundEvent>> {
    if let Ok(frame) = serde_json::from_value::<CompactDataFrame>(value.clone()) {
        return Ok(Some(InboundEvent::Data {
            stream_key: frame.sk,
            sequence: frame.sq,
            payload_type: frame.k,
            emitted_at_epoch_ms: frame.at,
            payload: frame.p,
        }));
    }

    let object = match value.as_object() {
        Some(object) => object,
        None => return Ok(None),
    };

    let event_type = match object.get("type").and_then(Value::as_str) {
        Some(value) => value,
        None => return Ok(None),
    };

    if event_type == "DATA" {
        return Ok(Some(InboundEvent::Data {
            stream_key: required_string(object, "streamKey")?.to_owned(),
            sequence: required_u64(object, "sequence")?,
            payload_type: required_string(object, "payloadType")?.to_owned(),
            emitted_at_epoch_ms: required_u64(object, "emittedAtEpochMs")?,
            payload: object.get("payload").cloned().unwrap_or(Value::Null),
        }));
    }

    if event_type == "BACKFILL" {
        let items = object
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let meta = object.get("meta").cloned();
        return Ok(Some(InboundEvent::Backfill {
            stream_key: required_string(object, "streamKey")?.to_owned(),
            items,
            meta,
        }));
    }

    if event_type == "ERROR" {
        let message = object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown server error");
        let code = object
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(anyhow::anyhow!("[{code}] {message}"));
    }

    Ok(Some(InboundEvent::Control(value)))
}

fn required_string<'a>(object: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Expected `{key}` to be a string in websocket frame."))
}

fn required_u64(object: &serde_json::Map<String, Value>, key: &str) -> Result<u64> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("Expected `{key}` to be an unsigned integer in websocket frame."))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<invalid json>".to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{InboundEvent, decode_value};

    #[test]
    fn decode_compact_data_frame_maps_to_data_event() {
        let event = decode_value(json!({
            "sk": "PASSIVE_SIGNAL:123:ordinal",
            "sq": 7,
            "k": "psr",
            "at": 123456,
            "p": { "value": 10 }
        }))
        .expect("decode ok")
        .expect("event");

        match event {
            InboundEvent::Data {
                stream_key,
                sequence,
                payload_type,
                ..
            } => {
                assert_eq!(stream_key, "PASSIVE_SIGNAL:123:ordinal");
                assert_eq!(sequence, 7);
                assert_eq!(payload_type, "psr");
            }
            _ => panic!("expected data event"),
        }
    }
}
