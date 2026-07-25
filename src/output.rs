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
        .with_context(|| format!("Failed to open destination file at {}.", path.display()))?;
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
    use std::io;

    use serde_json::json;

    use super::*;

    struct CaptureWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        fail_write: bool,
        fail_flush: bool,
    }

    impl Write for CaptureWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                return Err(io::Error::other("write failed"));
            }
            self.bytes.lock().expect("capture lock").extend(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                return Err(io::Error::other("flush failed"));
            }
            Ok(())
        }
    }

    fn capture_writer(fail_write: bool, fail_flush: bool) -> (SharedWriter, Arc<Mutex<Vec<u8>>>) {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = CaptureWriter {
            bytes: bytes.clone(),
            fail_write,
            fail_flush,
        };
        (Arc::new(Mutex::new(Box::new(writer))), bytes)
    }

    fn data_event(stream_key: &str) -> InboundEvent {
        InboundEvent::Data {
            stream_key: stream_key.to_owned(),
            sequence: 2,
            payload_type: "VALUE".to_owned(),
            emitted_at_epoch_ms: 10,
            payload: json!({"value": 42}),
        }
    }

    #[test]
    fn event_filters_distinguish_data_status_and_control_events() {
        let data = data_event("PASSIVE_SIGNAL_RESULT:id");
        let status = data_event("PASSIVE_SIGNAL_STATUS:id");
        let control = InboundEvent::Control(json!({"type": "SUBSCRIBED"}));

        assert!(should_emit_event(&data, StreamOutputFilter::All));
        assert!(should_emit_event(&control, StreamOutputFilter::Events));
        assert!(!should_emit_event(&data, StreamOutputFilter::Events));
        assert!(should_emit_event(&data, StreamOutputFilter::Data));
        assert!(!should_emit_event(&status, StreamOutputFilter::Data));
        assert!(should_emit_event(&status, StreamOutputFilter::Status));
        assert!(!should_emit_event(&control, StreamOutputFilter::Status));
    }

    #[test]
    fn subscription_events_render_in_every_format() {
        for (format, expected) in [
            (StreamOutputFormat::Pretty, "DATA PASSIVE_SIGNAL_RESULT:id"),
            (StreamOutputFormat::Json, "\"type\": \"DATA\""),
            (StreamOutputFormat::Ndjson, "\"type\":\"DATA\""),
        ] {
            let (writer, bytes) = capture_writer(false, false);
            write_subscription_event(&writer, &data_event("PASSIVE_SIGNAL_RESULT:id"), format)
                .expect("write event");
            let rendered =
                String::from_utf8(bytes.lock().expect("bytes lock").clone()).expect("utf8 output");
            assert!(rendered.contains(expected));
            assert!(rendered.ends_with('\n'));
        }
    }

    #[test]
    fn subscription_event_reports_write_flush_and_poison_failures() {
        let (write_failure, _) = capture_writer(true, false);
        let error = write_subscription_event(
            &write_failure,
            &data_event("result"),
            StreamOutputFormat::Ndjson,
        )
        .expect_err("write failure");
        assert!(error.to_string().contains("Failed to write"));

        let (flush_failure, _) = capture_writer(false, true);
        let error = write_subscription_event(
            &flush_failure,
            &data_event("result"),
            StreamOutputFormat::Ndjson,
        )
        .expect_err("flush failure");
        assert!(error.to_string().contains("Failed to flush"));

        let (poisoned, _) = capture_writer(false, false);
        let poisoned_for_thread = poisoned.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned_for_thread.lock().expect("initial lock");
            panic!("poison writer");
        })
        .join();
        let error =
            write_subscription_event(&poisoned, &data_event("result"), StreamOutputFormat::Ndjson)
                .expect_err("poisoned writer");
        assert!(error.to_string().contains("Failed to lock"));
    }

    #[test]
    fn signal_helpers_select_ids_and_normalise_statuses() {
        assert_eq!(
            select_signal_id(&json!({"activeSignalId": "active", "passiveSignalId": "passive"})),
            Some("active")
        );
        assert_eq!(
            select_signal_id(&json!({"passiveSignalId": "passive"})),
            Some("passive")
        );
        assert_eq!(select_signal_id(&json!({})), None);
        assert_eq!(
            string_field(&json!({"second": "value"}), &["first", "second"]),
            Some("value")
        );
        assert_eq!(bool_field(&json!({"flag": true}), "flag"), Some(true));

        for (value, expected) in [
            (json!({"isPaused": true}), "inactive"),
            (json!({"status": "BLOCKED"}), "blocked"),
            (json!({"status": "inactive"}), "inactive"),
            (json!({"status": "active"}), "active"),
            (json!({"status": "running"}), "active"),
            (json!({"status": "custom"}), "active"),
            (json!({}), "active"),
        ] {
            assert_eq!(resolve_signal_status(&value), expected);
        }
    }

    #[test]
    fn validate_response_maps_to_public_json_shape() {
        let response = ValidateResponse {
            auth_type: "API_KEY".to_owned(),
            auth_token_id: "token".to_owned(),
            session_id: None,
            api_key_id: Some("key".to_owned()),
            user_id: "7".to_owned(),
            user_type: "individual".to_owned(),
            expires_at: None,
            revalidate_at: "tomorrow".to_owned(),
        };

        assert_eq!(
            validate_to_json(&response),
            json!({
                "authType": "API_KEY",
                "authTokenId": "token",
                "sessionId": null,
                "apiKeyId": "key",
                "userId": "7",
                "userType": "individual",
                "expiresAt": null,
                "revalidateAt": "tomorrow",
            })
        );
    }

    #[test]
    fn file_writer_appends_rendered_events() {
        let path = std::env::temp_dir().join(format!(
            "disc-cli-output-{}-{}.ndjson",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        if path.exists() {
            std::fs::remove_file(&path).expect("remove stale output");
        }
        let writer = create_file_writer(&path).expect("file writer");
        write_subscription_event(
            &writer,
            &InboundEvent::Control(json!({"type": "READY"})),
            StreamOutputFormat::Ndjson,
        )
        .expect("write file event");
        drop(writer);

        let contents = std::fs::read_to_string(&path).expect("read output");
        assert_eq!(contents, "{\"type\":\"READY\"}\n");
        std::fs::remove_file(path).expect("remove output");
    }

    #[test]
    fn print_helpers_accept_every_output_format() {
        let value = json!({"passiveSignalId": "signal", "label": "Signal"});
        print_json_value(&value, JsonOutputFormat::Json).expect("pretty json");
        print_json_value(&value, JsonOutputFormat::Ndjson).expect("ndjson");
        print_signal_list(std::slice::from_ref(&value), ListOutputFormat::Json).expect("list json");
        print_signal_list(std::slice::from_ref(&value), ListOutputFormat::Ndjson)
            .expect("list ndjson");
        print_signal_list(&[value, json!({})], ListOutputFormat::Table).expect("list table");
        let _ = create_stdout_writer();
    }
}
