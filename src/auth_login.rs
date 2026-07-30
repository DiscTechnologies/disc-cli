use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::config::{ConfigStore, StoredAuthProfile};

const DEFAULT_API_BASE_URL: &str = "https://api.disc.tech";
const FALLBACK_POLL_INTERVAL_SECONDS: u64 = 5;
const MAX_POLL_INTERVAL_SECONDS: u64 = 30;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateApprovalRequest<'a> {
    client_name: &'a str,
    client_version: &'a str,
    machine_label: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateApprovalResponse {
    device_code: String,
    user_code: String,
    approval_token: String,
    verification_uri: String,
    expires_at: String,
    interval_seconds: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PollApprovalRequest<'a> {
    device_code: &'a str,
    approval_token: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code)]
struct PollApproval {
    cli_login_approval_id: String,
    client_name: String,
    client_version: Option<String>,
    subject_id: Option<String>,
    machine_label: Option<String>,
    requested_scopes: Vec<String>,
    status: String,
    expires_at: String,
    approved_at: Option<String>,
    credential_issued_at: Option<String>,
    credential_retrieved_at: Option<String>,
    denied_at: Option<String>,
    revoked_at: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code)]
struct ApiKeySummary {
    api_key_id: String,
    label: String,
    key_identifier: String,
    preview: String,
    created_at: String,
    last_used_at: Option<String>,
    revoked_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CliLoginSubject {
    subject_id: String,
    subject_key: String,
    subject_kind: String,
    display_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CliLoginCredential {
    #[serde(rename = "type")]
    credential_type: String,
    raw_key: String,
    api_key: ApiKeySummary,
    subject: CliLoginSubject,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PollApprovalResponse {
    approval: PollApproval,
    credential: Option<CliLoginCredential>,
}

fn bounded_machine_label(machine_label: Option<String>) -> String {
    let candidate = machine_label
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Disc CLI".to_owned());
    candidate.trim().chars().take(160).collect()
}

fn profile_name(subject: &CliLoginSubject, api_base_url: &str) -> Result<String> {
    let parsed_url = reqwest::Url::parse(api_base_url).context("Disc API base URL is invalid.")?;
    let origin = parsed_url
        .host_str()
        .context("Disc API base URL must include a host.")?;
    let origin = match parsed_url.port() {
        Some(port) => format!("{origin}-{port}"),
        None => origin.to_owned(),
    };
    Ok(format!(
        "{}@{}#{}",
        subject.subject_key, subject.subject_id, origin
    ))
}

fn parse_expiry(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .context("Disc CLI login returned an invalid expiry timestamp.")
}

async fn post_json<TRequest: Serialize, TResponse: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: String,
    request: &TRequest,
) -> Result<Option<TResponse>> {
    let response = match client.post(url).json(request).send().await {
        Ok(response) => response,
        Err(error) if error.is_connect() || error.is_timeout() || error.is_request() => {
            return Ok(None);
        }
        Err(error) => return Err(error).context("Disc CLI login request failed."),
    };

    if response.status().is_server_error() || response.status() == StatusCode::TOO_MANY_REQUESTS {
        return Ok(None);
    }

    let response = response
        .error_for_status()
        .context("Disc CLI login request was rejected.")?;
    Ok(Some(
        response
            .json::<TResponse>()
            .await
            .context("Disc CLI login returned an invalid response.")?,
    ))
}

pub async fn login(
    store: &ConfigStore,
    api_base_url: Option<String>,
    machine_label: Option<String>,
    no_browser: bool,
) -> Result<()> {
    let api_base_url = api_base_url.unwrap_or_else(|| DEFAULT_API_BASE_URL.to_owned());
    let machine_label = bounded_machine_label(machine_label);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("Failed to build the Disc login HTTP client.")?;
    let create_url = format!(
        "{}/auth/cli-login/approvals",
        api_base_url.trim_end_matches('/')
    );
    let approval = post_json::<_, CreateApprovalResponse>(
        &client,
        create_url,
        &CreateApprovalRequest {
            client_name: "Disc CLI",
            client_version: env!("CARGO_PKG_VERSION"),
            machine_label: &machine_label,
        },
    )
    .await?
    .context("Disc CLI login service is temporarily unavailable.")?;
    let expires_at = parse_expiry(&approval.expires_at)?;

    println!("Open this URL to approve Disc CLI login:");
    println!("{}", approval.verification_uri);
    println!("User code: {}", approval.user_code);

    if !no_browser && webbrowser::open(&approval.verification_uri).is_err() {
        eprintln!("Warning: Could not open a browser automatically. Open the URL above manually.");
    }

    let interval_seconds = if approval.interval_seconds == 0 {
        FALLBACK_POLL_INTERVAL_SECONDS
    } else {
        approval
            .interval_seconds
            .clamp(1, MAX_POLL_INTERVAL_SECONDS)
    };
    let poll_url = format!(
        "{}/auth/cli-login/approvals/poll",
        api_base_url.trim_end_matches('/')
    );

    loop {
        if Utc::now() >= expires_at {
            bail!("Disc CLI login expired before approval.");
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => bail!("Disc CLI login cancelled."),
            _ = tokio::time::sleep(Duration::from_secs(interval_seconds)) => {}
        }

        let Some(result) = post_json::<_, PollApprovalResponse>(
            &client,
            poll_url.clone(),
            &PollApprovalRequest {
                device_code: &approval.device_code,
                approval_token: &approval.approval_token,
            },
        )
        .await?
        else {
            eprintln!("Waiting for Disc login service...");
            continue;
        };
        let _ = parse_expiry(&result.approval.expires_at)?;

        match result.approval.status.as_str() {
            "pending" => println!("Waiting for browser approval..."),
            "approved" => {
                let credential = result
                    .credential
                    .context("Disc CLI login was approved, but no credential was returned.")?;
                if credential.credential_type != "api_key" || credential.raw_key.trim().is_empty() {
                    bail!("Disc CLI login returned an invalid credential.");
                }
                let profile = profile_name(&credential.subject, &api_base_url)?;
                store.save_profile(StoredAuthProfile {
                    profile: profile.clone(),
                    api_key: credential.raw_key,
                    api_base_url,
                    subject_id: Some(credential.subject.subject_id),
                    subject_key: Some(credential.subject.subject_key),
                    subject_kind: Some(credential.subject.subject_kind),
                    display_name: Some(credential.subject.display_name.clone()),
                    created_at: Some(credential.api_key.created_at),
                })?;
                println!(
                    "Disc CLI login complete. Active profile: {} ({}) [{}].",
                    credential.subject.display_name, profile, credential.api_key.label
                );
                return Ok(());
            }
            "denied" | "expired" | "revoked" => {
                bail!("Disc CLI login {}.", result.approval.status)
            }
            _ => bail!("Disc CLI login returned an unknown approval status."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CliLoginSubject, bounded_machine_label, profile_name};

    #[test]
    fn machine_labels_are_trimmed_and_bounded() {
        assert_eq!(
            bounded_machine_label(Some("  workstation  ".to_owned())),
            "workstation"
        );
        assert_eq!(bounded_machine_label(Some("x".repeat(200))).len(), 160);
    }

    #[test]
    fn profile_names_are_stable_subject_identifiers() {
        let subject = CliLoginSubject {
            subject_id: "42".to_owned(),
            subject_key: "example-org".to_owned(),
            subject_kind: "ORGANISATION".to_owned(),
            display_name: "Example".to_owned(),
        };
        assert_eq!(
            profile_name(&subject, "https://api.disc.tech").expect("profile"),
            "example-org@42#api.disc.tech"
        );
        assert_eq!(
            profile_name(&subject, "http://localhost:3001").expect("profile"),
            "example-org@42#localhost-3001"
        );
    }
}
