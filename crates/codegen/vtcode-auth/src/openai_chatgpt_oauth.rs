//! OpenAI ChatGPT subscription OAuth flow and secure session storage.
//!
//! This module implements an OAuth 2.0 PKCE authorization-code flow for ChatGPT
//! subscription auth, mirroring the flow used by [openai/codex]. By default VT
//! Code reuses the Codex CLI's **public PKCE OAuth client identity** (no client
//! secret — the ID is not a secret by OAuth 2.1 design). This is an **unofficial
//! compatibility mechanism**: OpenAI has not documented or guaranteed third-party
//! reuse of this client identity, and a public client ID is not authorization
//! to reuse another tool's OAuth registration. This allows ChatGPT subscription
//! login to work without the Codex CLI installed.
//! Organizations with their own OpenAI-issued client can override via
//! `VTCODE_OPENAI_OAUTH_CLIENT_ID` / `VTCODE_OPENAI_OAUTH_ORIGINATOR`.
//!
//! - OAuth authorization-code flow with PKCE
//! - refresh-token exchange
//! - token exchange for an OpenAI API-key-style bearer token
//! - secure storage in keyring or encrypted file storage
//!
//! Based on patterns from [openai/codex] (Apache-2.0). Copyright 2025 OpenAI.
//! See the repository `THIRD-PARTY-NOTICES` file for full attribution.
//!
//! [openai/codex]: https://github.com/openai/codex

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fs2::FileExt;
use reqwest::Client;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::fs::OpenOptions;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

use crate::storage_paths::auth_storage_dir;
use crate::{OpenAIAuthConfig, OpenAIPreferredMethod};

pub use super::credentials::AuthCredentialsStoreMode;
use super::pkce::PkceChallenge;
#[cfg(test)]
use crate::openai_refresh_policy::extract_error_code;
use crate::openai_refresh_policy::{RefreshFailureAction, classify_refresh_failure};
use crate::openai_session_storage::OpenAiSessionStorage;
#[cfg(test)]
use crate::openai_session_storage::{
    decrypt_legacy_session as decrypt_session, encrypt_legacy_session as encrypt_session,
    legacy_session_path as get_session_path,
};

const OPENAI_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Default OAuth client identity.
///
/// This is the **Codex CLI's public PKCE OAuth client ID**. VT Code reuses
/// Codex's public client identity (a PKCE public client with no client secret
/// — the ID is not a secret by OAuth 2.1 design) as an **unofficial
/// compatibility mechanism**. OpenAI has not documented or guaranteed
/// third-party reuse of this identity, and a public client ID is not
/// authorization to reuse another tool's OAuth registration. This lets VT
/// Code perform ChatGPT subscription login without requiring the Codex CLI
/// to be installed.
///
/// Organizations with their own OpenAI-issued OAuth client can override this
/// via the `VTCODE_OPENAI_OAUTH_CLIENT_ID` environment variable.
///
/// See `docs/guides/oauth-authentication.md` for the full explanation.
const DEFAULT_OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Default originator sent to OpenAI's authorization endpoint.
///
/// This matches the Codex CLI's originator because the default client ID is
/// Codex's. Override with `VTCODE_OPENAI_OAUTH_ORIGINATOR` when using a custom
/// client ID.
const DEFAULT_OPENAI_ORIGINATOR: &str = "codex_cli_rs";
/// Maximum bytes read from a token-endpoint error response body for
/// classification. Prevents unbounded reads from a misbehaving or hostile
/// endpoint while still capturing standard OAuth 2.0 error JSON.
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const OPENAI_CALLBACK_PATH: &str = "/auth/callback";
const OPENAI_REFRESH_LOCK_FILE: &str = "openai_chatgpt.refresh.lock";
const REFRESH_INTERVAL_SECS: u64 = 8 * 60;
const REFRESH_SKEW_SECS: u64 = 60;

/// Resolved OAuth client identity (client ID + originator).
///
/// Both fields must be consistent: when a custom client ID is provided via
/// `VTCODE_OPENAI_OAUTH_CLIENT_ID`, the originator must also be overridden
/// via `VTCODE_OPENAI_OAUTH_ORIGINATOR`. Sending a custom client ID with
/// Codex's `codex_cli_rs` originator (or vice versa) would be inconsistent
/// and is rejected.
///
/// `Debug` is safe to derive: the client ID is a public PKCE client
/// identifier (not a secret by OAuth 2.1 design), and the originator is
/// a public identifier string.
#[derive(Debug)]
struct OAuthClientIdentity {
    client_id: String,
    originator: String,
}

/// Resolve the OAuth client identity from environment variables.
///
/// ## Invariant
///
/// The client ID and originator form a **coherent pair**. One-sided overrides
/// are rejected to prevent mixed identities (e.g. a custom client ID paired
/// with Codex's `codex_cli_rs` originator).
///
/// - Both `VTCODE_OPENAI_OAUTH_CLIENT_ID` and `VTCODE_OPENAI_OAUTH_ORIGINATOR`
///   set and non-blank → use the custom pair.
/// - Neither set → use the complete Codex default pair.
/// - Only one set → return a configuration error with an actionable message.
///   The caller must surface this so the user can fix the environment before
///   any OAuth request is sent.
///
/// All four flow stages (authorization URL, code exchange, refresh, token
/// exchange) call this resolver, so the same coherent pair is used throughout.
fn resolve_oauth_client_identity() -> Result<OAuthClientIdentity> {
    let custom_client_id = std::env::var("VTCODE_OPENAI_OAUTH_CLIENT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let custom_originator = std::env::var("VTCODE_OPENAI_OAUTH_ORIGINATOR")
        .ok()
        .filter(|v| !v.trim().is_empty());

    match (custom_client_id, custom_originator) {
        (Some(id), Some(originator)) => Ok(OAuthClientIdentity { client_id: id, originator }),
        (Some(_), None) => bail!(
            "VTCODE_OPENAI_OAUTH_CLIENT_ID is set but VTCODE_OPENAI_OAUTH_ORIGINATOR is not. \
             The client ID and originator must be overridden together to form a coherent OAuth \
             identity. Set VTCODE_OPENAI_OAUTH_ORIGINATOR to match your custom client ID, \
             or unset VTCODE_OPENAI_OAUTH_CLIENT_ID to use the default Codex identity."
        ),
        (None, Some(_)) => bail!(
            "VTCODE_OPENAI_OAUTH_ORIGINATOR is set but VTCODE_OPENAI_OAUTH_CLIENT_ID is not. \
             The client ID and originator must be overridden together to form a coherent OAuth \
             identity. Set VTCODE_OPENAI_OAUTH_CLIENT_ID to match your custom originator, \
             or unset VTCODE_OPENAI_OAUTH_ORIGINATOR to use the default Codex identity."
        ),
        (None, None) => Ok(OAuthClientIdentity {
            client_id: DEFAULT_OPENAI_CLIENT_ID.to_string(),
            originator: DEFAULT_OPENAI_ORIGINATOR.to_string(),
        }),
    }
}

/// Stored OpenAI ChatGPT subscription session.
///
/// Custom `Debug` redacts all token fields to prevent credential leakage
/// through `tracing::debug!(?session)` or error wrappers.
#[derive(Clone, Serialize, Deserialize)]
pub struct OpenAIChatGptSession {
    /// Exchanged OpenAI bearer token used for normal API calls when available.
    /// If unavailable, VT Code falls back to the OAuth access token.
    pub openai_api_key: String,
    /// OAuth ID token from the sign-in flow.
    pub id_token: String,
    /// OAuth access token from the sign-in flow.
    pub access_token: String,
    /// Refresh token used to renew the session.
    pub refresh_token: String,
    /// ChatGPT workspace/account identifier, if present.
    pub account_id: Option<String>,
    /// Account email, if present.
    pub email: Option<String>,
    /// ChatGPT plan type, if present.
    pub plan: Option<String>,
    /// When the session was originally created.
    pub obtained_at: u64,
    /// When the OAuth/API-key exchange was last refreshed.
    pub refreshed_at: u64,
    /// Access-token expiry, if supplied by the authority.
    pub expires_at: Option<u64>,
}

impl fmt::Debug for OpenAIChatGptSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAIChatGptSession")
            .field("openai_api_key", &"<redacted>")
            .field("id_token", &"<redacted>")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("plan", &self.plan)
            .field("obtained_at", &self.obtained_at)
            .field("refreshed_at", &self.refreshed_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl OpenAIChatGptSession {
    fn is_refresh_due(&self) -> bool {
        let now = now_secs();
        if let Some(expires_at) = self.expires_at
            && now.saturating_add(REFRESH_SKEW_SECS) >= expires_at
        {
            return true;
        }
        now.saturating_sub(self.refreshed_at) >= REFRESH_INTERVAL_SECS
    }
}

/// Host-provided refresher for externally managed ChatGPT auth tokens.
#[async_trait]
pub trait OpenAIChatGptSessionRefresher: Send + Sync {
    async fn refresh_session(&self, current: &OpenAIChatGptSession) -> Result<OpenAIChatGptSession>;
}

#[derive(Clone)]
enum OpenAIChatGptAuthRefreshStrategy {
    Stored {
        storage_mode: AuthCredentialsStoreMode,
    },
    External {
        refresher: Arc<dyn OpenAIChatGptSessionRefresher>,
    },
}

impl fmt::Debug for OpenAIChatGptAuthRefreshStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stored { storage_mode } => f.debug_struct("Stored").field("storage_mode", storage_mode).finish(),
            Self::External { .. } => f.debug_struct("External").finish_non_exhaustive(),
        }
    }
}

/// Runtime auth state shared by OpenAI provider instances.
#[derive(Clone)]
pub struct OpenAIChatGptAuthHandle {
    session: Arc<Mutex<OpenAIChatGptSession>>,
    refresh_gate: Arc<AsyncMutex<()>>,
    auto_refresh: bool,
    refresh_strategy: OpenAIChatGptAuthRefreshStrategy,
}

impl fmt::Debug for OpenAIChatGptAuthHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAIChatGptAuthHandle")
            .field("auto_refresh", &self.auto_refresh)
            .field("refresh_strategy", &self.refresh_strategy)
            .finish()
    }
}

impl OpenAIChatGptAuthHandle {
    pub fn new(
        session: OpenAIChatGptSession,
        auth_config: OpenAIAuthConfig,
        storage_mode: AuthCredentialsStoreMode,
    ) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            refresh_gate: Arc::new(AsyncMutex::new(())),
            auto_refresh: auth_config.auto_refresh,
            refresh_strategy: OpenAIChatGptAuthRefreshStrategy::Stored { storage_mode },
        }
    }

    pub fn new_external(
        session: OpenAIChatGptSession,
        auto_refresh: bool,
        refresher: Arc<dyn OpenAIChatGptSessionRefresher>,
    ) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            refresh_gate: Arc::new(AsyncMutex::new(())),
            auto_refresh,
            refresh_strategy: OpenAIChatGptAuthRefreshStrategy::External { refresher },
        }
    }

    pub fn snapshot(&self) -> Result<OpenAIChatGptSession> {
        self.session
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| anyhow!("openai chatgpt auth mutex poisoned"))
    }

    pub fn current_api_key(&self) -> Result<String> {
        self.snapshot().map(|session| active_api_bearer_token(&session).to_string())
    }

    pub fn provider_label(&self) -> &'static str {
        "OpenAI (ChatGPT)"
    }

    pub async fn refresh_if_needed(&self) -> Result<()> {
        if !self.auto_refresh {
            return Ok(());
        }

        self.refresh_when(|session| session.is_refresh_due()).await
    }

    pub async fn force_refresh(&self) -> Result<()> {
        self.refresh_when(|_| true).await
    }

    async fn refresh_when<P>(&self, should_refresh: P) -> Result<()>
    where
        P: FnOnce(&OpenAIChatGptSession) -> bool,
    {
        let _refresh_guard = self.refresh_gate.lock().await;
        let session = self.snapshot()?;
        if !should_refresh(&session) {
            return Ok(());
        }

        let refreshed = match &self.refresh_strategy {
            OpenAIChatGptAuthRefreshStrategy::Stored { storage_mode } => {
                refresh_openai_chatgpt_session_from_snapshot(&session, *storage_mode).await?
            }
            OpenAIChatGptAuthRefreshStrategy::External { refresher } => refresher.refresh_session(&session).await?,
        };
        self.replace_session(refreshed)
    }

    #[must_use]
    fn using_external_tokens(&self) -> bool {
        matches!(self.refresh_strategy, OpenAIChatGptAuthRefreshStrategy::External { .. })
    }

    fn replace_session(&self, session: OpenAIChatGptSession) -> Result<()> {
        let mut guard = self.session.lock().map_err(|_| anyhow!("openai chatgpt auth mutex poisoned"))?;
        *guard = session;
        Ok(())
    }
}

/// OpenAI auth resolution chosen for the current runtime.
///
/// Custom `Debug` redacts the bearer `api_key` to prevent credential leakage.
#[derive(Clone)]
pub enum OpenAIResolvedAuth {
    ApiKey {
        api_key: String,
    },
    ChatGpt {
        api_key: String,
        handle: OpenAIChatGptAuthHandle,
    },
}

impl fmt::Debug for OpenAIResolvedAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { .. } => f.debug_struct("OpenAIResolvedAuth::ApiKey").finish(),
            Self::ChatGpt { .. } => f.debug_struct("OpenAIResolvedAuth::ChatGpt").finish(),
        }
    }
}

impl OpenAIResolvedAuth {
    pub fn api_key(&self) -> &str {
        match self {
            Self::ApiKey { api_key } => api_key,
            Self::ChatGpt { api_key, .. } => api_key,
        }
    }

    pub fn handle(&self) -> Option<OpenAIChatGptAuthHandle> {
        match self {
            Self::ApiKey { .. } => None,
            Self::ChatGpt { handle, .. } => Some(handle.clone()),
        }
    }

    fn using_chatgpt(&self) -> bool {
        matches!(self, Self::ChatGpt { .. })
    }
}

fn active_api_bearer_token(session: &OpenAIChatGptSession) -> &str {
    if session.openai_api_key.trim().is_empty() {
        session.access_token.as_str()
    } else {
        session.openai_api_key.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAIResolvedAuthSource {
    ApiKey,
    ChatGpt,
}

/// Where the ChatGPT session originated — used by CLI/TUI to render accurate
/// status without directly inspecting the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAIChatGptSessionProvenance {
    /// Session stored in VT Code's own credential storage (full auto-refresh).
    Native,
    /// Session loaded from Codex CLI's `~/.codex/auth.json` (managed by Codex).
    CodexFallback,
}

/// Redacted summary of available OpenAI credentials for CLI/TUI display.
///
/// Does NOT carry token data — only metadata (email, plan, provenance, expiry)
/// so credential values can never leak through `Debug` or logging.
#[derive(Debug, Clone)]
pub struct OpenAICredentialOverview {
    pub api_key_available: bool,
    /// Email from the ChatGPT session's ID token, if available.
    pub chatgpt_email: Option<String>,
    /// Plan type from the ChatGPT session's ID token, if available.
    pub chatgpt_plan: Option<String>,
    /// `true` when a ChatGPT session (native or Codex fallback) is available.
    pub chatgpt_session_present: bool,
    /// Provenance of the ChatGPT session — `None` when no session is available.
    pub chatgpt_session_provenance: Option<OpenAIChatGptSessionProvenance>,
    /// `true` only when Codex's auth.json was **successfully parsed** into a
    /// usable session (not merely that the file exists on disk).
    pub codex_fallback_available: bool,
    pub active_source: Option<OpenAIResolvedAuthSource>,
    pub preferred_method: OpenAIPreferredMethod,
    pub notice: Option<String>,
    pub recommendation: Option<String>,
}

/// Generic auth status reused by slash auth/status output.
#[derive(Debug, Clone)]
pub enum OpenAIChatGptAuthStatus {
    Authenticated {
        label: Option<String>,
        age_seconds: u64,
        expires_in: Option<u64>,
    },
    NotAuthenticated,
}

/// Build the OpenAI ChatGPT OAuth authorization URL.
pub fn get_openai_chatgpt_auth_url(challenge: &PkceChallenge, callback_port: u16, state: &str) -> Result<String> {
    let redirect_uri = format!("http://localhost:{callback_port}{OPENAI_CALLBACK_PATH}");
    let identity = resolve_oauth_client_identity()?;
    let query = [
        ("response_type", "code".to_string()),
        ("client_id", identity.client_id.clone()),
        ("redirect_uri", redirect_uri),
        ("scope", "openid profile email offline_access api.connectors.read api.connectors.invoke".to_string()),
        ("code_challenge", challenge.code_challenge.clone()),
        ("code_challenge_method", challenge.code_challenge_method.clone()),
        ("id_token_add_organizations", "true".to_string()),
        ("codex_cli_simplified_flow", "true".to_string()),
        ("state", state.to_string()),
        ("originator", identity.originator),
    ];

    let encoded = query
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(format!("{OPENAI_AUTH_URL}?{encoded}"))
}

pub fn generate_openai_oauth_state() -> Result<String> {
    let mut state_bytes = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut state_bytes)
        .map_err(|_| anyhow!("failed to generate openai oauth state"))?;
    Ok(URL_SAFE_NO_PAD.encode(state_bytes))
}

pub fn parse_openai_chatgpt_manual_callback_input(input: &str, expected_state: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("missing authorization callback input");
    }

    let query = if trimmed.contains("://") {
        let url = reqwest::Url::parse(trimmed).context("invalid callback url")?;
        url.query()
            .ok_or_else(|| anyhow!("callback url did not include a query string"))?
            .to_string()
    } else if trimmed.contains('=') {
        trimmed.trim_start_matches('?').to_string()
    } else {
        bail!("paste the full redirect url or query string containing code and state");
    };

    let code = extract_query_value(&query, "code")
        .ok_or_else(|| anyhow!("callback input did not include an authorization code"))?;
    let state = extract_query_value(&query, "state").ok_or_else(|| anyhow!("callback input did not include state"))?;
    if state != expected_state {
        bail!("OAuth error: state mismatch");
    }
    Ok(code)
}

/// Exchange an authorization code for OAuth tokens.
pub async fn exchange_openai_chatgpt_code_for_tokens(
    code: &str,
    challenge: &PkceChallenge,
    callback_port: u16,
) -> Result<OpenAIChatGptSession> {
    let redirect_uri = format!("http://localhost:{callback_port}{OPENAI_CALLBACK_PATH}");
    let identity = resolve_oauth_client_identity()?;
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&identity.client_id),
        urlencoding::encode(&challenge.code_verifier),
    );

    let token_response: OpenAITokenResponse = Client::new()
        .post(OPENAI_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("failed to exchange openai authorization code")?
        .error_for_status()
        .context("openai authorization-code exchange failed")?
        .json()
        .await
        .context("failed to parse openai authorization-code response")?;

    build_session_from_token_response(token_response).await
}

/// Resolve the active OpenAI auth source for the current configuration.
pub fn resolve_openai_auth(
    auth_config: &OpenAIAuthConfig,
    storage_mode: AuthCredentialsStoreMode,
    api_key: Option<String>,
) -> Result<OpenAIResolvedAuth> {
    crate::auth_service::OpenAIAccountAuthService::new(auth_config.clone(), storage_mode).resolve_runtime_auth(api_key)
}

pub fn summarize_openai_credentials(
    auth_config: &OpenAIAuthConfig,
    storage_mode: AuthCredentialsStoreMode,
    api_key: Option<String>,
) -> Result<OpenAICredentialOverview> {
    crate::auth_service::OpenAIAccountAuthService::new(auth_config.clone(), storage_mode).summarize_credentials(api_key)
}

pub fn save_openai_chatgpt_session(session: &OpenAIChatGptSession) -> Result<()> {
    save_openai_chatgpt_session_with_mode(session, AuthCredentialsStoreMode::default())
}

pub fn save_openai_chatgpt_session_with_mode(
    session: &OpenAIChatGptSession,
    mode: AuthCredentialsStoreMode,
) -> Result<()> {
    OpenAiSessionStorage::new().save(session, mode)
}

pub fn load_openai_chatgpt_session() -> Result<Option<OpenAIChatGptSession>> {
    OpenAiSessionStorage::new().load(AuthCredentialsStoreMode::Keyring)
}

pub fn load_openai_chatgpt_session_with_mode(mode: AuthCredentialsStoreMode) -> Result<Option<OpenAIChatGptSession>> {
    OpenAiSessionStorage::new().load(mode)
}

pub fn clear_openai_chatgpt_session() -> Result<()> {
    OpenAiSessionStorage::new().clear_all()
}

pub fn clear_openai_chatgpt_session_with_mode(mode: AuthCredentialsStoreMode) -> Result<()> {
    OpenAiSessionStorage::new().clear(mode)
}

pub fn get_openai_chatgpt_auth_status() -> Result<OpenAIChatGptAuthStatus> {
    get_openai_chatgpt_auth_status_with_mode(AuthCredentialsStoreMode::default())
}

pub fn get_openai_chatgpt_auth_status_with_mode(mode: AuthCredentialsStoreMode) -> Result<OpenAIChatGptAuthStatus> {
    let Some(session) = load_openai_chatgpt_session_with_mode(mode)? else {
        return Ok(OpenAIChatGptAuthStatus::NotAuthenticated);
    };
    let now = now_secs();
    Ok(OpenAIChatGptAuthStatus::Authenticated {
        label: session
            .email
            .clone()
            .or_else(|| session.plan.clone())
            .or_else(|| session.account_id.clone()),
        age_seconds: now.saturating_sub(session.obtained_at),
        expires_in: session.expires_at.map(|expires_at| expires_at.saturating_sub(now)),
    })
}

pub async fn refresh_openai_chatgpt_session_with_mode(mode: AuthCredentialsStoreMode) -> Result<OpenAIChatGptSession> {
    let session = load_openai_chatgpt_session_with_mode(mode)?.ok_or_else(|| anyhow!("Run vtcode login openai"))?;
    refresh_openai_chatgpt_session_from_snapshot(&session, mode).await
}

async fn refresh_openai_chatgpt_session_from_snapshot(
    session: &OpenAIChatGptSession,
    storage_mode: AuthCredentialsStoreMode,
) -> Result<OpenAIChatGptSession> {
    let _lock = acquire_refresh_lock().await?;
    if let Some(current) = load_openai_chatgpt_session_with_mode(storage_mode)?
        && session_has_newer_refresh_state(&current, session)
    {
        return Ok(current);
    }
    refresh_openai_chatgpt_session_without_lock(session, storage_mode).await
}

/// Refresh the ChatGPT session using the stored refresh token.
///
/// The response is parsed as [`OpenAIRefreshResponse`] with independently
/// optional fields — OpenAI's token endpoint may omit unchanged fields.
/// Omitted fields preserve the current session's values. This matches the
/// behavior of `openai/codex`'s `RefreshResponse` + `persist_tokens`.
async fn refresh_openai_chatgpt_session_without_lock(
    current: &OpenAIChatGptSession,
    storage_mode: AuthCredentialsStoreMode,
) -> Result<OpenAIChatGptSession> {
    let identity = resolve_oauth_client_identity()?;
    let response = Client::new()
        .post(OPENAI_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=refresh_token&client_id={}&refresh_token={}",
            urlencoding::encode(&identity.client_id),
            urlencoding::encode(&current.refresh_token),
        ))
        .send()
        .await
        .context("failed to refresh openai chatgpt token")?;

    // Check for HTTP errors. Unlike error_for_status_ref(), we capture the
    // response body to classify token-endpoint errors (e.g. invalid_grant,
    // refresh_token_expired) that reqwest's status-only error would miss.
    if !response.status().is_success() {
        let status = response.status();
        // Read a bounded body for error classification — never log it raw.
        let body_text = read_bounded_text(response, MAX_ERROR_BODY_BYTES).await;
        return Err(classify_refresh_status_error(status, &body_text));
    }

    let refresh_response: OpenAIRefreshResponse =
        response.json().await.context("failed to parse openai refresh response")?;

    let session = merge_refresh_response(current, refresh_response).await?;
    // Guard against a blank access_token — the primary bearer credential.
    // This protects the minimal-session refresh helper (which starts with
    // blank token fields) from persisting a session with blank tokens when
    // the token endpoint returns a partial response that omits access_token.
    if session.access_token.trim().is_empty() {
        bail!("openai token refresh returned no access token — the session cannot be used");
    }
    save_openai_chatgpt_session_with_mode(&session, storage_mode)?;
    Ok(session)
}

/// Merge a partial refresh response into the current session, preserving
/// omitted fields. Only re-exchanges the API key when a new `id_token` is
/// present; otherwise keeps the previous exchanged key.
async fn merge_refresh_response(
    current: &OpenAIChatGptSession,
    resp: OpenAIRefreshResponse,
) -> Result<OpenAIChatGptSession> {
    let now = now_secs();
    // Track which fields were present before moving them out of resp.
    // Treat blank-string values as absent — some token endpoints return
    // empty strings for omitted fields rather than leaving them out.
    let has_new_id_token = resp.id_token.as_deref().is_some_and(|v| !v.trim().is_empty());
    let has_new_access_token = resp.access_token.as_deref().is_some_and(|v| !v.trim().is_empty());
    let new_id_token = resp
        .id_token
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| current.id_token.clone());
    let new_access_token = resp
        .access_token
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| current.access_token.clone());
    let new_refresh_token = resp
        .refresh_token
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| current.refresh_token.clone());

    // Re-exchange the API key only when a new id_token was provided.
    let openai_api_key = if has_new_id_token {
        match exchange_openai_chatgpt_api_key(&new_id_token).await {
            Ok(api_key) => api_key,
            Err(err) => {
                tracing::warn!("openai api-key exchange unavailable, falling back to previous key: {err}");
                current.openai_api_key.clone()
            }
        }
    } else {
        current.openai_api_key.clone()
    };

    // Recompute expiry: prefer expires_in from the response, then try to parse
    // exp from the new access_token JWT. For a **changed** access token without
    // expires_in or a parseable exp, set None — do NOT inherit the previous
    // token's expiry, which belongs to a different token.
    //
    // Key distinction: `has_new_access_token` means the field was present and
    // non-blank, NOT that the token value changed. If the endpoint repeats the
    // same opaque access token without expires_in, the old expiry is still
    // valid and must be preserved.
    let access_token_changed = has_new_access_token && new_access_token != current.access_token;
    let expires_at = if let Some(secs) = resp.expires_in {
        Some(now.saturating_add(secs))
    } else if access_token_changed {
        parse_jwt_exp(&new_access_token)
    } else {
        current.expires_at
    };

    // Update email/plan/account_id only when a new id_token was provided.
    let (email, plan, account_id) = if has_new_id_token {
        let id_claims = parse_jwt_claims(&new_id_token)?;
        let access_claims = parse_jwt_claims(&new_access_token).ok();
        let email = id_claims.email.clone();
        let plan = access_claims.as_ref().and_then(|c| c.plan.clone()).or(id_claims.plan);
        let account_id = access_claims
            .as_ref()
            .and_then(|c| c.account_id.clone())
            .or(id_claims.account_id);
        (email, plan, account_id)
    } else {
        (current.email.clone(), current.plan.clone(), current.account_id.clone())
    };

    Ok(OpenAIChatGptSession {
        openai_api_key,
        id_token: new_id_token,
        access_token: new_access_token,
        refresh_token: new_refresh_token,
        account_id,
        email,
        plan,
        // Preserve the original obtained_at — only refreshed_at advances.
        obtained_at: current.obtained_at,
        refreshed_at: now,
        expires_at,
    })
}

async fn build_session_from_token_response(token_response: OpenAITokenResponse) -> Result<OpenAIChatGptSession> {
    // Validate that the token response contains usable credentials.
    if token_response.access_token.trim().is_empty() {
        bail!("openai authorization-code response did not include a usable access token");
    }
    if token_response.refresh_token.trim().is_empty() {
        bail!("openai authorization-code response did not include a usable refresh token");
    }
    let id_claims = parse_jwt_claims(&token_response.id_token)?;
    let access_claims = parse_jwt_claims(&token_response.access_token).ok();
    let api_key = match exchange_openai_chatgpt_api_key(&token_response.id_token).await {
        Ok(api_key) => api_key,
        Err(err) => {
            tracing::warn!("openai api-key exchange unavailable, falling back to oauth access token: {err}");
            String::new()
        }
    };
    let now = now_secs();
    Ok(OpenAIChatGptSession {
        openai_api_key: api_key,
        id_token: token_response.id_token,
        access_token: token_response.access_token,
        refresh_token: token_response.refresh_token,
        account_id: access_claims
            .as_ref()
            .and_then(|claims| claims.account_id.clone())
            .or(id_claims.account_id),
        email: id_claims
            .email
            .or_else(|| access_claims.as_ref().and_then(|claims| claims.email.clone())),
        plan: access_claims.as_ref().and_then(|claims| claims.plan.clone()).or(id_claims.plan),
        obtained_at: now,
        refreshed_at: now,
        expires_at: token_response.expires_in.map(|secs| now.saturating_add(secs)),
    })
}

async fn exchange_openai_chatgpt_api_key(id_token: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct ExchangeResponse {
        access_token: String,
    }

    let identity = resolve_oauth_client_identity()?;
    let exchange: ExchangeResponse = Client::new()
        .post(OPENAI_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type={}&client_id={}&requested_token={}&subject_token={}&subject_token_type={}",
            urlencoding::encode("urn:ietf:params:oauth:grant-type:token-exchange"),
            urlencoding::encode(&identity.client_id),
            urlencoding::encode("openai-api-key"),
            urlencoding::encode(id_token),
            urlencoding::encode("urn:ietf:params:oauth:token-type:id_token"),
        ))
        .send()
        .await
        .context("failed to exchange openai id token for api key")?
        .error_for_status()
        .context("openai api-key exchange failed")?
        .json()
        .await
        .context("failed to parse openai api-key exchange response")?;

    Ok(exchange.access_token)
}

#[derive(Deserialize)]
struct OpenAITokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Refresh-token grant response — all token fields are independently optional
/// because OpenAI's token endpoint may omit unchanged fields (matching the
/// behavior observed in `openai/codex`'s `RefreshResponse`). Omitted fields
/// preserve the previous session's values during merge.
#[derive(Deserialize)]
struct OpenAIRefreshResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: Option<ProfileClaims>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<AuthClaims>,
}

#[derive(Debug, Deserialize)]
struct ProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ParsedIdTokenClaims {
    pub(crate) email: Option<String>,
    pub(crate) account_id: Option<String>,
    pub(crate) plan: Option<String>,
}

pub(crate) fn parse_jwt_claims(jwt: &str) -> Result<ParsedIdTokenClaims> {
    let mut parts = jwt.split('.');
    let (_, payload_b64, _) = match (parts.next(), parts.next(), parts.next()) {
        (Some(header), Some(payload), Some(signature))
            if !header.is_empty() && !payload.is_empty() && !signature.is_empty() =>
        {
            (header, payload, signature)
        }
        _ => bail!("invalid openai id token"),
    };

    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .context("failed to decode openai id token payload")?;
    let claims: IdTokenClaims = serde_json::from_slice(&payload).context("failed to parse openai id token payload")?;

    Ok(ParsedIdTokenClaims {
        email: claims.email.or_else(|| claims.profile.and_then(|profile| profile.email)),
        account_id: claims.auth.as_ref().and_then(|auth| auth.chatgpt_account_id.clone()),
        plan: claims.auth.and_then(|auth| auth.chatgpt_plan_type),
    })
}

/// Extract the standard `exp` (expiry) claim from a JWT, if present.
///
/// Returns `None` when the token is not a JWT or has no `exp` claim.
/// This is used to populate `expires_at` for Codex-imported sessions,
/// since Codex's `auth.json` does not store expiry separately.
pub(crate) fn parse_jwt_exp(jwt: &str) -> Option<u64> {
    let mut parts = jwt.split('.');
    let _ = parts.next()?;
    let payload_b64 = parts.next()?;
    if payload_b64.is_empty() {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    #[derive(Deserialize)]
    struct ExpClaim {
        #[serde(default)]
        exp: Option<u64>,
    }
    let claims: ExpClaim = serde_json::from_slice(&payload).ok()?;
    claims.exp
}

fn extract_query_value(query: &str, key: &str) -> Option<String> {
    query
        .trim_start_matches('?')
        .split('&')
        .filter_map(|pair| {
            let (pair_key, pair_value) = pair.split_once('=')?;
            (pair_key == key)
                .then(|| urlencoding::decode(pair_value).ok().map(|value| value.into_owned()))
                .flatten()
        })
        .find(|value| !value.is_empty())
}

fn session_has_newer_refresh_state(current: &OpenAIChatGptSession, previous: &OpenAIChatGptSession) -> bool {
    current.refresh_token != previous.refresh_token
        || current.refreshed_at > previous.refreshed_at
        || current.obtained_at > previous.obtained_at
}

struct RefreshLockGuard {
    file: fs::File,
}

impl Drop for RefreshLockGuard {
    fn drop(&mut self) {
        drop(FileExt::unlock(&self.file));
    }
}

async fn acquire_refresh_lock() -> Result<RefreshLockGuard> {
    let path = auth_storage_dir()?.join(OPENAI_REFRESH_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .context("failed to open openai refresh lock")?;
    let file = tokio::task::spawn_blocking(move || {
        file.lock_exclusive().context("failed to acquire openai refresh lock")?;
        Ok::<_, anyhow::Error>(file)
    })
    .await
    .context("openai refresh lock task failed")??;
    Ok(RefreshLockGuard { file })
}

/// Read at most `max_bytes` from an HTTP response body as a string.
///
/// Reads the response in chunks and stops once `max_bytes` have been
/// accumulated, preventing unbounded memory allocation from a misbehaving or
/// hostile endpoint. Invalid UTF-8 sequences are replaced (lossy) since we
/// only use the text for best-effort error classification, never for display
/// or logging.
async fn read_bounded_text(mut response: reqwest::Response, max_bytes: usize) -> String {
    let mut buf = Vec::with_capacity(max_bytes.min(8 * 1024));
    while buf.len() < max_bytes {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = max_bytes - buf.len();
                if chunk.len() <= remaining {
                    buf.extend_from_slice(&chunk);
                } else {
                    buf.extend_from_slice(chunk.get(..remaining).unwrap_or_default());
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn classify_refresh_status_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    let failure = classify_refresh_failure(status, body);
    if failure.action() == RefreshFailureAction::ClearStoredSession {
        if let Err(clear_err) = clear_session_from_all_stores() {
            tracing::warn!("failed to clear expired openai chatgpt session across all stores: {clear_err}");
        }
    }
    failure.into_error()
}

fn clear_session_from_all_stores() -> Result<()> {
    OpenAiSessionStorage::new().clear_all()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
