use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use dialoguer::{Select, theme::ColorfulTheme};
use fs2::FileExt;
use oauth2::{CsrfToken, PkceCodeChallenge};
use reqwest::{StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::config::{ConfigStore, StoredAuthProfile};
use crate::credential_store::{CredentialStore, SystemCredentialStore};

const DEFAULT_API_BASE_URL: &str = "https://api.disc.tech";
const DEFAULT_ISSUER: &str = "https://sso.disc.tech/realms/disc";
const DEFAULT_CLIENT_ID: &str = "disc-cli";
const CALLBACK_PATH: &str = "/oauth/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
struct ProviderMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    device_authorization_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: String,
}

#[derive(Debug)]
pub struct TokenSet {
    pub access_token: SecretString,
    pub refresh_token: SecretString,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
enum DevicePollAction {
    Continue(Duration),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionSubject {
    subject_id: String,
    subject_key: String,
    subject_kind: String,
    display_name: String,
    selectable: bool,
    capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    user_id: String,
    subjects: Vec<SessionSubject>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubjectContextResponse {
    context_token: String,
    subject: SessionSubject,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubjectContextRequest<'a> {
    operation_id: String,
    subject_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_context_token: Option<&'a str>,
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .context("Failed to build the OAuth HTTP client.")
}

pub fn validate_api_base_url(value: &str) -> Result<String> {
    let parsed = Url::parse(value).context("Disc API base URL is invalid.")?;
    let is_secure = parsed.scheme() == "https";
    let is_loopback_http = parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1");

    if !is_secure && !is_loopback_http {
        bail!("Disc API base URL must use HTTPS outside local development.");
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        bail!("Disc API base URL must be an origin without credentials, path, query, or fragment.");
    }

    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

async fn read_bounded_response(response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_BODY_BYTES as u64)
    {
        bail!("OAuth response exceeded the maximum allowed size.");
    }
    let bytes = response
        .bytes()
        .await
        .context("Failed to read OAuth response.")?;
    if bytes.len() > MAX_HTTP_BODY_BYTES {
        bail!("OAuth response exceeded the maximum allowed size.");
    }
    Ok(bytes.to_vec())
}

async fn discover(client: &reqwest::Client, issuer: &str) -> Result<ProviderMetadata> {
    let issuer = issuer.trim_end_matches('/');
    let url = format!("{issuer}/.well-known/openid-configuration");
    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to discover the Disc identity provider.")?
        .error_for_status()
        .context("Disc identity-provider discovery was rejected.")?;
    let metadata: ProviderMetadata =
        serde_json::from_slice(&read_bounded_response(response).await?)
            .context("Disc identity-provider discovery returned invalid JSON.")?;
    if metadata.issuer.trim_end_matches('/') != issuer {
        bail!("Identity-provider discovery returned a different issuer.");
    }
    let issuer_url = Url::parse(issuer).context("Disc identity-provider issuer URL is invalid.")?;
    for endpoint in [
        Some(metadata.authorization_endpoint.as_str()),
        Some(metadata.token_endpoint.as_str()),
        metadata.device_authorization_endpoint.as_deref(),
        metadata.revocation_endpoint.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let parsed = Url::parse(endpoint)
            .context("Identity-provider metadata contains an invalid endpoint.")?;
        let is_secure = parsed.scheme() == "https";
        let is_loopback_http = parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1");
        if !is_secure && !is_loopback_http {
            bail!("Identity-provider endpoints must use HTTPS outside local development.");
        }
        if parsed.scheme() != issuer_url.scheme()
            || parsed.host_str() != issuer_url.host_str()
            || parsed.port_or_known_default() != issuer_url.port_or_known_default()
        {
            bail!("Identity-provider metadata contains a cross-origin endpoint.");
        }
    }
    Ok(metadata)
}

fn validate_token_response(raw: RawTokenResponse) -> Result<TokenSet> {
    if !raw.token_type.eq_ignore_ascii_case("bearer") || raw.access_token.trim().is_empty() {
        bail!("Identity provider returned an invalid bearer token response.");
    }
    let refresh_token = raw
        .refresh_token
        .filter(|value| !value.trim().is_empty())
        .context("Identity provider did not return a refresh token.")?;
    if raw.expires_in == 0 {
        bail!("Identity provider returned an already-expired access token.");
    }
    Ok(TokenSet {
        access_token: SecretString::from(raw.access_token),
        refresh_token: SecretString::from(refresh_token),
    })
}

async fn exchange_form(
    client: &reqwest::Client,
    token_endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenSet> {
    let response = client
        .post(token_endpoint)
        .form(form)
        .send()
        .await
        .context("OAuth token request failed.")?;
    let status = response.status();
    let bytes = read_bounded_response(response).await?;
    if !status.is_success() {
        let oauth_error = serde_json::from_slice::<OAuthErrorResponse>(&bytes).ok();
        let code = oauth_error
            .as_ref()
            .map(|value| safe_oauth_error_code(&value.error))
            .unwrap_or_else(|| "unknown_error".to_owned());
        bail!("OAuth token request failed ({code}).");
    }
    validate_token_response(
        serde_json::from_slice(&bytes)
            .context("Identity provider returned an invalid token response.")?,
    )
}

fn safe_oauth_error_code(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        value.to_owned()
    } else {
        "unknown_error".to_owned()
    }
}

fn callback_page(is_success: bool) -> String {
    let (page_title, state_class, symbol, heading, description) = if is_success {
        (
            "Disc CLI connected",
            "success",
            "✓",
            "CLI connected",
            "Authorization completed successfully. You can return to your terminal and continue using Disc.",
        )
    } else {
        (
            "Disc CLI authorization cancelled",
            "cancelled",
            "×",
            "Authorization cancelled",
            "Disc CLI was not connected. Return to your terminal to try again when you are ready.",
        )
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="dark">
  <title>{page_title}</title>
  <style>{}</style>
</head>
<body>
  <div class="wave wave-one" aria-hidden="true"></div>
  <div class="wave wave-two" aria-hidden="true"></div>
  <main aria-labelledby="callback-title">
    <div class="brand" aria-label="Disc"><span class="brand-mark" aria-hidden="true"></span><span>DISC</span></div>
    <div class="content {state_class}">
      <div class="status" aria-hidden="true">{symbol}</div>
      <div class="copy">
        <h1 id="callback-title">{heading}</h1>
        <p>{description}</p>
      </div>
    </div>
    <p class="hint">This window can now be closed safely.</p>
  </main>
</body>
</html>"#,
        include_str!("oauth_callback.css")
    )
}

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    let callback = async {
        let (mut stream, peer) = listener
            .accept()
            .await
            .context("Failed to accept OAuth callback.")?;
        if !peer.ip().is_loopback() {
            bail!("OAuth callback did not originate from the loopback interface.");
        }
        let expected_host = format!(
            "127.0.0.1:{}",
            listener
                .local_addr()
                .context("Failed to resolve OAuth callback address.")?
                .port()
        );
        let mut request = Vec::with_capacity(2048);
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|value| value == b"\r\n\r\n") {
            if request.len() >= 8192 {
                bail!("OAuth callback request headers were too large.");
            }
            let count = stream
                .read(&mut chunk)
                .await
                .context("Failed to read OAuth callback.")?;
            if count == 0 {
                bail!("OAuth callback ended before its headers were complete.");
            }
            request.extend_from_slice(&chunk[..count]);
        }
        let request =
            std::str::from_utf8(&request).context("OAuth callback was not valid HTTP.")?;
        let request_line = request
            .lines()
            .next()
            .context("OAuth callback request was empty.")?;
        let target = request_line
            .strip_prefix("GET ")
            .and_then(|value| value.split_once(" HTTP/1.").map(|pair| pair.0))
            .context("OAuth callback used an unsupported request format.")?;
        let host_headers: Vec<_> = request
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim())
            .collect();
        if host_headers != [expected_host.as_str()] {
            bail!("OAuth callback used an unexpected Host header.");
        }
        let (code, error) = parse_callback_target(target, expected_state)?;
        let (status, body) = if error.is_some() {
            ("400 Bad Request", callback_page(false))
        } else {
            ("200 OK", callback_page(true))
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .context("Failed to answer OAuth callback.")?;
        if let Some(error) = error {
            bail!("Disc CLI authorization was denied ({error}).");
        }
        code.context("OAuth callback omitted the authorization code.")
    };
    tokio::time::timeout(CALLBACK_TIMEOUT, callback)
        .await
        .context("Timed out waiting for browser authorization.")?
}

fn parse_callback_target(
    target: &str,
    expected_state: &str,
) -> Result<(Option<String>, Option<String>)> {
    let parsed = Url::parse(&format!("http://127.0.0.1{target}"))
        .context("OAuth callback target was invalid.")?;
    if parsed.path() != CALLBACK_PATH {
        bail!("OAuth callback used an unexpected path.");
    }
    if parsed.fragment().is_some() {
        bail!("OAuth callback must not contain a fragment.");
    }
    let values = |name: &str| -> Vec<String> {
        parsed
            .query_pairs()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .collect()
    };
    let states = values("state");
    let codes = values("code");
    let errors = values("error");
    if states.len() != 1 || codes.len() > 1 || errors.len() > 1 {
        bail!("OAuth callback contained duplicate or missing parameters.");
    }
    if codes.len() + errors.len() != 1 {
        bail!("OAuth callback must contain exactly one code or error.");
    }
    let state = &states[0];
    if state
        .as_bytes()
        .ct_eq(expected_state.as_bytes())
        .unwrap_u8()
        != 1
    {
        bail!("OAuth callback state did not match this login attempt.");
    }
    Ok((codes.into_iter().next(), errors.into_iter().next()))
}

async fn login_with_pkce(
    client: &reqwest::Client,
    metadata: &ProviderMetadata,
    client_id: &str,
    no_browser: bool,
) -> Result<TokenSet> {
    login_with_pkce_using(client, metadata, client_id, |authorization_url| {
        println!("Open this URL to authenticate Disc CLI:\n{authorization_url}");
        if !no_browser && webbrowser::open(authorization_url.as_str()).is_err() {
            eprintln!(
                "Warning: Could not open a browser automatically. Open the URL above manually."
            );
        }
        Ok(())
    })
    .await
}

async fn login_with_pkce_using<F>(
    client: &reqwest::Client,
    metadata: &ProviderMetadata,
    client_id: &str,
    on_authorization_url: F,
) -> Result<TokenSet>
where
    F: FnOnce(&Url) -> Result<()>,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind the OAuth loopback callback listener.")?;
    let port = listener
        .local_addr()
        .context("Failed to resolve OAuth callback address.")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let state = CsrfToken::new_random();
    let mut authorization_url = Url::parse(&metadata.authorization_endpoint)
        .context("Authorization endpoint URL is invalid.")?;
    authorization_url
        .query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", "disc-cli")
        .append_pair("state", state.secret())
        .append_pair("code_challenge", challenge.as_str())
        .append_pair("code_challenge_method", "S256");
    on_authorization_url(&authorization_url)?;
    let code = tokio::select! {
        result = wait_for_callback(listener, state.secret()) => result?,
        _ = tokio::signal::ctrl_c() => bail!("Disc CLI login cancelled."),
    };
    exchange_form(
        client,
        &metadata.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", verifier.secret()),
        ],
    )
    .await
}

async fn login_with_device(
    client: &reqwest::Client,
    metadata: &ProviderMetadata,
    client_id: &str,
    no_browser: bool,
) -> Result<TokenSet> {
    let endpoint = metadata
        .device_authorization_endpoint
        .as_deref()
        .context("The identity provider does not advertise device authorization.")?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let response = client
        .post(endpoint)
        .form(&[
            ("client_id", client_id),
            ("scope", "disc-cli"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .context("Device authorization request failed.")?
        .error_for_status()
        .context("Device authorization request was rejected.")?;
    let details: DeviceAuthorizationResponse =
        serde_json::from_slice(&read_bounded_response(response).await?)
            .context("Identity provider returned invalid device authorization details.")?;
    println!(
        "Open this URL to authenticate Disc CLI:\n{}",
        details.verification_uri
    );
    println!("User code: {}", details.user_code);
    if !no_browser && let Some(url) = &details.verification_uri_complete {
        let _ = webbrowser::open(url);
    }
    let deadline = Instant::now() + Duration::from_secs(details.expires_in.min(900));
    let mut interval = Duration::from_secs(details.interval.unwrap_or(5).clamp(1, 30));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => bail!("Disc CLI login cancelled."),
            _ = tokio::time::sleep(interval) => {}
        }
        if Instant::now() >= deadline {
            bail!("Device authorization expired.");
        }
        let response = client
            .post(&metadata.token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", details.device_code.as_str()),
                ("code_verifier", verifier.secret()),
            ])
            .send()
            .await
            .context("Device token polling failed.")?;
        let status = response.status();
        let bytes = read_bounded_response(response).await?;
        if status.is_success() {
            return validate_token_response(
                serde_json::from_slice(&bytes)
                    .context("Identity provider returned an invalid token response.")?,
            );
        }
        let error: OAuthErrorResponse = serde_json::from_slice(&bytes)
            .context("Identity provider returned an invalid device error.")?;
        match device_poll_action(&error.error, interval)? {
            DevicePollAction::Continue(next_interval) => interval = next_interval,
        }
    }
}

fn device_poll_action(error: &str, interval: Duration) -> Result<DevicePollAction> {
    match error {
        "authorization_pending" => Ok(DevicePollAction::Continue(interval)),
        "slow_down" => Ok(DevicePollAction::Continue(
            (interval + Duration::from_secs(5)).min(Duration::from_secs(30)),
        )),
        "access_denied" => bail!("Device authorization was denied."),
        "expired_token" => bail!("Device authorization expired."),
        code => bail!(
            "Device authorization failed ({}).",
            safe_oauth_error_code(code)
        ),
    }
}

async fn choose_subject(
    client: &reqwest::Client,
    api_base_url: &str,
    access_token: &str,
    requested_subject: Option<&str>,
) -> Result<(SessionResponse, SessionSubject)> {
    let response = client
        .get(format!("{}/session", api_base_url.trim_end_matches('/')))
        .bearer_auth(access_token)
        .send()
        .await
        .context("Failed to load Disc subjects.")?
        .error_for_status()
        .context("Disc rejected the OAuth session.")?;
    let session: SessionResponse = serde_json::from_slice(&read_bounded_response(response).await?)
        .context("Disc returned invalid session metadata.")?;
    let eligible = eligible_subjects(&session.subjects);
    if eligible.is_empty() {
        bail!("This account has no eligible Disc product subject.");
    }
    let selected = if let Some(selector) = requested_subject {
        match_requested_subject(&eligible, selector)?
    } else if eligible.len() == 1 {
        eligible[0].clone()
    } else {
        if !io::stdin().is_terminal() {
            bail!("Multiple subjects are eligible; pass --subject in a non-interactive terminal.");
        }
        let labels: Vec<_> = eligible
            .iter()
            .map(|subject| format!("{} ({})", subject.display_name, subject.subject_key))
            .collect();
        let index = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Choose a Disc subject")
            .items(&labels)
            .default(0)
            .interact()
            .context("Failed to read subject selection.")?;
        eligible[index].clone()
    };
    Ok((session, selected))
}

fn eligible_subjects(subjects: &[SessionSubject]) -> Vec<SessionSubject> {
    subjects
        .iter()
        .filter(|subject| {
            subject.selectable && subject.capabilities.iter().any(|value| value == "PRODUCT")
        })
        .cloned()
        .collect()
}

fn match_requested_subject(eligible: &[SessionSubject], selector: &str) -> Result<SessionSubject> {
    let matches: Vec<_> = eligible
        .iter()
        .filter(|subject| subject.subject_id == selector || subject.subject_key == selector)
        .cloned()
        .collect();
    if matches.len() != 1 {
        bail!("Subject selector must match exactly one eligible subject.");
    }
    Ok(matches[0].clone())
}

async fn create_subject_context(
    client: &reqwest::Client,
    api_base_url: &str,
    access_token: &str,
    subject: &SessionSubject,
) -> Result<SubjectContextResponse> {
    let subject_id = subject
        .subject_id
        .parse::<u64>()
        .context("Disc returned an invalid subject id.")?;
    let response = client
        .post(format!(
            "{}/session/subject-context",
            api_base_url.trim_end_matches('/')
        ))
        .bearer_auth(access_token)
        .json(&SubjectContextRequest {
            operation_id: Uuid::new_v4().to_string(),
            subject_id,
            previous_context_token: None,
        })
        .send()
        .await
        .context("Failed to create Disc subject context.")?
        .error_for_status()
        .context("Disc rejected the selected subject.")?;
    serde_json::from_slice(&read_bounded_response(response).await?)
        .context("Disc returned invalid subject-context metadata.")
}

fn profile_name(subject: &SessionSubject, api_base_url: &str) -> Result<String> {
    let parsed_url = Url::parse(api_base_url).context("Disc API base URL is invalid.")?;
    let host = parsed_url
        .host_str()
        .context("Disc API base URL must include a host.")?;
    Ok(format!(
        "{}@{}#{host}",
        subject.subject_key, subject.subject_id
    ))
}

pub struct LoginOptions {
    pub api_base_url: Option<String>,
    pub issuer: Option<String>,
    pub oauth_client_id: Option<String>,
    pub requested_profile: Option<String>,
    pub requested_subject: Option<String>,
    pub device: bool,
    pub no_browser: bool,
}

pub async fn login(store: &ConfigStore, options: LoginOptions) -> Result<()> {
    login_with_store(store, options, &SystemCredentialStore).await
}

async fn login_with_store(
    store: &ConfigStore,
    options: LoginOptions,
    credential_store: &dyn CredentialStore,
) -> Result<()> {
    let LoginOptions {
        api_base_url,
        issuer,
        oauth_client_id,
        requested_profile,
        requested_subject,
        device,
        no_browser,
    } = options;
    let api_base_url =
        validate_api_base_url(api_base_url.as_deref().unwrap_or(DEFAULT_API_BASE_URL))?;
    let issuer = issuer.unwrap_or_else(|| DEFAULT_ISSUER.to_owned());
    let client_id = oauth_client_id.unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned());
    let client = http_client()?;
    let metadata = discover(&client, &issuer).await?;
    let tokens = if device {
        login_with_device(&client, &metadata, &client_id, no_browser).await?
    } else {
        login_with_pkce(&client, &metadata, &client_id, no_browser).await?
    };
    let (session, selected) = choose_subject(
        &client,
        &api_base_url,
        tokens.access_token.expose_secret(),
        requested_subject.as_deref(),
    )
    .await?;
    let context = create_subject_context(
        &client,
        &api_base_url,
        tokens.access_token.expose_secret(),
        &selected,
    )
    .await?;
    if context.subject.subject_id != selected.subject_id {
        bail!("Disc returned a subject context for a different subject.");
    }
    let profile = requested_profile.unwrap_or(profile_name(&selected, &api_base_url)?);
    let credential_account = format!(
        "{}:{}:{}:{}",
        URL_SAFE_NO_PAD.encode(metadata.issuer.as_bytes()),
        client_id,
        session.user_id,
        selected.subject_id
    );
    credential_store
        .set_refresh_token(&credential_account, tokens.refresh_token.expose_secret())?;
    let save_result = store.save_profile(StoredAuthProfile {
        profile: profile.clone(),
        api_key: String::new(),
        api_base_url,
        subject_id: Some(selected.subject_id),
        subject_key: Some(selected.subject_key),
        subject_kind: Some(selected.subject_kind),
        display_name: Some(selected.display_name.clone()),
        created_at: Some(Utc::now().to_rfc3339()),
        issuer: Some(metadata.issuer),
        oauth_client_id: Some(client_id),
        keycloak_user_id: Some(session.user_id),
        credential_store_account: Some(credential_account.clone()),
    });
    if let Err(error) = save_result {
        credential_store
            .delete_refresh_token(&credential_account)
            .context("Failed to roll back OAuth credentials after profile persistence failed.")?;
        return Err(error);
    }
    println!(
        "Authenticated for {}. Active profile: {profile}.",
        selected.display_name
    );
    Ok(())
}

async fn refresh_with_store(
    profile: &StoredAuthProfile,
    store: &dyn CredentialStore,
    config_store: &ConfigStore,
) -> Result<TokenSet> {
    let issuer = profile
        .issuer
        .as_deref()
        .context("OAuth profile is missing issuer metadata.")?;
    let client_id = profile
        .oauth_client_id
        .as_deref()
        .context("OAuth profile is missing client metadata.")?;
    let account = profile
        .credential_store_account
        .as_deref()
        .context("OAuth profile is missing its credential-store reference.")?;
    let lock_path = config_store.credential_lock_path(account)?;
    let _lock = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .context({
                format!(
                    "Failed to open OAuth refresh lock at {}.",
                    lock_path.display()
                )
            })?;
        file.lock_exclusive().context({
            format!(
                "Failed to acquire OAuth refresh lock at {}.",
                lock_path.display()
            )
        })?;
        Ok(file)
    })
    .await
    .context("OAuth refresh-lock task failed.")??;
    let refresh_token = store.get_refresh_token(account)?;
    let client = http_client()?;
    let metadata = discover(&client, issuer).await?;
    let tokens = exchange_form(
        &client,
        &metadata.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", &refresh_token),
        ],
    )
    .await?;
    store
        .set_refresh_token(account, tokens.refresh_token.expose_secret())
        .context(
            "Failed to persist the rotated OAuth refresh token; run `disc auth login` again.",
        )?;
    Ok(tokens)
}

pub struct RuntimeOauth {
    pub access_token: SecretString,
    pub subject_context_token: SecretString,
}

pub async fn runtime_oauth(profile: &StoredAuthProfile) -> Result<RuntimeOauth> {
    let config_store = ConfigStore::discover()?;
    runtime_oauth_with_store(profile, &SystemCredentialStore, &config_store).await
}

async fn runtime_oauth_with_store(
    profile: &StoredAuthProfile,
    store: &dyn CredentialStore,
    config_store: &ConfigStore,
) -> Result<RuntimeOauth> {
    let tokens = refresh_with_store(profile, store, config_store).await?;
    let subject_id = profile
        .subject_id
        .as_deref()
        .context("OAuth profile has no selected subject.")?;
    let subject = SessionSubject {
        subject_id: subject_id.to_owned(),
        subject_key: profile
            .subject_key
            .clone()
            .context("OAuth profile is missing the subject key.")?,
        subject_kind: profile
            .subject_kind
            .clone()
            .context("OAuth profile is missing the subject kind.")?,
        display_name: profile
            .display_name
            .clone()
            .unwrap_or_else(|| subject_id.to_owned()),
        selectable: true,
        capabilities: vec!["PRODUCT".to_owned()],
    };
    let client = http_client()?;
    let context = create_subject_context(
        &client,
        &profile.api_base_url,
        tokens.access_token.expose_secret(),
        &subject,
    )
    .await?;
    Ok(RuntimeOauth {
        access_token: tokens.access_token,
        subject_context_token: SecretString::from(context.context_token),
    })
}

pub async fn logout(profile: &StoredAuthProfile) -> Result<()> {
    logout_with_store(profile, &SystemCredentialStore).await
}

async fn logout_with_store(profile: &StoredAuthProfile, store: &dyn CredentialStore) -> Result<()> {
    let Some(account) = profile.credential_store_account.as_deref() else {
        return Ok(());
    };
    let refresh_token = store.get_refresh_token(account)?;
    let issuer = profile
        .issuer
        .as_deref()
        .context("OAuth profile is missing issuer metadata.")?;
    let client_id = profile
        .oauth_client_id
        .as_deref()
        .context("OAuth profile is missing client metadata.")?;
    let client = http_client()?;
    let metadata = discover(&client, issuer).await?;
    let endpoint = metadata
        .revocation_endpoint
        .context("The identity provider does not advertise token revocation; local credentials were preserved.")?;
    let response = client
        .post(endpoint)
        .form(&[
            ("client_id", client_id),
            ("token", refresh_token.as_str()),
            ("token_type_hint", "refresh_token"),
        ])
        .send()
        .await
        .context("Failed to revoke the Disc OAuth session; local credentials were preserved.")?;
    if response.status() != StatusCode::OK && response.status() != StatusCode::NO_CONTENT {
        bail!(
            "The identity provider rejected OAuth session revocation; local credentials were preserved."
        );
    }
    store.delete_refresh_token(account)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use reqwest::Url;
    use secrecy::ExposeSecret;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::{
        DevicePollAction, LoginOptions, RawTokenResponse, SessionSubject, choose_subject,
        create_subject_context, device_poll_action, discover, eligible_subjects, exchange_form,
        http_client, login_with_device, login_with_pkce_using, login_with_store, logout_with_store,
        match_requested_subject, parse_callback_target, profile_name, read_bounded_response,
        refresh_with_store, runtime_oauth_with_store, safe_oauth_error_code, validate_api_base_url,
        validate_token_response, wait_for_callback,
    };
    use crate::config::{ConfigStore, StoredAuthProfile};
    use crate::credential_store::CredentialStore;

    #[derive(Default)]
    struct MemoryCredentialStore {
        fail_writes: bool,
        values: Mutex<HashMap<String, String>>,
    }

    impl CredentialStore for MemoryCredentialStore {
        fn get_refresh_token(&self, account: &str) -> anyhow::Result<String> {
            self.values
                .lock()
                .expect("credential lock")
                .get(account)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing refresh token"))
        }

        fn set_refresh_token(&self, account: &str, refresh_token: &str) -> anyhow::Result<()> {
            if self.fail_writes {
                anyhow::bail!("simulated credential-store failure");
            }
            self.values
                .lock()
                .expect("credential lock")
                .insert(account.to_owned(), refresh_token.to_owned());
            Ok(())
        }

        fn delete_refresh_token(&self, account: &str) -> anyhow::Result<()> {
            self.values.lock().expect("credential lock").remove(account);
            Ok(())
        }
    }

    fn subject(id: &str, key: &str, selectable: bool, capabilities: &[&str]) -> SessionSubject {
        SessionSubject {
            subject_id: id.to_owned(),
            subject_key: key.to_owned(),
            subject_kind: "partner".to_owned(),
            display_name: key.to_owned(),
            selectable,
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    async fn one_shot_server(response: String) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let origin = format!("http://{}", listener.local_addr().expect("server address"));
        let response = Arc::new(response);
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 16 * 1024];
            let count = stream.read(&mut request).await.expect("read request");
            request.truncate(count);
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            request
        });
        (origin, handle)
    }

    #[test]
    fn token_response_requires_bearer_refresh_and_positive_expiry() {
        assert!(
            validate_token_response(RawTokenResponse {
                access_token: "access".to_owned(),
                refresh_token: Some("refresh".to_owned()),
                expires_in: 300,
                token_type: "Bearer".to_owned(),
            })
            .is_ok()
        );
        assert!(
            validate_token_response(RawTokenResponse {
                access_token: "access".to_owned(),
                refresh_token: None,
                expires_in: 300,
                token_type: "Bearer".to_owned(),
            })
            .is_err()
        );
        assert!(
            validate_token_response(RawTokenResponse {
                access_token: String::new(),
                refresh_token: Some("refresh".to_owned()),
                expires_in: 300,
                token_type: "Bearer".to_owned(),
            })
            .is_err()
        );
        assert!(
            validate_token_response(RawTokenResponse {
                access_token: "access".to_owned(),
                refresh_token: Some(String::new()),
                expires_in: 300,
                token_type: "Bearer".to_owned(),
            })
            .is_err()
        );
        assert!(
            validate_token_response(RawTokenResponse {
                access_token: "access".to_owned(),
                refresh_token: Some("refresh".to_owned()),
                expires_in: 0,
                token_type: "Bearer".to_owned(),
            })
            .is_err()
        );
        assert!(
            validate_token_response(RawTokenResponse {
                access_token: "access".to_owned(),
                refresh_token: Some("refresh".to_owned()),
                expires_in: 300,
                token_type: "mac".to_owned(),
            })
            .is_err()
        );
    }

    #[test]
    fn callback_parser_requires_exact_path_state_and_one_result() {
        assert_eq!(
            parse_callback_target("/oauth/callback?code=one&state=expected", "expected")
                .expect("valid callback"),
            (Some("one".to_owned()), None)
        );
        assert!(parse_callback_target("/wrong?code=one&state=expected", "expected").is_err());
        assert!(parse_callback_target("/oauth/callback?code=one&state=wrong", "expected").is_err());
        assert!(
            parse_callback_target(
                "/oauth/callback?code=one&code=two&state=expected",
                "expected"
            )
            .is_err()
        );
        assert!(
            parse_callback_target(
                "/oauth/callback?code=one&error=denied&state=expected",
                "expected"
            )
            .is_err()
        );
        assert!(
            parse_callback_target(
                "/oauth/callback?code=one&state=expected#fragment",
                "expected"
            )
            .is_err()
        );
        assert!(
            parse_callback_target(
                "/oauth/callback?code=one&state=expected&state=expected",
                "expected"
            )
            .is_err()
        );
        assert!(parse_callback_target("/oauth/callback?state=expected", "expected").is_err());
        assert!(
            parse_callback_target(
                "/oauth/callback?error=access_denied&state=expected",
                "expected"
            )
            .expect("valid denial")
            .1
            .is_some()
        );
    }

    #[test]
    fn api_base_url_rejects_unsafe_or_non_origin_values() {
        assert_eq!(
            validate_api_base_url("https://api.disc.tech/").expect("valid production URL"),
            "https://api.disc.tech"
        );
        assert!(validate_api_base_url("http://127.0.0.1:6000").is_ok());
        for invalid in [
            "http://api.disc.tech",
            "http://localhost:6000",
            "https://user:pass@api.disc.tech",
            "https://api.disc.tech/path",
            "https://api.disc.tech?query=1",
            "https://api.disc.tech/#fragment",
        ] {
            assert!(validate_api_base_url(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn subject_eligibility_and_selection_are_strict_and_unambiguous() {
        let subjects = vec![
            subject("1", "eligible", true, &["PRODUCT"]),
            subject("2", "not-selectable", false, &["PRODUCT"]),
            subject("3", "wrong-capability", true, &["BILLING"]),
            subject("eligible", "ambiguous-id", true, &["PRODUCT"]),
        ];
        let eligible = eligible_subjects(&subjects);
        assert_eq!(eligible.len(), 2);
        assert_eq!(
            match_requested_subject(&eligible, "1")
                .expect("select by id")
                .subject_key,
            "eligible"
        );
        assert_eq!(
            match_requested_subject(&eligible, "ambiguous-id")
                .expect("select by key")
                .subject_id,
            "eligible"
        );
        assert!(match_requested_subject(&eligible, "eligible").is_err());
        assert!(match_requested_subject(&eligible, "missing").is_err());
        assert!(match_requested_subject(&[], "missing").is_err());
    }

    #[tokio::test]
    async fn subject_loading_selects_one_or_an_explicit_eligible_subject_and_rejects_empty_sets() {
        let client = http_client().expect("client");
        let single_body = r#"{"userId":"user-42","subjects":[{"subjectId":"42","subjectKey":"partner","subjectKind":"PARTNER","displayName":"Partner","selectable":true,"capabilities":["PRODUCT"]}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{single_body}",
            single_body.len()
        );
        let (origin, server) = one_shot_server(response).await;
        let (session, selected) = choose_subject(&client, &origin, "access-secret", None)
            .await
            .expect("single eligible subject");
        assert_eq!(session.user_id, "user-42");
        assert_eq!(selected.subject_id, "42");
        server.await.expect("server");

        let multiple_body = r#"{"userId":"user-42","subjects":[{"subjectId":"42","subjectKey":"partner","subjectKind":"PARTNER","displayName":"Partner","selectable":true,"capabilities":["PRODUCT"]},{"subjectId":"84","subjectKey":"brand","subjectKind":"BRAND","displayName":"Brand","selectable":true,"capabilities":["PRODUCT"]}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{multiple_body}",
            multiple_body.len()
        );
        let (origin, server) = one_shot_server(response).await;
        let (_, selected) = choose_subject(&client, &origin, "access-secret", Some("brand"))
            .await
            .expect("explicit subject");
        assert_eq!(selected.subject_id, "84");
        server.await.expect("server");

        let empty_body = r#"{"userId":"user-42","subjects":[{"subjectId":"42","subjectKey":"partner","subjectKind":"PARTNER","displayName":"Partner","selectable":false,"capabilities":["PRODUCT"]}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{empty_body}",
            empty_body.len()
        );
        let (origin, server) = one_shot_server(response).await;
        assert!(
            choose_subject(&client, &origin, "access-secret", None)
                .await
                .expect_err("no eligible subjects")
                .to_string()
                .contains("no eligible")
        );
        server.await.expect("server");
    }

    #[tokio::test]
    async fn subject_context_validates_numeric_ids_and_profile_names_include_origin_identity() {
        let invalid = subject("not-a-number", "partner", true, &["PRODUCT"]);
        assert!(
            create_subject_context(
                &http_client().expect("client"),
                "http://127.0.0.1:9",
                "access-secret",
                &invalid,
            )
            .await
            .expect_err("invalid subject id")
            .to_string()
            .contains("invalid subject id")
        );

        let valid = subject("42", "partner", true, &["PRODUCT"]);
        assert_eq!(
            profile_name(&valid, "https://api.disc.tech").expect("profile name"),
            "partner@42#api.disc.tech"
        );
        assert!(profile_name(&valid, "not a URL").is_err());
    }

    #[test]
    fn device_polling_implements_pending_slow_down_denial_and_expiry_without_provider_text() {
        assert_eq!(
            device_poll_action("authorization_pending", std::time::Duration::from_secs(5))
                .expect("pending"),
            DevicePollAction::Continue(std::time::Duration::from_secs(5))
        );
        assert_eq!(
            device_poll_action("slow_down", std::time::Duration::from_secs(5)).expect("slow down"),
            DevicePollAction::Continue(std::time::Duration::from_secs(10))
        );
        assert_eq!(
            device_poll_action("slow_down", std::time::Duration::from_secs(30))
                .expect("bounded slow down"),
            DevicePollAction::Continue(std::time::Duration::from_secs(30))
        );
        assert!(
            device_poll_action("access_denied", std::time::Duration::from_secs(5))
                .expect_err("denied")
                .to_string()
                .contains("denied")
        );
        assert!(
            device_poll_action("expired_token", std::time::Duration::from_secs(5))
                .expect_err("expired")
                .to_string()
                .contains("expired")
        );
        let unknown = device_poll_action(
            "server_error_secret-description",
            std::time::Duration::from_secs(5),
        )
        .expect_err("unknown")
        .to_string();
        assert!(unknown.contains("server_error_secret-description"));
        assert_eq!(safe_oauth_error_code("invalid_grant"), "invalid_grant");
        assert_eq!(safe_oauth_error_code("secret leaked!"), "unknown_error");
        assert_eq!(safe_oauth_error_code(&"a".repeat(65)), "unknown_error");
    }

    #[tokio::test]
    async fn discovery_accepts_same_origin_loopback_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let document = serde_json::json!({
            "issuer": origin,
            "authorization_endpoint": format!("{origin}/authorize"),
            "token_endpoint": format!("{origin}/token"),
            "device_authorization_endpoint": format!("{origin}/device"),
            "revocation_endpoint": format!("{origin}/revoke")
        })
        .to_string();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.expect("read");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{document}",
                document.len()
            );
            stream.write_all(response.as_bytes()).await.expect("write");
        });
        let metadata = discover(&http_client().expect("client"), &origin)
            .await
            .expect("valid discovery");
        assert_eq!(metadata.issuer, origin);
        task.await.expect("server");
    }

    #[tokio::test]
    async fn discovery_rejects_issuer_and_endpoint_substitution() {
        for document in [
            serde_json::json!({
                "issuer": "http://127.0.0.1:9/other",
                "authorization_endpoint": "http://127.0.0.1:9/authorize",
                "token_endpoint": "http://127.0.0.1:9/token"
            }),
            serde_json::json!({
                "issuer": "__ORIGIN__",
                "authorization_endpoint": "https://evil.example/authorize",
                "token_endpoint": "__ORIGIN__/token"
            }),
        ] {
            let template = document.to_string();
            let (origin, server) = one_shot_server(String::new()).await;
            server.abort();
            let document = template.replace("__ORIGIN__", &origin);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{document}",
                document.len()
            );
            let (actual_origin, server) = one_shot_server(response).await;
            let issuer = if template.contains("__ORIGIN__") {
                actual_origin.clone()
            } else {
                actual_origin
            };
            assert!(
                discover(&http_client().expect("client"), &issuer)
                    .await
                    .is_err()
            );
            server.await.expect("server");
        }
    }

    #[tokio::test]
    async fn token_exchange_redacts_provider_descriptions_and_secrets() {
        let body = r#"{"error":"invalid_grant","error_description":"refresh-secret leaked"}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (origin, server) = one_shot_server(response).await;
        let error = exchange_form(
            &http_client().expect("client"),
            &format!("{origin}/token"),
            &[("refresh_token", "refresh-secret")],
        )
        .await
        .expect_err("exchange must fail")
        .to_string();
        assert!(error.contains("invalid_grant"));
        assert!(!error.contains("refresh-secret"));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn bounded_responses_reject_oversized_bodies_before_or_after_streaming() {
        let header_response =
            "HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\nConnection: close\r\n\r\n";
        let (origin, server) = one_shot_server(header_response.to_owned()).await;
        let response = http_client()
            .expect("client")
            .get(origin)
            .send()
            .await
            .expect("response");
        assert!(
            read_bounded_response(response)
                .await
                .expect_err("oversized declared body")
                .to_string()
                .contains("maximum allowed size")
        );
        server.await.expect("server");

        let body = "x".repeat(super::MAX_HTTP_BODY_BYTES + 1);
        let response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        );
        let (origin, server) = one_shot_server(response).await;
        let response = http_client()
            .expect("client")
            .get(origin)
            .send()
            .await
            .expect("response");
        assert!(
            read_bounded_response(response)
                .await
                .expect_err("oversized streamed body")
                .to_string()
                .contains("maximum allowed size")
        );
        server.await.expect("server");
    }

    #[tokio::test]
    async fn device_login_reports_missing_support_and_provider_denial_without_secret_text() {
        let client = http_client().expect("client");
        let unsupported = super::ProviderMetadata {
            issuer: "http://127.0.0.1:9".to_owned(),
            authorization_endpoint: "http://127.0.0.1:9/authorize".to_owned(),
            token_endpoint: "http://127.0.0.1:9/token".to_owned(),
            device_authorization_endpoint: None,
            revocation_endpoint: None,
        };
        assert!(
            login_with_device(&client, &unsupported, "disc-cli", true)
                .await
                .expect_err("missing device endpoint")
                .to_string()
                .contains("does not advertise")
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let endpoint = format!("{origin}/device");
        let token_endpoint = format!("{origin}/token");
        let server = tokio::spawn(async move {
            for request_index in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).await.expect("read");
                let (status, body) = if request_index == 0 {
                    (
                        "200 OK",
                        r#"{"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://example.invalid/device","expires_in":60,"interval":1}"#,
                    )
                } else {
                    (
                        "400 Bad Request",
                        r#"{"error":"access_denied","error_description":"provider-secret"}"#,
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
        });
        let metadata = super::ProviderMetadata {
            issuer: origin,
            authorization_endpoint: "http://127.0.0.1:9/authorize".to_owned(),
            token_endpoint,
            device_authorization_endpoint: Some(endpoint),
            revocation_endpoint: None,
        };
        let error = login_with_device(&client, &metadata, "disc-cli", true)
            .await
            .expect_err("device denial")
            .to_string();
        assert!(error.contains("denied"));
        assert!(!error.contains("provider-secret"));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn refresh_rotation_reads_and_atomically_replaces_the_keyring_secret() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let server_origin = origin.clone();
        let server = tokio::spawn(async move {
            for request_index in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = vec![0_u8; 16 * 1024];
                let count = stream.read(&mut request).await.expect("read");
                request.truncate(count);
                let request_text = String::from_utf8(request).expect("HTTP request");
                let body = if request_index == 0 {
                    serde_json::json!({
                        "issuer": server_origin,
                        "authorization_endpoint": format!("{server_origin}/authorize"),
                        "token_endpoint": format!("{server_origin}/token"),
                        "revocation_endpoint": format!("{server_origin}/revoke")
                    })
                    .to_string()
                } else {
                    assert!(request_text.contains("refresh_token=old-refresh"));
                    assert!(request_text.contains("client_id=disc-cli"));
                    r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":300,"token_type":"Bearer"}"#
                        .to_owned()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
        });
        let credentials = MemoryCredentialStore::default();
        credentials
            .set_refresh_token("credential-account", "old-refresh")
            .expect("seed refresh token");
        let config_dir =
            std::env::temp_dir().join(format!("disc-refresh-test-{}", uuid::Uuid::new_v4()));
        let config = ConfigStore::at_root(config_dir.clone());
        let profile = StoredAuthProfile {
            profile: "test".to_owned(),
            api_key: String::new(),
            api_base_url: "http://127.0.0.1:3001".to_owned(),
            subject_id: Some("42".to_owned()),
            subject_key: Some("subject".to_owned()),
            subject_kind: Some("partner".to_owned()),
            display_name: Some("Subject".to_owned()),
            created_at: None,
            issuer: Some(origin),
            oauth_client_id: Some("disc-cli".to_owned()),
            keycloak_user_id: Some("user".to_owned()),
            credential_store_account: Some("credential-account".to_owned()),
        };

        let tokens = refresh_with_store(&profile, &credentials, &config)
            .await
            .expect("refresh");
        assert_eq!(tokens.access_token.expose_secret(), "new-access");
        assert_eq!(
            credentials
                .get_refresh_token("credential-account")
                .expect("rotated refresh"),
            "new-refresh"
        );
        server.await.expect("server");
        std::fs::remove_dir_all(config_dir).expect("remove test config");
    }

    #[tokio::test]
    async fn device_login_persists_one_subject_profile_and_only_a_keyring_reference() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let server_origin = origin.clone();
        let server = tokio::spawn(async move {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = vec![0_u8; 16 * 1024];
                let count = stream.read(&mut request).await.expect("read");
                request.truncate(count);
                let request = String::from_utf8(request).expect("HTTP request");
                let first_line = request.lines().next().expect("request line");
                let body = if first_line.contains("/.well-known/openid-configuration") {
                    serde_json::json!({
                        "issuer": server_origin,
                        "authorization_endpoint": format!("{server_origin}/authorize"),
                        "token_endpoint": format!("{server_origin}/token"),
                        "device_authorization_endpoint": format!("{server_origin}/device"),
                        "revocation_endpoint": format!("{server_origin}/revoke")
                    })
                    .to_string()
                } else if first_line.contains("POST /device ") {
                    assert!(request.contains("scope=disc-cli"));
                    assert!(request.contains("code_challenge="));
                    assert!(request.contains("code_challenge_method=S256"));
                    r#"{"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://example.invalid/device","expires_in":60,"interval":1}"#.to_owned()
                } else if first_line.contains("POST /token ") {
                    assert!(request.contains("device_code=device-secret"));
                    assert!(request.contains("code_verifier="));
                    r#"{"access_token":"access-secret","refresh_token":"refresh-secret","expires_in":300,"token_type":"Bearer"}"#.to_owned()
                } else if first_line.contains("GET /session ") {
                    assert!(request.contains("authorization: Bearer access-secret"));
                    r#"{"userId":"user-42","subjects":[{"subjectId":"42","subjectKey":"partner","subjectKind":"PARTNER","displayName":"Partner","selectable":true,"capabilities":["PRODUCT"]}]}"#.to_owned()
                } else if first_line.contains("POST /session/subject-context ") {
                    assert!(request.contains("\"subjectId\":42"));
                    r#"{"contextToken":"context-secret","subject":{"subjectId":"42","subjectKey":"partner","subjectKind":"PARTNER","displayName":"Partner","selectable":true,"capabilities":["PRODUCT"]}}"#.to_owned()
                } else {
                    panic!("unexpected request: {first_line}");
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
        });
        let config_dir =
            std::env::temp_dir().join(format!("disc-login-test-{}", uuid::Uuid::new_v4()));
        let config = ConfigStore::at_root(config_dir.clone());
        let credentials = MemoryCredentialStore::default();
        login_with_store(
            &config,
            LoginOptions {
                api_base_url: Some(origin.clone()),
                issuer: Some(origin),
                oauth_client_id: Some("disc-cli".to_owned()),
                requested_profile: Some("partner-profile".to_owned()),
                requested_subject: Some("42".to_owned()),
                device: true,
                no_browser: true,
            },
            &credentials,
        )
        .await
        .expect("device login");

        let auth = config.load_auth().expect("load auth").expect("auth");
        let profile = &auth.profiles["partner-profile"];
        assert!(profile.api_key.is_empty());
        assert_eq!(profile.subject_id.as_deref(), Some("42"));
        let account = profile
            .credential_store_account
            .as_deref()
            .expect("credential reference");
        assert_eq!(
            credentials
                .get_refresh_token(account)
                .expect("refresh token"),
            "refresh-secret"
        );
        let metadata =
            std::fs::read_to_string(config_dir.join("auth.json")).expect("auth metadata");
        assert!(!metadata.contains("access-secret"));
        assert!(!metadata.contains("refresh-secret"));
        server.await.expect("server");
        std::fs::remove_dir_all(config_dir).expect("remove test config");
    }

    #[tokio::test]
    async fn callback_listener_enforces_host_and_returns_only_a_valid_code() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind callback");
        let address = listener.local_addr().expect("address");
        let callback = tokio::spawn(wait_for_callback(listener, "state"));
        let mut stream = TcpStream::connect(address).await.expect("connect");
        stream
            .write_all(
                format!(
                    "GET /oauth/callback?code=secret-code&state=state HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                    address.port()
                )
                .as_bytes(),
            )
            .await
            .expect("write callback");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("response");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Content-Type: text/html; charset=utf-8"));
        assert!(response.contains("Content-Security-Policy: default-src 'none'"));
        assert!(response.contains("<h1 id=\"callback-title\">CLI connected</h1>"));
        assert!(response.contains("--disc-color-bg-default: #08090a"));
        assert!(response.contains("width: min(100%, 32rem)"));
        assert!(!response.contains("secret-code"));
        assert_eq!(
            callback
                .await
                .expect("callback task")
                .expect("valid callback"),
            "secret-code"
        );
    }

    #[tokio::test]
    async fn callback_listener_rejects_host_header_injection() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind callback");
        let address = listener.local_addr().expect("address");
        let callback = tokio::spawn(wait_for_callback(listener, "state"));
        let mut stream = TcpStream::connect(address).await.expect("connect");
        stream
            .write_all(
                format!(
                    "GET /oauth/callback?code=secret-code&state=state HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nHost: evil.example\r\n\r\n",
                    address.port()
                )
                .as_bytes(),
            )
            .await
            .expect("write callback");
        assert!(callback.await.expect("callback task").is_err());

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind denial callback");
        let address = listener.local_addr().expect("address");
        let callback = tokio::spawn(wait_for_callback(listener, "state"));
        let mut stream = TcpStream::connect(address).await.expect("connect");
        stream
            .write_all(
                format!(
                    "GET /oauth/callback?error=access_denied&state=state HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                    address.port()
                )
                .as_bytes(),
            )
            .await
            .expect("write denial callback");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("denial response");
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("<h1 id=\"callback-title\">Authorization cancelled</h1>"));
        assert!(response.contains("class=\"content cancelled\""));
        assert!(!response.contains("access_denied"));
        assert!(
            callback
                .await
                .expect("callback task")
                .expect_err("denied callback")
                .to_string()
                .contains("access_denied")
        );

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind incomplete callback");
        let address = listener.local_addr().expect("address");
        let callback = tokio::spawn(wait_for_callback(listener, "state"));
        let mut stream = TcpStream::connect(address).await.expect("connect");
        stream
            .write_all(b"GET /oauth/callback?code=secret-code&state=state HTTP/1.1\r\n")
            .await
            .expect("write incomplete callback");
        drop(stream);
        assert!(
            callback
                .await
                .expect("callback task")
                .expect_err("incomplete callback")
                .to_string()
                .contains("before its headers were complete")
        );

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind oversized callback");
        let address = listener.local_addr().expect("address");
        let callback = tokio::spawn(wait_for_callback(listener, "state"));
        let mut stream = TcpStream::connect(address).await.expect("connect");
        stream
            .write_all(&vec![b'x'; 8192])
            .await
            .expect("write oversized callback");
        assert!(
            callback
                .await
                .expect("callback task")
                .expect_err("oversized callback")
                .to_string()
                .contains("too large")
        );
    }

    #[tokio::test]
    async fn pkce_login_binds_loopback_validates_state_and_exchanges_the_verifier() {
        let token_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind token endpoint");
        let origin = format!(
            "http://{}",
            token_listener.local_addr().expect("token address")
        );
        let metadata = super::ProviderMetadata {
            issuer: origin.clone(),
            authorization_endpoint: format!("{origin}/authorize"),
            token_endpoint: format!("{origin}/token"),
            device_authorization_endpoint: None,
            revocation_endpoint: None,
        };
        let token_server = tokio::spawn(async move {
            let (mut stream, _) = token_listener.accept().await.expect("accept token request");
            let mut request = vec![0_u8; 16 * 1024];
            let count = stream.read(&mut request).await.expect("read token request");
            request.truncate(count);
            let request = String::from_utf8(request).expect("HTTP request");
            assert!(request.starts_with("POST /token "));
            assert!(request.contains("grant_type=authorization_code"));
            assert!(request.contains("client_id=disc-cli"));
            assert!(request.contains("code=authorization-code"));
            assert!(request.contains("redirect_uri=http%3A%2F%2F127.0.0.1"));
            assert!(request.contains("code_verifier="));
            let body = r#"{"access_token":"access-secret","refresh_token":"refresh-secret","expires_in":300,"token_type":"Bearer"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.expect("write");
        });

        let tokens = login_with_pkce_using(
            &http_client().expect("client"),
            &metadata,
            "disc-cli",
            |authorization_url| {
                let query: HashMap<_, _> = authorization_url.query_pairs().into_owned().collect();
                assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
                assert_eq!(query.get("scope").map(String::as_str), Some("disc-cli"));
                assert_eq!(
                    query.get("code_challenge_method").map(String::as_str),
                    Some("S256")
                );
                assert!(
                    query
                        .get("code_challenge")
                        .is_some_and(|value| !value.is_empty())
                );
                let callback_url =
                    Url::parse(query.get("redirect_uri").expect("redirect URI")).expect("URL");
                let state = query.get("state").expect("state").clone();
                tokio::spawn(async move {
                    let address = format!(
                        "127.0.0.1:{}",
                        callback_url.port().expect("callback port")
                    );
                    let mut stream = TcpStream::connect(address).await.expect("connect callback");
                    stream
                        .write_all(
                            format!(
                                "GET {}?code=authorization-code&state={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                                callback_url.path(),
                                urlencoding::encode(&state),
                                callback_url.port().expect("callback port")
                            )
                            .as_bytes(),
                        )
                        .await
                        .expect("write callback");
                    let mut response = String::new();
                    stream
                        .read_to_string(&mut response)
                        .await
                        .expect("callback response");
                    assert!(response.starts_with("HTTP/1.1 200 OK"));
                });
                Ok(())
            },
        )
        .await
        .expect("PKCE login");
        assert_eq!(tokens.access_token.expose_secret(), "access-secret");
        assert_eq!(tokens.refresh_token.expose_secret(), "refresh-secret");
        token_server.await.expect("token server");
    }

    #[tokio::test]
    async fn runtime_oauth_rotates_refresh_and_creates_a_fresh_subject_context() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let server_origin = origin.clone();
        let server = tokio::spawn(async move {
            for request_index in 0..3 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = vec![0_u8; 16 * 1024];
                let count = stream.read(&mut request).await.expect("read");
                request.truncate(count);
                let request = String::from_utf8(request).expect("HTTP request");
                let body = match request_index {
                    0 => serde_json::json!({
                        "issuer": server_origin,
                        "authorization_endpoint": format!("{server_origin}/authorize"),
                        "token_endpoint": format!("{server_origin}/token"),
                        "revocation_endpoint": format!("{server_origin}/revoke")
                    })
                    .to_string(),
                    1 => {
                        assert!(request.contains("refresh_token=old-refresh"));
                        r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":300,"token_type":"Bearer"}"#.to_owned()
                    }
                    2 => {
                        assert!(request.starts_with("POST /session/subject-context "));
                        assert!(request.contains("authorization: Bearer fresh-access"));
                        assert!(request.contains("\"subjectId\":42"));
                        r#"{"contextToken":"fresh-context","subject":{"subjectId":"42","subjectKey":"partner","subjectKind":"PARTNER","displayName":"Partner","selectable":true,"capabilities":["PRODUCT"]}}"#.to_owned()
                    }
                    _ => unreachable!(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
        });
        let credentials = MemoryCredentialStore::default();
        credentials
            .set_refresh_token("credential-account", "old-refresh")
            .expect("seed refresh token");
        let config_dir =
            std::env::temp_dir().join(format!("disc-runtime-test-{}", uuid::Uuid::new_v4()));
        let config = ConfigStore::at_root(config_dir.clone());
        let profile = StoredAuthProfile {
            profile: "partner-profile".to_owned(),
            api_key: String::new(),
            api_base_url: origin.clone(),
            subject_id: Some("42".to_owned()),
            subject_key: Some("partner".to_owned()),
            subject_kind: Some("PARTNER".to_owned()),
            display_name: Some("Partner".to_owned()),
            created_at: None,
            issuer: Some(origin),
            oauth_client_id: Some("disc-cli".to_owned()),
            keycloak_user_id: Some("user-42".to_owned()),
            credential_store_account: Some("credential-account".to_owned()),
        };

        let runtime = runtime_oauth_with_store(&profile, &credentials, &config)
            .await
            .expect("runtime OAuth");
        assert_eq!(runtime.access_token.expose_secret(), "fresh-access");
        assert_eq!(
            runtime.subject_context_token.expose_secret(),
            "fresh-context"
        );
        assert_eq!(
            credentials
                .get_refresh_token("credential-account")
                .expect("rotated refresh"),
            "fresh-refresh"
        );
        server.await.expect("server");
        std::fs::remove_dir_all(config_dir).expect("remove test config");
    }

    #[tokio::test]
    async fn logout_revokes_remotely_before_deleting_the_local_refresh_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let server_origin = origin.clone();
        let server = tokio::spawn(async move {
            for request_index in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = vec![0_u8; 16 * 1024];
                let count = stream.read(&mut request).await.expect("read");
                request.truncate(count);
                let request = String::from_utf8(request).expect("HTTP request");
                let (status, body) = if request_index == 0 {
                    (
                        "200 OK",
                        serde_json::json!({
                            "issuer": server_origin,
                            "authorization_endpoint": format!("{server_origin}/authorize"),
                            "token_endpoint": format!("{server_origin}/token"),
                            "revocation_endpoint": format!("{server_origin}/revoke")
                        })
                        .to_string(),
                    )
                } else {
                    assert!(request.starts_with("POST /revoke "));
                    assert!(request.contains("client_id=disc-cli"));
                    assert!(request.contains("token=refresh-secret"));
                    assert!(request.contains("token_type_hint=refresh_token"));
                    ("204 No Content", String::new())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
        });
        let credentials = MemoryCredentialStore::default();
        credentials
            .set_refresh_token("credential-account", "refresh-secret")
            .expect("seed refresh token");
        let profile = StoredAuthProfile {
            profile: "partner-profile".to_owned(),
            api_key: String::new(),
            api_base_url: "https://api.disc.tech".to_owned(),
            subject_id: Some("42".to_owned()),
            subject_key: Some("partner".to_owned()),
            subject_kind: Some("PARTNER".to_owned()),
            display_name: Some("Partner".to_owned()),
            created_at: None,
            issuer: Some(origin),
            oauth_client_id: Some("disc-cli".to_owned()),
            keycloak_user_id: Some("user-42".to_owned()),
            credential_store_account: Some("credential-account".to_owned()),
        };

        logout_with_store(&profile, &credentials)
            .await
            .expect("logout");
        assert!(credentials.get_refresh_token("credential-account").is_err());
        server.await.expect("server");
    }

    #[tokio::test]
    async fn logout_preserves_local_credentials_when_remote_revocation_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let server_origin = origin.clone();
        let server = tokio::spawn(async move {
            for request_index in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).await.expect("read");
                let (status, body) = if request_index == 0 {
                    (
                        "200 OK",
                        serde_json::json!({
                            "issuer": server_origin,
                            "authorization_endpoint": format!("{server_origin}/authorize"),
                            "token_endpoint": format!("{server_origin}/token"),
                            "revocation_endpoint": format!("{server_origin}/revoke")
                        })
                        .to_string(),
                    )
                } else {
                    ("503 Service Unavailable", String::new())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
        });
        let credentials = MemoryCredentialStore::default();
        credentials
            .set_refresh_token("credential-account", "refresh-secret")
            .expect("seed refresh token");
        let profile = StoredAuthProfile {
            profile: "partner-profile".to_owned(),
            api_key: String::new(),
            api_base_url: "https://api.disc.tech".to_owned(),
            subject_id: Some("42".to_owned()),
            subject_key: Some("partner".to_owned()),
            subject_kind: Some("PARTNER".to_owned()),
            display_name: Some("Partner".to_owned()),
            created_at: None,
            issuer: Some(origin),
            oauth_client_id: Some("disc-cli".to_owned()),
            keycloak_user_id: Some("user-42".to_owned()),
            credential_store_account: Some("credential-account".to_owned()),
        };

        let error = logout_with_store(&profile, &credentials)
            .await
            .expect_err("revocation must fail")
            .to_string();
        assert!(error.contains("local credentials were preserved"));
        assert_eq!(
            credentials
                .get_refresh_token("credential-account")
                .expect("preserved refresh token"),
            "refresh-secret"
        );
        server.await.expect("server");
    }
}
