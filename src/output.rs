use std::fs::OpenOptions;
use std::io::{Write, stdout};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use comfy_table::{Cell, ContentArrangement, Table, presets::UTF8_FULL};
use serde_json::{Value, json};

use crate::cli::{JsonOutputFormat, ListOutputFormat, StreamOutputFilter, StreamOutputFormat};
use crate::http::ValidateResponse;
use crate::ws::InboundEvent;

pub type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub fn create_stdout_writer() -> SharedWriter {
    Arc::new(Mutex::new(Box::new(stdout())))
}

pub fn create_file_writer(path: &Path) -> Result<SharedWriter> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context(format!(
            "Failed to open destination file at {}.",
            path.display()
        ))?;
    Ok(Arc::new(Mutex::new(Box::new(file))))
}

pub fn print_json_value(value: &Value, format: JsonOutputFormat) -> Result<()> {
    let line = match format {
        JsonOutputFormat::Json => {
            serde_json::to_string_pretty(value).context("Failed to render JSON output.")?
        }
        JsonOutputFormat::Ndjson => {
            serde_json::to_string(value).context("Failed to render NDJSON output.")?
        }
    };

    println!("{line}");
    Ok(())
}

pub fn print_signal_list(values: &[Value], format: ListOutputFormat) -> Result<()> {
    match format {
        ListOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(values).context("Failed to render JSON output.")?
            );
        }
        ListOutputFormat::Ndjson => {
            for value in values {
                println!(
                    "{}",
                    serde_json::to_string(value).context("Failed to render NDJSON output.")?
                );
            }
        }
        ListOutputFormat::Table => {
            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_content_arrangement(ContentArrangement::Dynamic);
            table.set_header(vec![
                Cell::new("ID"),
                Cell::new("Label"),
                Cell::new("Status"),
            ]);

            for value in values {
                let id = select_signal_id(value);
                let label = string_field(value, &["label"]);
                let status = resolve_signal_status(value);

                table.add_row(vec![
                    Cell::new(id.unwrap_or("-")),
                    Cell::new(label.unwrap_or("-")),
                    Cell::new(status),
                ]);
            }

            println!("{table}");
        }
    }

    Ok(())
}

pub fn should_emit_event(event: &InboundEvent, output_filter: StreamOutputFilter) -> bool {
    match output_filter {
        StreamOutputFilter::All => true,
        StreamOutputFilter::Events => event.is_control_event(),
        StreamOutputFilter::Data => event.is_data_event() && !event.is_status_stream(),
        StreamOutputFilter::Status => event.is_status_stream(),
    }
}

pub fn write_subscription_event(
    writer: &SharedWriter,
    event: &InboundEvent,
    format: StreamOutputFormat,
) -> Result<()> {
    let rendered = match format {
        StreamOutputFormat::Pretty => event.pretty_line(),
        StreamOutputFormat::Json => serde_json::to_string_pretty(&event.as_json())
            .context("Failed to render JSON subscription event.")?,
        StreamOutputFormat::Ndjson => serde_json::to_string(&event.as_json())
            .context("Failed to render NDJSON subscription event.")?,
    };

    let mut writer = writer
        .lock()
        .map_err(|_| anyhow::anyhow!("Failed to lock output writer."))?;
    writer
        .write_all(rendered.as_bytes())
        .context("Failed to write subscription event.")?;
    writer
        .write_all(b"\n")
        .context("Failed to finalize output line.")?;
    writer.flush().context("Failed to flush output writer.")?;
    Ok(())
}

fn select_signal_id(value: &Value) -> Option<&str> {
    if let Some(active_signal_id) = value.get("activeSignalId").and_then(Value::as_str) {
        return Some(active_signal_id);
    }

    if let Some(passive_signal_id) = value.get("passiveSignalId").and_then(Value::as_str) {
        return Some(passive_signal_id);
    }

    None
}

fn string_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        if let Some(result) = value.get(*key).and_then(Value::as_str) {
            return Some(result);
        }
    }

    None
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn resolve_signal_status(value: &Value) -> &'static str {
    let is_paused = bool_field(value, "isPaused").unwrap_or(false);
    if is_paused {
        return "inactive";
    }

    match value.get("status").and_then(Value::as_str) {
        Some(raw_status) if raw_status.eq_ignore_ascii_case("blocked") => "blocked",
        Some(raw_status) if raw_status.eq_ignore_ascii_case("inactive") => "inactive",
        Some(raw_status) if raw_status.eq_ignore_ascii_case("active") => "active",
        Some(raw_status) if raw_status.eq_ignore_ascii_case("running") => "active",
        Some(_) => "active",
        None => "active",
    }
}

pub fn validate_to_json(value: &ValidateResponse) -> Value {
    json!({
        "authType": value.auth_type,
        "authTokenId": value.auth_token_id,
        "sessionId": value.session_id,
        "apiKeyId": value.api_key_id,
        "userId": value.user_id,
        "userType": value.user_type,
        "expiresAt": value.expires_at,
        "revalidateAt": value.revalidate_at,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use crate::cli::{StreamOutputFilter, StreamOutputFormat};
    use crate::http::ValidateResponse;
    use crate::ws::InboundEvent;

    use super::{
        SharedWriter, create_file_writer, resolve_signal_status, select_signal_id,
        should_emit_event, string_field, validate_to_json, write_subscription_event,
    };

    #[derive(Clone)]
    struct CapturingWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        should_fail: bool,
    }

    impl Write for CapturingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.should_fail {
                return Err(io::Error::other("intentional write failure"));
            }
            self.bytes
                .lock()
                .expect("capture buffer lock")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.should_fail {
                return Err(io::Error::other("intentional flush failure"));
            }
            Ok(())
        }
    }

    fn capturing_writer() -> (SharedWriter, Arc<Mutex<Vec<u8>>>) {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = CapturingWriter {
            bytes: bytes.clone(),
            should_fail: false,
        };
        (Arc::new(Mutex::new(Box::new(writer))), bytes)
    }

    fn data_event(stream_key: &str) -> InboundEvent {
        InboundEvent::Data {
            stream_key: stream_key.to_owned(),
            sequence: 2,
            payload_type: "result".to_owned(),
            emitted_at_epoch_ms: 10,
            payload: json!({"value": 42}),
        }
    }

    fn temporary_file(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "disc-cli-output-{test_name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn stream_filters_distinguish_data_status_and_control_events() {
        let data = data_event("PASSIVE_SIGNAL:one:ordinal");
        let status = data_event("PASSIVE_SIGNAL_STATUS:one:ordinal");
        let control = InboundEvent::Control(json!({"type": "SUBSCRIBED"}));

        for event in [&data, &status, &control] {
            assert!(should_emit_event(event, StreamOutputFilter::All));
        }
        assert!(should_emit_event(&data, StreamOutputFilter::Data));
        assert!(!should_emit_event(&status, StreamOutputFilter::Data));
        assert!(!should_emit_event(&control, StreamOutputFilter::Data));
        assert!(!should_emit_event(&data, StreamOutputFilter::Status));
        assert!(should_emit_event(&status, StreamOutputFilter::Status));
        assert!(!should_emit_event(&control, StreamOutputFilter::Status));
        assert!(!should_emit_event(&data, StreamOutputFilter::Events));
        assert!(should_emit_event(&control, StreamOutputFilter::Events));
    }

    #[test]
    fn subscription_events_render_every_supported_format() {
        let event = data_event("PASSIVE_SIGNAL:one:ordinal");

        let (writer, bytes) = capturing_writer();
        write_subscription_event(&writer, &event, StreamOutputFormat::Pretty)
            .expect("write pretty event");
        assert_eq!(
            String::from_utf8(bytes.lock().expect("buffer").clone()).expect("UTF-8"),
            "DATA PASSIVE_SIGNAL:one:ordinal seq=2 kind=result payload={\"value\":42}\n"
        );

        let (writer, bytes) = capturing_writer();
        write_subscription_event(&writer, &event, StreamOutputFormat::Json)
            .expect("write JSON event");
        let rendered =
            String::from_utf8(bytes.lock().expect("buffer").clone()).expect("UTF-8 JSON");
        assert!(rendered.contains("\n  \"type\": \"DATA\""));
        assert!(rendered.ends_with('\n'));

        let (writer, bytes) = capturing_writer();
        write_subscription_event(&writer, &event, StreamOutputFormat::Ndjson)
            .expect("write NDJSON event");
        let rendered =
            String::from_utf8(bytes.lock().expect("buffer").clone()).expect("UTF-8 NDJSON");
        assert_eq!(rendered.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(rendered.trim()).expect("valid NDJSON"),
            event.as_json()
        );
    }

    #[test]
    fn subscription_event_reports_poisoned_lock_and_write_failures() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let failing_writer: SharedWriter = Arc::new(Mutex::new(Box::new(CapturingWriter {
            bytes,
            should_fail: true,
        })));
        let error = write_subscription_event(
            &failing_writer,
            &data_event("stream"),
            StreamOutputFormat::Ndjson,
        )
        .expect_err("write failure");
        assert!(
            error
                .to_string()
                .contains("Failed to write subscription event")
        );

        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(io::sink())));
        let poison_target = writer.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison_target.lock().expect("writer lock");
            panic!("poison writer");
        })
        .join();
        let error =
            write_subscription_event(&writer, &data_event("stream"), StreamOutputFormat::Pretty)
                .expect_err("poisoned lock");
        assert_eq!(error.to_string(), "Failed to lock output writer.");
    }

    #[test]
    fn file_writer_creates_and_appends_destination_lines() {
        let path = temporary_file("append");
        let event = data_event("PASSIVE_SIGNAL:file:ordinal");

        let writer = create_file_writer(&path).expect("create file writer");
        write_subscription_event(&writer, &event, StreamOutputFormat::Ndjson)
            .expect("write first line");
        drop(writer);

        let writer = create_file_writer(&path).expect("reopen file writer");
        write_subscription_event(&writer, &event, StreamOutputFormat::Ndjson)
            .expect("append second line");
        drop(writer);

        let contents = fs::read_to_string(&path).expect("read output file");
        assert_eq!(contents.lines().count(), 2);
        fs::remove_file(path).expect("remove output file");
    }

    #[test]
    fn file_writer_reports_an_invalid_destination() {
        let path = temporary_file("directory");
        fs::create_dir_all(&path).expect("create destination directory");
        let error = match create_file_writer(&path) {
            Ok(_) => panic!("directory must not be accepted as a destination file"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("Failed to open destination file")
        );
        fs::remove_dir(path).expect("remove directory");
    }

    #[test]
    fn signal_identity_and_status_resolution_follow_api_contract() {
        let active = json!({
            "activeSignalId": "active-one",
            "passiveSignalId": "passive-one",
            "label": "Active",
            "status": "RUNNING"
        });
        assert_eq!(select_signal_id(&active), Some("active-one"));
        assert_eq!(string_field(&active, &["missing", "label"]), Some("Active"));
        assert_eq!(resolve_signal_status(&active), "active");

        let passive = json!({"passiveSignalId": "passive-one", "status": "blocked"});
        assert_eq!(select_signal_id(&passive), Some("passive-one"));
        assert_eq!(resolve_signal_status(&passive), "blocked");

        for (value, expected) in [
            (json!({"isPaused": true, "status": "active"}), "inactive"),
            (json!({"status": "inactive"}), "inactive"),
            (json!({"status": "active"}), "active"),
            (json!({"status": "unexpected"}), "active"),
            (json!({}), "active"),
        ] {
            assert_eq!(resolve_signal_status(&value), expected);
        }

        assert_eq!(select_signal_id(&json!({"label": "none"})), None);
        assert_eq!(string_field(&json!({"label": 42}), &["label"]), None);
    }

    #[test]
    fn validate_response_maps_all_optional_and_required_fields() {
        let response = ValidateResponse {
            auth_type: "API_KEY".to_owned(),
            auth_token_id: "token-one".to_owned(),
            session_id: Some("session-one".to_owned()),
            api_key_id: Some("key-one".to_owned()),
            user_id: "user-one".to_owned(),
            user_type: "SUBJECT".to_owned(),
            expires_at: None,
            revalidate_at: "2026-07-28T12:00:00Z".to_owned(),
        };
        assert_eq!(
            validate_to_json(&response),
            json!({
                "authType": "API_KEY",
                "authTokenId": "token-one",
                "sessionId": "session-one",
                "apiKeyId": "key-one",
                "userId": "user-one",
                "userType": "SUBJECT",
                "expiresAt": null,
                "revalidateAt": "2026-07-28T12:00:00Z"
            })
        );
    }
}
