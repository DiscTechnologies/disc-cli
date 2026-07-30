use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum HttpCredential<'a> {
    ApiKey(&'a str),
    OAuth {
        access_token: &'a str,
        subject_context_token: &'a str,
    },
}

pub trait IntoHttpCredential<'a> {
    fn into_http_credential(self) -> HttpCredential<'a>;
}

impl<'a> IntoHttpCredential<'a> for HttpCredential<'a> {
    fn into_http_credential(self) -> HttpCredential<'a> {
        self
    }
}

impl<'a> IntoHttpCredential<'a> for &'a str {
    fn into_http_credential(self) -> HttpCredential<'a> {
        HttpCredential::ApiKey(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ValidateResponse {
    #[serde(rename = "authType")]
    pub auth_type: String,
    #[serde(rename = "authTokenId")]
    pub auth_token_id: String,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    #[serde(rename = "apiKeyId")]
    pub api_key_id: Option<String>,
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "userType")]
    pub user_type: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<String>,
    #[serde(rename = "revalidateAt")]
    pub revalidate_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PassiveSignalSummary {
    #[serde(rename = "passiveSignalId")]
    pub passive_signal_id: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActiveSignalSummary {
    #[serde(rename = "activeSignalId")]
    pub active_signal_id: String,
    #[serde(rename = "passiveSignalId")]
    pub passive_signal_id: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PassiveSignalListResponse {
    #[serde(rename = "passiveSignals")]
    pub passive_signals: Vec<PassiveSignalSummary>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActiveSignalListResponse {
    #[serde(rename = "activeSignals")]
    pub active_signals: Vec<ActiveSignalSummary>,
}

#[derive(Debug, Clone)]
pub struct DiscApiClient {
    client: reqwest::Client,
    base_url: String,
}

impl DiscApiClient {
    pub fn new<'a>(base_url: String, credential: impl IntoHttpCredential<'a>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        match credential.into_http_credential() {
            HttpCredential::ApiKey(api_key) => {
                headers.insert(
                    HeaderName::from_static("x-disc-api-key"),
                    HeaderValue::from_str(api_key)
                        .context("The configured API key is not a valid HTTP header value.")?,
                );
            }
            HttpCredential::OAuth {
                access_token,
                subject_context_token,
            } => {
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {access_token}"))
                        .context("The OAuth access token is not a valid HTTP header value.")?,
                );
                headers.insert(
                    HeaderName::from_static("x-disc-subject-context"),
                    HeaderValue::from_str(subject_context_token)
                        .context("The subject-context token is not a valid HTTP header value.")?,
                );
            }
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("Failed to build HTTP client.")?;

        Ok(Self { client, base_url })
    }

    pub async fn validate(&self) -> Result<ValidateResponse> {
        self.get_json("/validate").await
    }

    pub async fn list_passive_signals(&self) -> Result<Vec<Value>> {
        let response = self.get_json::<Value>("/passive-signals").await?;
        let passive_signals = response
            .get("passiveSignals")
            .and_then(Value::as_array)
            .cloned()
            .context("`passiveSignals` array missing from response.")?;
        Ok(passive_signals)
    }

    pub async fn list_passive_signal_summaries(&self) -> Result<Vec<PassiveSignalSummary>> {
        let response = self
            .get_json::<PassiveSignalListResponse>("/passive-signals")
            .await?;
        Ok(response.passive_signals)
    }

    pub async fn get_passive_signal(&self, passive_signal_id: &str) -> Result<Value> {
        self.get_json(&format!(
            "/passive-signals/{}",
            urlencoding::encode(passive_signal_id)
        ))
        .await
    }

    pub async fn list_active_signals(&self, passive_signal_id: &str) -> Result<Vec<Value>> {
        let path = format!(
            "/passive-signals/{}/active-signals",
            urlencoding::encode(passive_signal_id)
        );
        let response = self.get_json::<Value>(&path).await?;
        let active_signals = response
            .get("activeSignals")
            .and_then(Value::as_array)
            .cloned()
            .context("`activeSignals` array missing from response.")?;
        Ok(active_signals)
    }

    pub async fn list_active_signal_summaries(
        &self,
        passive_signal_id: &str,
    ) -> Result<Vec<ActiveSignalSummary>> {
        let path = format!(
            "/passive-signals/{}/active-signals",
            urlencoding::encode(passive_signal_id)
        );
        let response = self.get_json::<ActiveSignalListResponse>(&path).await?;
        Ok(response.active_signals)
    }

    pub async fn get_active_signal(&self, active_signal_id: &str) -> Result<Value> {
        self.get_json(&format!(
            "/active-signals/{}",
            urlencoding::encode(active_signal_id)
        ))
        .await
    }

    async fn get_json<T>(&self, path: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .context(format!("HTTP request failed for {url}."))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            let sanitized_body = if body.is_empty() {
                "<empty body>".to_owned()
            } else {
                body
            };
            anyhow::bail!("HTTP {} {}: {}", status.as_u16(), status, sanitized_body);
        }

        serde_json::from_str::<T>(&body).context({
            format!(
                "Failed to decode JSON response from {url}. Response body began with: {}",
                body.chars().take(200).collect::<String>()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    use serde_json::json;

    use super::DiscApiClient;

    fn serve_once(status: &str, body: &str) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
        let address = listener.local_addr().expect("server address");
        let status = status.to_owned();
        let body = body.to_owned();
        let (request_sender, request_receiver) = mpsc::channel();

        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let count = stream.read(&mut buffer).expect("read request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = request_sender.send(String::from_utf8(request).expect("HTTP request is UTF-8"));

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });

        (format!("http://{address}"), request_receiver)
    }

    fn assert_request(receiver: Receiver<String>, path: &str) {
        let request = receiver.recv().expect("captured request");
        assert!(
            request.starts_with(&format!("GET {path} HTTP/1.1\r\n")),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-disc-api-key: test-key\r\n"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn validates_auth_and_propagates_api_key_header() {
        let body = json!({
            "authType": "API_KEY",
            "authTokenId": "token-one",
            "sessionId": "session-one",
            "apiKeyId": "key-one",
            "userId": "user-one",
            "userType": "SUBJECT",
            "expiresAt": null,
            "revalidateAt": "2026-07-28T12:00:00Z"
        })
        .to_string();
        let (base_url, request) = serve_once("200 OK", &body);
        let client =
            DiscApiClient::new(format!("{base_url}/"), "test-key").expect("create API client");

        let response = client.validate().await.expect("validate response");

        assert_eq!(response.auth_type, "API_KEY");
        assert_eq!(response.auth_token_id, "token-one");
        assert_eq!(response.session_id.as_deref(), Some("session-one"));
        assert_eq!(response.api_key_id.as_deref(), Some("key-one"));
        assert_eq!(response.user_id, "user-one");
        assert_eq!(response.user_type, "SUBJECT");
        assert_eq!(response.expires_at, None);
        assert_eq!(response.revalidate_at, "2026-07-28T12:00:00Z");
        assert_request(request, "/validate");
    }

    #[tokio::test]
    async fn lists_passive_signals_as_values_and_typed_summaries() {
        let body = json!({
            "passiveSignals": [{
                "passiveSignalId": "passive-one",
                "label": "Revenue"
            }]
        })
        .to_string();
        let (base_url, request) = serve_once("200 OK", &body);
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let values = client.list_passive_signals().await.expect("passive values");
        assert_eq!(values[0]["passiveSignalId"], "passive-one");
        assert_request(request, "/passive-signals");

        let (base_url, request) = serve_once("200 OK", &body);
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let summaries = client
            .list_passive_signal_summaries()
            .await
            .expect("passive summaries");
        assert_eq!(summaries[0].passive_signal_id, "passive-one");
        assert_eq!(summaries[0].label, "Revenue");
        assert_request(request, "/passive-signals");
    }

    #[tokio::test]
    async fn gets_passive_and_active_signals_with_encoded_identifiers() {
        let (base_url, request) = serve_once("200 OK", r#"{"label":"Passive"}"#);
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let passive = client
            .get_passive_signal("passive / one")
            .await
            .expect("passive signal");
        assert_eq!(passive["label"], "Passive");
        assert_request(request, "/passive-signals/passive%20%2F%20one");

        let (base_url, request) = serve_once("200 OK", r#"{"label":"Active"}"#);
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let active = client
            .get_active_signal("active / one")
            .await
            .expect("active signal");
        assert_eq!(active["label"], "Active");
        assert_request(request, "/active-signals/active%20%2F%20one");
    }

    #[tokio::test]
    async fn lists_active_signals_as_values_and_typed_summaries() {
        let body = json!({
            "activeSignals": [{
                "activeSignalId": "active-one",
                "passiveSignalId": "passive-one",
                "label": "Revenue average"
            }]
        })
        .to_string();
        let expected_path = "/passive-signals/passive%20one/active-signals";

        let (base_url, request) = serve_once("200 OK", &body);
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let values = client
            .list_active_signals("passive one")
            .await
            .expect("active values");
        assert_eq!(values[0]["activeSignalId"], "active-one");
        assert_request(request, expected_path);

        let (base_url, request) = serve_once("200 OK", &body);
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let summaries = client
            .list_active_signal_summaries("passive one")
            .await
            .expect("active summaries");
        assert_eq!(summaries[0].active_signal_id, "active-one");
        assert_eq!(summaries[0].passive_signal_id, "passive-one");
        assert_eq!(summaries[0].label, "Revenue average");
        assert_request(request, expected_path);
    }

    #[tokio::test]
    async fn missing_signal_arrays_are_rejected() {
        let (base_url, _) = serve_once("200 OK", "{}");
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let error = client
            .list_passive_signals()
            .await
            .expect_err("missing passive signals");
        assert!(error.to_string().contains("`passiveSignals` array missing"));

        let (base_url, _) = serve_once("200 OK", r#"{"activeSignals":"invalid"}"#);
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let error = client
            .list_active_signals("passive")
            .await
            .expect_err("invalid active signals");
        assert!(error.to_string().contains("`activeSignals` array missing"));
    }

    #[tokio::test]
    async fn http_errors_preserve_status_and_body_without_hiding_empty_responses() {
        let (base_url, _) = serve_once("403 Forbidden", r#"{"error":"denied"}"#);
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let error = client.validate().await.expect_err("forbidden response");
        assert!(
            error
                .to_string()
                .contains(r#"HTTP 403 403 Forbidden: {"error":"denied"}"#)
        );

        let (base_url, _) = serve_once("500 Internal Server Error", "");
        let client = DiscApiClient::new(base_url, "test-key").expect("create API client");
        let error = client.validate().await.expect_err("empty error response");
        assert!(error.to_string().contains("<empty body>"));
    }

    #[tokio::test]
    async fn malformed_json_reports_endpoint_and_bounded_body_context() {
        let body = "x".repeat(250);
        let (base_url, _) = serve_once("200 OK", &body);
        let client = DiscApiClient::new(base_url.clone(), "test-key").expect("create API client");
        let error = client.validate().await.expect_err("malformed response");
        let message = error.to_string();
        assert!(message.contains(&format!("{base_url}/validate")));
        assert!(message.contains(&"x".repeat(200)));
        assert!(!message.contains(&"x".repeat(201)));
    }

    #[test]
    fn invalid_api_key_header_value_is_rejected() {
        let error = DiscApiClient::new("https://api.example.test".to_owned(), "bad\nkey")
            .expect_err("invalid header");
        assert!(
            error
                .to_string()
                .contains("configured API key is not a valid HTTP header value")
        );
    }
}
