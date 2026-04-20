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
        StreamOutputFilter::Data => event.is_data_event() && event.is_status_stream() == false,
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
