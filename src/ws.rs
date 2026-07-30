use std::borrow::Cow;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::select;
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async, tungstenite::client::IntoClientRequest, tungstenite::protocol::Message,
};

use crate::cli::{StreamOptions, WindowSemantics};
use crate::config::StoredAuthProfile;

const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_millis(1_000);

#[derive(Debug, Clone)]
pub enum WsCredential {
    ApiKey(String),
    Ticket(String),
    OAuth { profile: Box<StoredAuthProfile> },
}

impl From<&str> for WsCredential {
    fn from(value: &str) -> Self {
        Self::ApiKey(value.to_owned())
    }
}

impl From<&WsCredential> for WsCredential {
    fn from(value: &WsCredential) -> Self {
        value.clone()
    }
}

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

pub async fn run_subscription<F, C>(
    ws_url: &str,
    credential: C,
    client_id: Option<&str>,
    spec: &SubscriptionSpec,
    options: &StreamOptions,
    capture_ctrl_c: bool,
    mut on_event: F,
) -> Result<()>
where
    F: FnMut(InboundEvent) -> Result<bool>,
    C: Into<WsCredential>,
{
    let credential = credential.into();
    let targets = build_targets(spec, options);
    let payload = SubscribePayload {
        action_type: "SUBSCRIBE",
        targets,
    };
    let encoded_payload = rmp_serde::to_vec_named(&payload)
        .context("Failed to encode websocket subscribe payload.")?;
    let timeout_duration = options.timeout;

    loop {
        let connection_credential = resolve_connection_credential(&credential).await?;
        let protocols = build_protocols(&connection_credential, client_id);
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
        let (ws_stream, _) =
            connection_result.context(format!("Failed to connect to websocket at {ws_url}."))?;
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

fn build_protocols(credential: &WsCredential, client_id: Option<&str>) -> Vec<String> {
    let mut protocols = match credential {
        WsCredential::ApiKey(api_key) => vec![format!("apiKey-{api_key}")],
        WsCredential::Ticket(ticket) => vec![format!("sessionId-ticket.{ticket}")],
        WsCredential::OAuth { .. } => {
            unreachable!("OAuth credentials are exchanged for a WebSocket ticket")
        }
    };
    if let Some(value) = client_id {
        protocols.push(format!("clientId-{value}"));
    }
    protocols
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebSocketTicketResponse {
    ticket: String,
    expires_at: String,
}

async fn resolve_connection_credential(credential: &WsCredential) -> Result<WsCredential> {
    match credential {
        WsCredential::ApiKey(value) => Ok(WsCredential::ApiKey(value.clone())),
        WsCredential::Ticket(_) => bail!("WebSocket tickets cannot be reused."),
        WsCredential::OAuth { profile } => {
            let oauth = crate::auth_login::runtime_oauth(profile).await?;
            let response = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .context("Failed to build WebSocket ticket client.")?
                .post(format!(
                    "{}/auth/websocket-ticket",
                    profile.api_base_url.trim_end_matches('/')
                ))
                .bearer_auth(oauth.access_token.expose_secret())
                .header(
                    "X-Disc-Subject-Context",
                    oauth.subject_context_token.expose_secret(),
                )
                .send()
                .await
                .context("Failed to request a WebSocket ticket.")?
                .error_for_status()
                .context("Disc rejected the WebSocket ticket request.")?;
            let ticket = response
                .json::<WebSocketTicketResponse>()
                .await
                .context("Disc returned an invalid WebSocket ticket.")?;
            if ticket.ticket.trim().is_empty()
                || chrono::DateTime::parse_from_rfc3339(&ticket.expires_at).is_err()
            {
                bail!("Disc returned an invalid WebSocket ticket.");
            }
            Ok(WsCredential::Ticket(ticket.ticket))
        }
    }
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
    object.get(key).and_then(Value::as_str).context(format!(
        "Expected `{key}` to be a string in websocket frame."
    ))
}

fn required_u64(object: &serde_json::Map<String, Value>, key: &str) -> Result<u64> {
    object.get(key).and_then(Value::as_u64).context(format!(
        "Expected `{key}` to be an unsigned integer in websocket frame."
    ))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<invalid json>".to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};

    use crate::cli::{StreamOptions, StreamOutputFilter, WindowSemantics};

    use super::{
        InboundEvent, SubscriptionKind, SubscriptionSpec, WsCredential, build_protocols,
        build_targets, decode_message, decode_value, handle_next_message,
        resolve_connection_credential, run_subscription,
    };

    fn stream_options() -> StreamOptions {
        StreamOptions {
            output: StreamOutputFilter::Data,
            window_semantics: WindowSemantics::Ordinal,
            backfill: false,
            backfill_from: None,
            backfill_to: None,
            backfill_count: None,
            include_status: false,
            once: false,
            timeout: None,
            no_reconnect: false,
        }
    }

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

    #[test]
    fn inbound_events_render_json_pretty_lines_and_classification() {
        let data = InboundEvent::Data {
            stream_key: "PASSIVE_SIGNAL:123:ordinal".to_owned(),
            sequence: 7,
            payload_type: "psr".to_owned(),
            emitted_at_epoch_ms: 123_456,
            payload: json!({"value": 10}),
        };
        assert_eq!(
            data.as_json(),
            json!({
                "type": "DATA",
                "streamKey": "PASSIVE_SIGNAL:123:ordinal",
                "sequence": 7,
                "payloadType": "psr",
                "emittedAtEpochMs": 123456,
                "payload": {"value": 10}
            })
        );
        assert_eq!(
            data.pretty_line(),
            r#"DATA PASSIVE_SIGNAL:123:ordinal seq=7 kind=psr payload={"value":10}"#
        );
        assert!(data.is_data_event());
        assert!(!data.is_control_event());
        assert!(!data.is_status_stream());

        let backfill = InboundEvent::Backfill {
            stream_key: "ACTIVE_SIGNAL_STATUS:456:elapsed".to_owned(),
            items: vec![json!({"value": 1}), json!({"value": 2})],
            meta: Some(json!({"cursor": "next"})),
        };
        assert_eq!(
            backfill.pretty_line(),
            r#"BACKFILL ACTIVE_SIGNAL_STATUS:456:elapsed items=2 meta={"cursor":"next"}"#
        );
        assert!(backfill.is_data_event());
        assert!(backfill.is_status_stream());
        assert_eq!(
            backfill.as_json(),
            json!({
                "type": "BACKFILL",
                "streamKey": "ACTIVE_SIGNAL_STATUS:456:elapsed",
                "items": [{"value": 1}, {"value": 2}],
                "meta": {"cursor": "next"}
            })
        );

        let control = InboundEvent::Control(json!({
            "type": "SUBSCRIBED",
            "streamKey": "PASSIVE_SIGNAL_STATUS:123:ordinal"
        }));
        assert!(control.is_control_event());
        assert!(!control.is_data_event());
        assert!(control.is_status_stream());
        assert_eq!(
            control.pretty_line(),
            r#"{"streamKey":"PASSIVE_SIGNAL_STATUS:123:ordinal","type":"SUBSCRIBED"}"#
        );

        let no_meta = InboundEvent::Backfill {
            stream_key: "PASSIVE_SIGNAL:123:ordinal".to_owned(),
            items: Vec::new(),
            meta: None,
        };
        assert_eq!(
            no_meta.pretty_line(),
            "BACKFILL PASSIVE_SIGNAL:123:ordinal items=0"
        );
    }

    #[test]
    fn target_building_covers_signal_kinds_status_and_backfill_options() {
        let mut options = stream_options();
        options.window_semantics = WindowSemantics::Elapsed;
        options.backfill = true;
        options.backfill_from = Some(100);
        options.backfill_to = Some(200);
        options.backfill_count = Some(25);
        options.include_status = true;

        let passive = build_targets(
            &SubscriptionSpec {
                kind: SubscriptionKind::Passive,
                signal_id: "passive-id".to_owned(),
            },
            &options,
        );
        assert_eq!(passive.len(), 2);
        assert_eq!(passive[0].target_type, "PASSIVE_SIGNAL_RESULT");
        assert_eq!(passive[1].target_type, "PASSIVE_SIGNAL_STATUS");
        assert_eq!(passive[0].window_semantics, "elapsed");
        assert_eq!(passive[0].passive_signal_id.as_deref(), Some("passive-id"));
        assert!(passive[0].active_signal_id.is_none());
        assert_eq!(passive[0].backfill, Some(true));
        assert_eq!(passive[0].backfill_from_epoch_ms, Some(100));
        assert_eq!(passive[0].backfill_to_epoch_ms, Some(200));
        assert_eq!(passive[0].backfill_count, Some(25));

        let active = build_targets(
            &SubscriptionSpec {
                kind: SubscriptionKind::Active,
                signal_id: "active-id".to_owned(),
            },
            &stream_options(),
        );
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].target_type, "ACTIVE_SIGNAL_RESULT");
        assert_eq!(active[0].window_semantics, "ordinal");
        assert!(active[0].passive_signal_id.is_none());
        assert_eq!(active[0].active_signal_id.as_deref(), Some("active-id"));
        assert_eq!(active[0].backfill, None);
    }

    #[test]
    fn websocket_protocols_include_optional_client_identity() {
        assert_eq!(
            build_protocols(&WsCredential::ApiKey("secret".to_owned()), None),
            vec!["apiKey-secret"]
        );
        assert_eq!(
            build_protocols(&WsCredential::ApiKey("secret".to_owned()), Some("client")),
            vec!["apiKey-secret", "clientId-client"]
        );
        assert_eq!(
            build_protocols(&WsCredential::Ticket("one-time".to_owned()), Some("client")),
            vec!["sessionId-ticket.one-time", "clientId-client"]
        );
    }

    #[test]
    fn full_data_backfill_control_and_error_frames_decode() {
        let data = decode_value(json!({
            "type": "DATA",
            "streamKey": "stream",
            "sequence": 9,
            "payloadType": "result",
            "emittedAtEpochMs": 500,
            "payload": {"value": 12}
        }))
        .expect("decode data")
        .expect("data event");
        assert_eq!(data.as_json()["sequence"], 9);

        let data_without_payload = decode_value(json!({
            "type": "DATA",
            "streamKey": "stream",
            "sequence": 10,
            "payloadType": "result",
            "emittedAtEpochMs": 501
        }))
        .expect("decode data without payload")
        .expect("data event");
        assert!(data_without_payload.as_json()["payload"].is_null());

        let backfill = decode_value(json!({
            "type": "BACKFILL",
            "streamKey": "stream",
            "items": [{"value": 1}],
            "meta": {"count": 1}
        }))
        .expect("decode backfill")
        .expect("backfill event");
        assert_eq!(backfill.as_json()["items"][0]["value"], 1);

        let empty_backfill = decode_value(json!({
            "type": "BACKFILL",
            "streamKey": "stream",
            "items": "invalid"
        }))
        .expect("decode empty backfill")
        .expect("backfill event");
        assert_eq!(empty_backfill.as_json()["items"], json!([]));

        let control = decode_value(json!({"type": "SUBSCRIBED", "requestId": "one"}))
            .expect("decode control")
            .expect("control event");
        assert_eq!(control.as_json()["requestId"], "one");

        let error = decode_value(json!({"type": "ERROR", "code": "DENIED", "message": "no"}))
            .expect_err("error frame");
        assert_eq!(error.to_string(), "[DENIED] no");
        let default_error =
            decode_value(json!({"type": "ERROR"})).expect_err("default error frame");
        assert_eq!(default_error.to_string(), "[unknown] unknown server error");

        assert!(decode_value(json!(42)).expect("scalar ignored").is_none());
        assert!(
            decode_value(json!({"missing": "type"}))
                .expect("untyped ignored")
                .is_none()
        );
    }

    #[test]
    fn malformed_required_data_fields_report_the_exact_contract() {
        for (frame, expected) in [
            (
                json!({
                    "type": "DATA",
                    "streamKey": 42,
                    "sequence": 1,
                    "payloadType": "result",
                    "emittedAtEpochMs": 1
                }),
                "Expected `streamKey` to be a string",
            ),
            (
                json!({
                    "type": "DATA",
                    "streamKey": "stream",
                    "sequence": -1,
                    "payloadType": "result",
                    "emittedAtEpochMs": 1
                }),
                "Expected `sequence` to be an unsigned integer",
            ),
            (
                json!({
                    "type": "DATA",
                    "streamKey": "stream",
                    "sequence": 1,
                    "payloadType": 42,
                    "emittedAtEpochMs": 1
                }),
                "Expected `payloadType` to be a string",
            ),
            (
                json!({
                    "type": "DATA",
                    "streamKey": "stream",
                    "sequence": 1,
                    "payloadType": "result"
                }),
                "Expected `emittedAtEpochMs` to be an unsigned integer",
            ),
        ] {
            let error = decode_value(frame).expect_err("invalid data frame");
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn websocket_message_encodings_and_non_data_frames_are_handled() {
        let text = Message::Text(
            json!({
                "type": "DATA",
                "streamKey": "text-stream",
                "sequence": 1,
                "payloadType": "result",
                "emittedAtEpochMs": 2
            })
            .to_string()
            .into(),
        );
        assert!(decode_message(text).expect("decode text").is_some());

        let binary_value = json!({"type": "SUBSCRIBED"});
        let binary = Message::Binary(
            rmp_serde::to_vec_named(&binary_value)
                .expect("encode MessagePack")
                .into(),
        );
        assert!(decode_message(binary).expect("decode binary").is_some());

        assert!(
            decode_message(Message::Ping(Vec::new().into()))
                .expect("ping ignored")
                .is_none()
        );
        assert!(
            decode_message(Message::Pong(Vec::new().into()))
                .expect("pong ignored")
                .is_none()
        );
        assert!(
            decode_message(Message::Close(None))
                .expect("close ignored")
                .is_none()
        );
        assert!(decode_message(Message::Text("not-json".into())).is_err());
        assert!(decode_message(Message::Binary(vec![0xc1].into())).is_err());
    }

    #[test]
    fn next_message_handler_distinguishes_close_ignore_finish_and_errors() {
        let seen = std::cell::RefCell::new(Vec::new());
        let mut handler = |event: InboundEvent| {
            seen.borrow_mut().push(event.as_json());
            Ok(false)
        };
        assert_eq!(
            handle_next_message(None, &mut handler).expect("closed stream"),
            (true, false)
        );
        assert_eq!(
            handle_next_message(Some(Ok(Message::Ping(Vec::new().into()))), &mut handler)
                .expect("ignored ping"),
            (false, false)
        );
        assert!(seen.borrow().is_empty());

        let message = Message::Text(json!({"type": "SUBSCRIBED"}).to_string().into());
        assert_eq!(
            handle_next_message(Some(Ok(message)), &mut handler).expect("control message"),
            (false, false)
        );
        assert_eq!(seen.borrow().len(), 1);

        let mut finish = |_event: InboundEvent| Ok(true);
        let message = Message::Text(json!({"type": "SUBSCRIBED"}).to_string().into());
        assert_eq!(
            handle_next_message(Some(Ok(message)), &mut finish).expect("finish message"),
            (false, true)
        );

        let mut ignore = |_event: InboundEvent| Ok(false);
        let error = handle_next_message(Some(Err(WebSocketError::ConnectionClosed)), &mut ignore)
            .expect_err("websocket read failure");
        assert!(error.to_string().contains("Websocket message read failed"));

        let mut fail = |_event: InboundEvent| anyhow::bail!("callback failed");
        let message = Message::Text(json!({"type": "SUBSCRIBED"}).to_string().into());
        let error =
            handle_next_message(Some(Ok(message)), &mut fail).expect_err("callback failure");
        assert_eq!(error.to_string(), "callback failed");
    }

    #[test]
    fn stream_options_fixture_exercises_timeout_field() {
        let mut options = stream_options();
        options.timeout = Some(Duration::from_secs(5));
        assert_eq!(options.timeout, Some(Duration::from_secs(5)));
    }

    #[tokio::test]
    async fn subscription_sends_auth_and_targets_then_delivers_data_to_callback() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind WebSocket listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let mut socket =
                accept_hdr_async(stream, |request: &Request, mut response: Response| {
                    let protocols = request
                        .headers()
                        .get("sec-websocket-protocol")
                        .expect("protocol header")
                        .to_str()
                        .expect("protocol text");
                    assert_eq!(protocols, "apiKey-test-key,clientId-client-one");
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        "apiKey-test-key".parse().expect("protocol response"),
                    );
                    Ok(response)
                })
                .await
                .expect("accept WebSocket");

            let subscribe = socket
                .next()
                .await
                .expect("subscribe frame")
                .expect("valid subscribe frame");
            let Message::Binary(payload) = subscribe else {
                panic!("expected binary subscribe payload");
            };
            let value: serde_json::Value =
                rmp_serde::from_slice(&payload).expect("decode subscribe payload");
            assert_eq!(value["actionType"], "SUBSCRIBE");
            assert_eq!(value["targets"].as_array().expect("targets").len(), 2);
            assert_eq!(value["targets"][0]["passiveSignalId"], "passive-one");
            assert_eq!(value["targets"][0]["windowSemantics"], "elapsed");
            assert_eq!(value["targets"][0]["backfill"], true);
            assert_eq!(value["targets"][0]["backfillFromEpochMs"], 100);
            assert_eq!(value["targets"][0]["backfillToEpochMs"], 200);
            assert_eq!(value["targets"][0]["backfillCount"], 25);

            socket
                .send(Message::Text(
                    json!({
                        "type": "DATA",
                        "streamKey": "PASSIVE_SIGNAL:passive-one:elapsed",
                        "sequence": 3,
                        "payloadType": "result",
                        "emittedAtEpochMs": 500,
                        "payload": {"value": 99}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("send data");
        });

        let mut options = stream_options();
        options.window_semantics = WindowSemantics::Elapsed;
        options.include_status = true;
        options.backfill = true;
        options.backfill_from = Some(100);
        options.backfill_to = Some(200);
        options.backfill_count = Some(25);
        let spec = SubscriptionSpec {
            kind: SubscriptionKind::Passive,
            signal_id: "passive-one".to_owned(),
        };
        let mut seen = Vec::new();

        run_subscription(
            &format!("ws://{address}"),
            "test-key",
            Some("client-one"),
            &spec,
            &options,
            false,
            |event| {
                seen.push(event.as_json());
                Ok(true)
            },
        )
        .await
        .expect("subscription");
        server.await.expect("server task");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["payload"]["value"], 99);
    }

    #[tokio::test]
    async fn subscription_timeout_returns_without_waiting_for_server_close() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind WebSocket listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let mut socket =
                accept_hdr_async(stream, |_request: &Request, mut response: Response| {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        "apiKey-test-key".parse().expect("protocol response"),
                    );
                    Ok(response)
                })
                .await
                .expect("accept WebSocket");
            socket
                .next()
                .await
                .expect("subscribe frame")
                .expect("frame");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let mut options = stream_options();
        options.timeout = Some(Duration::from_millis(5));
        let spec = SubscriptionSpec {
            kind: SubscriptionKind::Active,
            signal_id: "active-one".to_owned(),
        };

        run_subscription(
            &format!("ws://{address}"),
            "test-key",
            None,
            &spec,
            &options,
            false,
            |_event| Ok(false),
        )
        .await
        .expect("timed subscription");
        server.abort();
    }

    #[tokio::test]
    async fn subscription_honours_no_reconnect_after_clean_server_close() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind WebSocket listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let mut socket =
                accept_hdr_async(stream, |_request: &Request, mut response: Response| {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        "apiKey-test-key".parse().expect("protocol response"),
                    );
                    Ok(response)
                })
                .await
                .expect("accept WebSocket");
            socket
                .next()
                .await
                .expect("subscribe frame")
                .expect("frame");
            socket.close(None).await.expect("close WebSocket");
        });
        let mut options = stream_options();
        options.no_reconnect = true;
        let spec = SubscriptionSpec {
            kind: SubscriptionKind::Passive,
            signal_id: "passive-one".to_owned(),
        };

        run_subscription(
            &format!("ws://{address}"),
            "test-key",
            None,
            &spec,
            &options,
            false,
            |_event| Ok(false),
        )
        .await
        .expect("cleanly closed subscription");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn subscription_reports_connection_and_request_construction_failures() {
        let spec = SubscriptionSpec {
            kind: SubscriptionKind::Passive,
            signal_id: "passive-one".to_owned(),
        };
        let options = stream_options();
        let error = run_subscription(
            "not a websocket URL",
            "test-key",
            None,
            &spec,
            &options,
            false,
            |_event| Ok(false),
        )
        .await
        .expect_err("invalid URL");
        assert!(
            error
                .to_string()
                .contains("Failed to build websocket request")
        );

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve loopback address");
        let address = listener.local_addr().expect("listener address");
        drop(listener);
        let error = run_subscription(
            &format!("ws://{address}"),
            "test-key",
            None,
            &spec,
            &options,
            false,
            |_event| Ok(false),
        )
        .await
        .expect_err("connection refusal");
        assert!(error.to_string().contains("Failed to connect to websocket"));

        assert!(matches!(
            resolve_connection_credential(&WsCredential::ApiKey("test-key".to_owned()))
                .await
                .expect("API key connection credential"),
            WsCredential::ApiKey(value) if value == "test-key"
        ));
        assert!(
            resolve_connection_credential(&WsCredential::Ticket("spent".to_owned()))
                .await
                .expect_err("ticket reuse")
                .to_string()
                .contains("cannot be reused")
        );
    }
}
