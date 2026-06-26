use super::openai_device::{
    DEFAULT_CLIENT_ID, DEFAULT_ISSUER, DeviceAuthHttp, DeviceAuthRuntime, DeviceCode, OAuthTokens,
    OpenAiDeviceAuthFlow, Result as DeviceAuthResult, account_id_from_access_token,
};
use super::store::{OpenAiAuthStore, OpenAiOAuthCredential};
use anyhow::Context;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// If OpenAI omits expires_in, assume a short-lived access token so future
// provider work refreshes early while keeping the persisted refresh token.
const FALLBACK_ACCESS_TOKEN_EXPIRES_IN: Duration = Duration::from_secs(5 * 60);
const BROWSER_CALLBACK_PORT: u16 = 1455;
const BROWSER_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const OPENAI_OAUTH_SCOPE: &str = "openid profile email offline_access";
const OPENAI_OAUTH_ORIGINATOR: &str = "dirge";
type DeviceCodeFuture<'a> = Pin<Box<dyn Future<Output = DeviceAuthResult<DeviceCode>> + Send + 'a>>;
type TokenFuture<'a> = Pin<Box<dyn Future<Output = DeviceAuthResult<OAuthTokens>> + Send + 'a>>;

pub(crate) trait OpenAiLoginFlow {
    fn request_device_code(&self) -> DeviceCodeFuture<'_>;

    fn complete_device_code_login(&self, device_code: DeviceCode) -> TokenFuture<'_>;
}

impl<H, R> OpenAiLoginFlow for OpenAiDeviceAuthFlow<H, R>
where
    H: DeviceAuthHttp,
    R: DeviceAuthRuntime,
{
    fn request_device_code(&self) -> DeviceCodeFuture<'_> {
        Box::pin(async move { OpenAiDeviceAuthFlow::request_device_code(self).await })
    }

    fn complete_device_code_login(&self, device_code: DeviceCode) -> TokenFuture<'_> {
        Box::pin(async move {
            OpenAiDeviceAuthFlow::complete_device_code_login(self, device_code).await
        })
    }
}

pub(crate) trait OpenAiCredentialStore {
    fn path(&self) -> &Path;

    fn save_openai(&self, credential: &OpenAiOAuthCredential) -> anyhow::Result<()>;
}

impl OpenAiCredentialStore for OpenAiAuthStore {
    fn path(&self) -> &Path {
        OpenAiAuthStore::path(self)
    }

    fn save_openai(&self, credential: &OpenAiOAuthCredential) -> anyhow::Result<()> {
        OpenAiAuthStore::save_openai(self, credential)?;
        Ok(())
    }
}

pub(crate) async fn run_auth_action(action: &crate::cli::AuthAction) -> anyhow::Result<()> {
    run_auth_action_with(action, login_openai).await
}

pub(crate) async fn run_auth_action_with<L, Fut>(
    action: &crate::cli::AuthAction,
    openai_login: L,
) -> anyhow::Result<()>
where
    L: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    match action {
        crate::cli::AuthAction::Openai { device_code } => {
            if *device_code {
                login_openai_device().await
            } else {
                openai_login().await
            }
        }
        crate::cli::AuthAction::Anthropic => {
            let path = crate::provider::anthropic_oauth::login_and_persist().await?;
            println!("Anthropic OAuth credentials saved to {}", path.display());
            Ok(())
        }
    }
}

pub(crate) async fn login_openai() -> anyhow::Result<()> {
    let store = OpenAiAuthStore::default();
    let mut stdout = std::io::stdout().lock();
    login_openai_browser_with_clock(store, current_epoch_ms, &mut stdout).await
}

pub(crate) async fn login_openai_device() -> anyhow::Result<()> {
    let flow = OpenAiDeviceAuthFlow::default();
    let store = OpenAiAuthStore::default();
    let mut stdout = std::io::stdout().lock();
    login_openai_with_clock(flow, store, current_epoch_ms, &mut stdout).await
}

async fn login_openai_browser_with_clock<S, W, N>(
    store: S,
    now_epoch_ms: N,
    stdout: &mut W,
) -> anyhow::Result<()>
where
    S: OpenAiCredentialStore,
    W: Write,
    N: FnOnce() -> anyhow::Result<i64>,
{
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = oauth_state();
    let authorize_url = openai_browser_authorize_url(&challenge, &state);
    let listener = TcpListener::bind(("127.0.0.1", BROWSER_CALLBACK_PORT)).with_context(|| {
        format!("failed to bind OpenAI OAuth callback port {BROWSER_CALLBACK_PORT}")
    })?;

    writeln!(stdout, "OpenAI browser login")?;
    writeln!(stdout, "Open this URL to authenticate with OpenAI:")?;
    writeln!(stdout)?;
    writeln!(stdout, "{authorize_url}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "Waiting for browser redirect on {BROWSER_REDIRECT_URI} ..."
    )?;

    let code = wait_for_browser_callback(listener, &state)?;
    let tokens = exchange_browser_authorization_code(&code, &verifier).await?;
    let credential = oauth_tokens_to_credential(tokens, now_epoch_ms()?);
    store.save_openai(&credential)?;

    writeln!(
        stdout,
        "OpenAI authorization saved to {}",
        store.path().display()
    )?;
    writeln!(
        stdout,
        "This login persists across Dirge sessions until you delete that file or OpenAI revokes it."
    )?;

    Ok(())
}

#[cfg(test)]
pub(crate) async fn login_openai_with<F, S, W>(
    flow: F,
    store: S,
    now_epoch_ms: i64,
    stdout: &mut W,
) -> anyhow::Result<()>
where
    F: OpenAiLoginFlow,
    S: OpenAiCredentialStore,
    W: Write,
{
    login_openai_with_clock(flow, store, || Ok(now_epoch_ms), stdout).await
}

async fn login_openai_with_clock<F, S, W, N>(
    flow: F,
    store: S,
    now_epoch_ms: N,
    stdout: &mut W,
) -> anyhow::Result<()>
where
    F: OpenAiLoginFlow,
    S: OpenAiCredentialStore,
    W: Write,
    N: FnOnce() -> anyhow::Result<i64>,
{
    let device_code = flow.request_device_code().await?;

    writeln!(stdout, "OpenAI device-code login")?;
    writeln!(stdout, "1. Open: {}", device_code.verification_url)?;
    writeln!(stdout, "2. Enter code: {}", device_code.user_code)?;
    writeln!(
        stdout,
        "Do not share this code. Anyone with it may be able to authorize Dirge as you."
    )?;
    writeln!(stdout, "Waiting for OpenAI authorization...")?;

    let tokens = flow.complete_device_code_login(device_code).await?;
    let credential = oauth_tokens_to_credential(tokens, now_epoch_ms()?);
    store.save_openai(&credential)?;

    writeln!(
        stdout,
        "OpenAI authorization saved to {}",
        store.path().display()
    )?;
    writeln!(
        stdout,
        "This login persists across Dirge sessions until you delete that file or OpenAI revokes it."
    )?;

    Ok(())
}

#[derive(serde::Deserialize)]
struct BrowserTokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(
        default,
        alias = "chatgpt_account_id",
        alias = "chatgptAccountId",
        alias = "chatgpt_account",
        alias = "accountId"
    )]
    account_id: Option<String>,
}

async fn exchange_browser_authorization_code(
    code: &str,
    verifier: &str,
) -> anyhow::Result<OAuthTokens> {
    let token_url = format!("{DEFAULT_ISSUER}/oauth/token");
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", DEFAULT_CLIENT_ID)
        .append_pair("code", code)
        .append_pair("code_verifier", verifier)
        .append_pair("redirect_uri", BROWSER_REDIRECT_URI)
        .finish();
    let response = reqwest::Client::new()
        .post(token_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "OpenAI browser OAuth token exchange failed with status {}{}",
            status.as_u16(),
            if body.trim().is_empty() {
                String::new()
            } else {
                format!(": {body}")
            }
        );
    }
    let token: BrowserTokenResponse =
        serde_json::from_str(&body).context("OpenAI browser OAuth token response was invalid")?;
    let account_id = normalize_optional_string(token.account_id)
        .or_else(|| account_id_from_access_token(&token.access_token));
    Ok(OAuthTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        id_token: token.id_token.unwrap_or_default(),
        account_id,
        expires_in: token.expires_in,
    })
}

fn openai_browser_authorize_url(challenge: &str, state: &str) -> String {
    format!(
        "{DEFAULT_ISSUER}/oauth/authorize?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("response_type", "code")
            .append_pair("client_id", DEFAULT_CLIENT_ID)
            .append_pair("redirect_uri", BROWSER_REDIRECT_URI)
            .append_pair("scope", OPENAI_OAUTH_SCOPE)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", OPENAI_OAUTH_ORIGINATOR)
            .finish()
    )
}

fn wait_for_browser_callback(
    listener: TcpListener,
    expected_state: &str,
) -> anyhow::Result<String> {
    let (mut stream, _) = listener.accept()?;
    let mut buf = [0_u8; 8192];
    let len = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..len]);
    let result = parse_browser_callback_request(&request, expected_state);
    let (status, body) = match &result {
        Ok(_) => (
            "200 OK",
            "OpenAI authentication completed. You can close this window.",
        ),
        Err(_) => (
            "400 Bad Request",
            "OpenAI authentication failed. You can close this window and rerun dirge auth openai.",
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    result
}

fn parse_browser_callback_request(request: &str, expected_state: &str) -> anyhow::Result<String> {
    let line = request
        .lines()
        .next()
        .context("empty OpenAI OAuth callback request")?;
    let target = line
        .split_whitespace()
        .nth(1)
        .context("malformed OpenAI OAuth callback request")?;
    let url = url::Url::parse(&format!("http://localhost{target}"))?;
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .context("OpenAI OAuth callback missing state")?;
    if state != expected_state {
        anyhow::bail!("OpenAI OAuth state mismatch");
    }
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .context("OpenAI OAuth callback missing code")?;
    Ok(code)
}

fn oauth_state() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn pkce_verifier() -> String {
    format!(
        "{}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn oauth_tokens_to_credential(
    tokens: OAuthTokens,
    now_epoch_ms: i64,
) -> OpenAiOAuthCredential {
    let expires_at_epoch_ms = access_token_expires_at_epoch_ms(now_epoch_ms, tokens.expires_in);
    OpenAiOAuthCredential::new(
        tokens.access_token,
        tokens.refresh_token,
        Some(tokens.id_token),
        tokens.account_id,
        expires_at_epoch_ms,
    )
}

fn access_token_expires_at_epoch_ms(now_epoch_ms: i64, expires_in_seconds: Option<u64>) -> i64 {
    let expires_in_seconds =
        expires_in_seconds.unwrap_or(FALLBACK_ACCESS_TOKEN_EXPIRES_IN.as_secs());
    let expires_in_ms = expires_in_seconds.saturating_mul(1000);
    let expires_in_ms = i64::try_from(expires_in_ms).unwrap_or(i64::MAX);
    now_epoch_ms.saturating_add(expires_in_ms)
}

fn current_epoch_ms() -> anyhow::Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| anyhow::anyhow!("system clock is before Unix epoch: {err}"))?;
    Ok(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_authorize_url_matches_openai_codex_oauth_shape() {
        let url = url::Url::parse(&openai_browser_authorize_url("challenge", "state-1")).unwrap();
        assert_eq!(
            url.as_str().split('?').next().unwrap(),
            "https://auth.openai.com/oauth/authorize"
        );
        let params = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            params.get("response_type").map(|v| v.as_ref()),
            Some("code")
        );
        assert_eq!(
            params.get("client_id").map(|v| v.as_ref()),
            Some(DEFAULT_CLIENT_ID)
        );
        assert_eq!(
            params.get("redirect_uri").map(|v| v.as_ref()),
            Some(BROWSER_REDIRECT_URI)
        );
        assert_eq!(
            params.get("scope").map(|v| v.as_ref()),
            Some(OPENAI_OAUTH_SCOPE)
        );
        assert_eq!(
            params.get("code_challenge").map(|v| v.as_ref()),
            Some("challenge")
        );
        assert_eq!(
            params.get("code_challenge_method").map(|v| v.as_ref()),
            Some("S256")
        );
        assert_eq!(params.get("state").map(|v| v.as_ref()), Some("state-1"));
        assert_eq!(
            params.get("id_token_add_organizations").map(|v| v.as_ref()),
            Some("true")
        );
        assert_eq!(
            params.get("codex_cli_simplified_flow").map(|v| v.as_ref()),
            Some("true")
        );
        assert_eq!(
            params.get("originator").map(|v| v.as_ref()),
            Some(OPENAI_OAUTH_ORIGINATOR)
        );
    }

    #[test]
    fn pkce_challenge_uses_s256_url_safe_no_pad() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn parses_browser_callback_code_and_validates_state() {
        let request =
            "GET /auth/callback?code=AUTH-CODE&state=STATE HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_browser_callback_request(request, "STATE").unwrap(),
            "AUTH-CODE"
        );

        let err = parse_browser_callback_request(request, "OTHER").unwrap_err();
        assert!(err.to_string().contains("state mismatch"));
    }
}
