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

        if status.is_success() == false {
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
