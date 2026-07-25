use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::Value;

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
    pub fn new(base_url: String, api_key: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let header_name = HeaderName::from_static("x-disc-api-key");
        let header_value = HeaderValue::from_str(api_key)
            .context("The configured API key is not a valid HTTP header value.")?;
        headers.insert(header_name, header_value);

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
            .with_context(|| format!("HTTP request failed for {url}."))?;

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

        serde_json::from_str::<T>(&body).with_context(|| {
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
    use std::thread::JoinHandle;

    use super::*;

    fn spawn_server(status: &str, body: &str) -> (String, Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
        let address = listener.local_addr().expect("server address");
        let (sender, receiver) = mpsc::channel();
        let status = status.to_owned();
        let body = body.to_owned();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = vec![0; 8192];
            let length = stream.read(&mut request).expect("read request");
            sender
                .send(String::from_utf8_lossy(&request[..length]).into_owned())
                .expect("send captured request");
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        (format!("http://{address}"), receiver, handle)
    }

    fn finish(receiver: Receiver<String>, handle: JoinHandle<()>) -> String {
        let request = receiver.recv().expect("captured request");
        handle.join().expect("server thread");
        request
    }

    #[test]
    fn client_rejects_api_keys_that_are_invalid_header_values() {
        let error = DiscApiClient::new("http://localhost".to_owned(), "bad\nkey")
            .expect_err("invalid header");
        assert!(error.to_string().contains("not a valid HTTP header"));
    }

    #[tokio::test]
    async fn validate_decodes_identity_and_sends_api_key_header() {
        let (base_url, receiver, handle) = spawn_server(
            "200 OK",
            r#"{
                "authType":"API_KEY",
                "authTokenId":"token",
                "sessionId":null,
                "apiKeyId":"key",
                "userId":"7",
                "userType":"individual",
                "expiresAt":null,
                "revalidateAt":"tomorrow"
            }"#,
        );
        let client = DiscApiClient::new(format!("{base_url}/"), "secret").expect("client");

        let response = client.validate().await.expect("validate");

        assert_eq!(response.auth_type, "API_KEY");
        assert_eq!(response.auth_token_id, "token");
        assert_eq!(response.api_key_id.as_deref(), Some("key"));
        assert_eq!(response.user_id, "7");
        let request = finish(receiver, handle);
        assert!(request.starts_with("GET /validate HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-disc-api-key: secret")
        );
    }

    #[tokio::test]
    async fn passive_signal_methods_decode_lists_summaries_and_details() {
        let (base_url, receiver, handle) = spawn_server(
            "200 OK",
            r#"{"passiveSignals":[{"passiveSignalId":"one","label":"One"}]}"#,
        );
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let values = client.list_passive_signals().await.expect("passive values");
        assert_eq!(values[0]["passiveSignalId"], "one");
        assert!(finish(receiver, handle).starts_with("GET /passive-signals "));

        let (base_url, receiver, handle) = spawn_server(
            "200 OK",
            r#"{"passiveSignals":[{"passiveSignalId":"one","label":"One"}]}"#,
        );
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let summaries = client
            .list_passive_signal_summaries()
            .await
            .expect("passive summaries");
        assert_eq!(summaries[0].passive_signal_id, "one");
        assert_eq!(summaries[0].label, "One");
        finish(receiver, handle);

        let (base_url, receiver, handle) =
            spawn_server("200 OK", r#"{"passiveSignalId":"one/two"}"#);
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let detail = client
            .get_passive_signal("one/two")
            .await
            .expect("passive detail");
        assert_eq!(detail["passiveSignalId"], "one/two");
        assert!(finish(receiver, handle).starts_with("GET /passive-signals/one%2Ftwo "));
    }

    #[tokio::test]
    async fn active_signal_methods_decode_lists_summaries_and_details() {
        let body = r#"{"activeSignals":[{"activeSignalId":"active","passiveSignalId":"passive","label":"Active"}]}"#;
        let (base_url, receiver, handle) = spawn_server("200 OK", body);
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let values = client
            .list_active_signals("passive/id")
            .await
            .expect("active values");
        assert_eq!(values[0]["activeSignalId"], "active");
        assert!(
            finish(receiver, handle)
                .starts_with("GET /passive-signals/passive%2Fid/active-signals ")
        );

        let (base_url, receiver, handle) = spawn_server("200 OK", body);
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let summaries = client
            .list_active_signal_summaries("passive")
            .await
            .expect("active summaries");
        assert_eq!(summaries[0].active_signal_id, "active");
        assert_eq!(summaries[0].passive_signal_id, "passive");
        assert_eq!(summaries[0].label, "Active");
        finish(receiver, handle);

        let (base_url, receiver, handle) =
            spawn_server("200 OK", r#"{"activeSignalId":"active/id"}"#);
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let detail = client
            .get_active_signal("active/id")
            .await
            .expect("active detail");
        assert_eq!(detail["activeSignalId"], "active/id");
        assert!(finish(receiver, handle).starts_with("GET /active-signals/active%2Fid "));
    }

    #[tokio::test]
    async fn list_methods_reject_missing_arrays() {
        let (base_url, receiver, handle) =
            spawn_server("200 OK", r#"{"passiveSignals":"invalid"}"#);
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let error = client
            .list_passive_signals()
            .await
            .expect_err("missing passive array");
        assert!(error.to_string().contains("`passiveSignals` array missing"));
        finish(receiver, handle);

        let (base_url, receiver, handle) = spawn_server("200 OK", r#"{"activeSignals":null}"#);
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let error = client
            .list_active_signals("passive")
            .await
            .expect_err("missing active array");
        assert!(error.to_string().contains("`activeSignals` array missing"));
        finish(receiver, handle);
    }

    #[tokio::test]
    async fn request_errors_include_status_body_and_decode_context() {
        let (base_url, receiver, handle) = spawn_server("403 Forbidden", r#"{"error":"denied"}"#);
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let error = client
            .get_active_signal("active")
            .await
            .expect_err("forbidden");
        assert!(error.to_string().contains("HTTP 403"));
        assert!(error.to_string().contains("denied"));
        finish(receiver, handle);

        let (base_url, receiver, handle) = spawn_server("500 Internal Server Error", "");
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let error = client
            .get_active_signal("active")
            .await
            .expect_err("empty error");
        assert!(error.to_string().contains("<empty body>"));
        finish(receiver, handle);

        let long_invalid_body = "x".repeat(250);
        let (base_url, receiver, handle) = spawn_server("200 OK", &long_invalid_body);
        let client = DiscApiClient::new(base_url, "key").expect("client");
        let error = client
            .get_active_signal("active")
            .await
            .expect_err("invalid json");
        assert!(error.to_string().contains("Failed to decode JSON"));
        assert!(error.to_string().contains(&"x".repeat(200)));
        assert!(!error.to_string().contains(&"x".repeat(201)));
        finish(receiver, handle);
    }

    #[tokio::test]
    async fn connection_failures_include_the_requested_url() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let address = listener.local_addr().expect("address");
        drop(listener);
        let base_url = format!("http://{address}");
        let client = DiscApiClient::new(base_url.clone(), "key").expect("client");

        let error = client.validate().await.expect_err("connection should fail");

        assert!(error.to_string().contains("HTTP request failed"));
        assert!(format!("{error:#}").contains(&format!("{base_url}/validate")));
    }
}
