/// GitHub OAuth Device Flow (RFC 8628) for acquiring a GitHub access token.
///
/// This module implements the device authorization flow that allows a CLI tool
/// to authenticate with GitHub without requiring a browser redirect to
/// localhost.  The flow works as follows:
///
/// 1. Request a device code from `https://github.com/login/device/code`.
/// 2. Display the `user_code` and `verification_uri` to the user so they can
///    authorize on a different device or browser tab.
/// 3. Poll `https://github.com/login/oauth/access_token` at the requested
///    interval until the user authorizes (or the code expires).
/// 4. Persist the resulting access token via
///    [`runtime::save_github_token`].
///
/// The stored token is subsequently read by
/// [`crate::providers::openai_compat::OpenAiCompatClient::from_env`] when
/// `GITHUB_TOKEN` is absent from the environment.
use std::time::Duration;

use crate::http_client::build_http_client_or_default;

/// GitHub OAuth App client ID used for device-flow authentication.
///
/// Users can override this via `GITHUB_COPILOT_CLIENT_ID` if they have
/// registered their own OAuth App.  The default value is the publicly-known
/// client ID used by the GitHub CLI (open-sourced at github.com/cli/cli).
const DEFAULT_GITHUB_CLIENT_ID: &str = "178c6fc778ccc68e1d6a";

/// Scopes requested when authorizing.  `read:user` is required to verify the
/// token; `copilot` grants access to the GitHub Copilot API.
const GITHUB_COPILOT_SCOPES: &str = "read:user copilot";

const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Errors that can occur during the device authorization flow.
#[derive(Debug)]
pub enum DeviceFlowError {
    /// Network or HTTP error.
    Http(reqwest::Error),
    /// The JSON response from GitHub could not be parsed.
    Json(String),
    /// The device code expired before the user authorized.
    Expired,
    /// The user explicitly denied the authorization request.
    AccessDenied,
    /// The flow timed out (caller-supplied deadline exceeded).
    Timeout,
}

impl std::fmt::Display for DeviceFlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error during GitHub device flow: {e}"),
            Self::Json(msg) => write!(f, "failed to parse GitHub response: {msg}"),
            Self::Expired => write!(f, "device code expired before authorization was granted"),
            Self::AccessDenied => write!(f, "authorization was denied by the user"),
            Self::Timeout => write!(f, "timed out waiting for GitHub authorization"),
        }
    }
}

impl std::error::Error for DeviceFlowError {}

impl From<reqwest::Error> for DeviceFlowError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e)
    }
}

/// Parameters received in step 1 of the device flow.
#[derive(Debug, Clone)]
pub struct DeviceCodeInfo {
    pub device_code: String,
    /// Short code to show the user.
    pub user_code: String,
    /// URL the user should visit to authorize.
    pub verification_uri: String,
    /// Polling interval in seconds requested by GitHub.
    pub interval: u64,
    /// Lifetime of the device code in seconds.
    pub expires_in: u64,
}

/// Request the initial device code from GitHub.
pub async fn request_device_code(client_id: &str) -> Result<DeviceCodeInfo, DeviceFlowError> {
    let http = build_http_client_or_default();
    let resp = http
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[("client_id", client_id), ("scope", GITHUB_COPILOT_SCOPES)])
        .send()
        .await?
        .text()
        .await?;

    let v: serde_json::Value =
        serde_json::from_str(&resp).map_err(|e| DeviceFlowError::Json(e.to_string()))?;

    let device_code = v["device_code"]
        .as_str()
        .ok_or_else(|| DeviceFlowError::Json("missing device_code".to_string()))?
        .to_string();
    let user_code = v["user_code"]
        .as_str()
        .ok_or_else(|| DeviceFlowError::Json("missing user_code".to_string()))?
        .to_string();
    let verification_uri = v["verification_uri"]
        .as_str()
        .ok_or_else(|| DeviceFlowError::Json("missing verification_uri".to_string()))?
        .to_string();
    let expires_in = v["expires_in"].as_u64().unwrap_or(900);
    let interval = v["interval"].as_u64().unwrap_or(5);

    Ok(DeviceCodeInfo {
        device_code,
        user_code,
        verification_uri,
        interval,
        expires_in,
    })
}

/// Poll the GitHub token endpoint until the user authorizes, the code expires,
/// or the user denies.  Returns the access token on success.
pub async fn poll_for_token(
    client_id: &str,
    info: &DeviceCodeInfo,
) -> Result<String, DeviceFlowError> {
    let http = build_http_client_or_default();
    let poll_interval = Duration::from_secs(info.interval.max(5));
    let deadline = Duration::from_secs(info.expires_in);
    let mut elapsed = Duration::ZERO;

    loop {
        tokio::time::sleep(poll_interval).await;
        elapsed += poll_interval;
        if elapsed >= deadline {
            return Err(DeviceFlowError::Expired);
        }

        let resp = http
            .post(ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id),
                ("device_code", info.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?
            .text()
            .await?;

        let v: serde_json::Value = serde_json::from_str(&resp)
            .map_err(|e| DeviceFlowError::Json(format!("token poll: {e}")))?;

        if let Some(token) = v["access_token"].as_str().filter(|t| !t.is_empty()) {
            return Ok(token.to_string());
        }

        match v["error"].as_str() {
            Some("expired_token") => return Err(DeviceFlowError::Expired),
            Some("access_denied") => return Err(DeviceFlowError::AccessDenied),
            // "authorization_pending", "slow_down", unknown, or absent → keep polling
            _ => {}
        }
    }
}

/// Returns the client ID to use, preferring `GITHUB_COPILOT_CLIENT_ID` when set.
#[must_use]
pub fn github_client_id() -> String {
    std::env::var("GITHUB_COPILOT_CLIENT_ID")
        .unwrap_or_else(|_| DEFAULT_GITHUB_CLIENT_ID.to_string())
}

// ── Copilot model listing ─────────────────────────────────────────────────────

/// A single model entry returned by the GitHub Copilot models endpoint.
#[derive(Debug, Clone)]
pub struct CopilotModel {
    /// Model ID used with the `copilot/` prefix (e.g. `copilot/gpt-4o`).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Model vendor (e.g. `OpenAI`, `Anthropic`).
    pub vendor: String,
    /// Maximum context window in tokens (0 when not reported).
    pub max_context_tokens: u64,
    /// Maximum output tokens (0 when not reported).
    pub max_output_tokens: u64,
    /// Whether the model supports tool/function calls.
    pub supports_tools: bool,
}

/// Fetch the list of models available to the authenticated Copilot user.
///
/// Calls `GET https://api.githubcopilot.com/models` and returns the parsed
/// model list. The `token` argument should be a GitHub OAuth access token with
/// the `copilot` scope.
pub async fn fetch_copilot_models(token: &str) -> Result<Vec<CopilotModel>, DeviceFlowError> {
    use crate::providers::openai_compat::DEFAULT_GITHUB_COPILOT_BASE_URL;

    let http = build_http_client_or_default();
    let url = format!("{DEFAULT_GITHUB_COPILOT_BASE_URL}/models");

    let resp = http
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        // Required by the Copilot API to identify the integration
        .header("Copilot-Integration-Id", "vscode-chat")
        .send()
        .await
        .map_err(DeviceFlowError::Http)?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(DeviceFlowError::Http)?;

    if !status.is_success() {
        return Err(DeviceFlowError::Json(format!(
            "models endpoint returned HTTP {status}: {body}"
        )));
    }

    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| DeviceFlowError::Json(e.to_string()))?;

    let data = v["data"].as_array().ok_or_else(|| {
        DeviceFlowError::Json("models response missing `data` array".to_string())
    })?;

    let mut models = Vec::with_capacity(data.len());
    for entry in data {
        let id = match entry["id"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let name = entry["name"].as_str().unwrap_or(&id).to_string();
        let vendor = entry["vendor"].as_str().unwrap_or("").to_string();

        let limits = &entry["capabilities"]["limits"];
        let max_context_tokens = limits["max_context_window_tokens"].as_u64().unwrap_or(0);
        let max_output_tokens = limits["max_output_tokens"].as_u64().unwrap_or(0);
        let supports_tools = entry["capabilities"]["supports"]["tool_calls"]
            .as_bool()
            .unwrap_or(false);

        models.push(CopilotModel {
            id,
            name,
            vendor,
            max_context_tokens,
            max_output_tokens,
            supports_tools,
        });
    }

    // Sort alphabetically by id for stable, predictable output
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}


mod tests {
    #[test]
    fn github_client_id_returns_default_when_env_unset() {
        // Ensure the env var is absent for this test; other tests may set it.
        let original = std::env::var("GITHUB_COPILOT_CLIENT_ID").ok();
        std::env::remove_var("GITHUB_COPILOT_CLIENT_ID");
        assert_eq!(github_client_id(), DEFAULT_GITHUB_CLIENT_ID);
        if let Some(v) = original {
            std::env::set_var("GITHUB_COPILOT_CLIENT_ID", v);
        }
    }

    #[test]
    fn github_client_id_respects_env_override() {
        std::env::set_var("GITHUB_COPILOT_CLIENT_ID", "custom-client-id");
        assert_eq!(github_client_id(), "custom-client-id");
        std::env::remove_var("GITHUB_COPILOT_CLIENT_ID");
    }
}
