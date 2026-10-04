use chrono::{DateTime, SecondsFormat, Utc};
use futures::future::join_all;
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    webview::WebviewWindowBuilder,
    AppHandle, Emitter, Manager, PhysicalPosition, Position, Runtime, WebviewUrl, WebviewWindow,
};
use thiserror::Error;

#[cfg(target_os = "macos")]
mod touchbar;

#[cfg(target_os = "macos")]
mod macos_window;

/// Max parallel quota fetches — avoids serial multi-second stalls on large pools.
const QUOTA_FETCH_CONCURRENCY: usize = 6;

const KEYRING_SERVICE: &str = "com.sub2api.pet";
const KEYRING_USER: &str = "active-session";
const TRAY_ID: &str = "main-tray";
/// Normal OS window showing every account's quota (opened from the native menu).
const ACCOUNT_PANEL_LABEL: &str = "account-panel";
const ACCOUNT_PANEL_WIDTH: f64 = 520.0;
const ACCOUNT_PANEL_HEIGHT: f64 = 760.0;
const ACCOUNT_PANEL_MIN_WIDTH: f64 = 420.0;
const ACCOUNT_PANEL_MIN_HEIGHT: f64 = 560.0;

#[derive(Debug, Error)]
enum PetError {
    #[error("网络请求失败：{0}")]
    Network(#[from] reqwest::Error),
    #[error("服务器返回了无法识别的数据")]
    InvalidResponse,
    #[error("{0}")]
    Api(String),
    #[error("登录状态已失效，请重新登录")]
    Unauthorized,
    #[error("系统钥匙串不可用：{0}")]
    Keyring(String),
}

impl Serialize for PetError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Clone)]
struct ApiState {
    client: Client,
}

#[derive(Debug, Serialize, Deserialize)]
struct Tokens {
    base_url: String,
    email: String,
    access_token: String,
    refresh_token: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum LoginStatus {
    Connected,
    Requires2fa,
}

#[derive(Debug, Serialize)]
struct LoginResult {
    status: LoginStatus,
    temp_token: Option<String>,
    email_masked: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PoolAccount {
    id: i64,
    name: String,
    status: String,
    plan: Option<String>,
    platform: String,
    account_type: String,
    last_used_at: Option<String>,
}

/// Kept for the single-account command used by older clients.
#[derive(Debug, Serialize)]
struct CodexAccount {
    id: i64,
    name: String,
    status: String,
    plan: Option<String>,
}

#[derive(Debug, Serialize)]
struct QuotaSnapshot {
    account_id: i64,
    account_name: String,
    used_percent: f64,
    remaining_percent: f64,
    reset_at: Option<String>,
    updated_at: String,
    source: String,
    window_label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct QuotaWindow {
    label: String,
    used_percent: f64,
    remaining_percent: f64,
    reset_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct AccountQuotaRow {
    id: i64,
    name: String,
    status: String,
    plan: Option<String>,
    platform: String,
    account_type: String,
    last_used_at: Option<String>,
    /// Primary remaining percent used for alerts (lowest remaining window).
    remaining_percent: Option<f64>,
    windows: Vec<QuotaWindow>,
    updated_at: Option<String>,
    source: Option<String>,
}

fn normalize_base_url(value: &str) -> Result<String, PetError> {
    let mut base = value.trim().trim_end_matches('/').to_string();
    if base.ends_with("/api/v1") {
        return Ok(base);
    }
    let parsed =
        reqwest::Url::parse(&base).map_err(|_| PetError::Api("请输入完整的平台地址".into()))?;
    if parsed.scheme() != "https"
        && parsed.host_str() != Some("localhost")
        && parsed.host_str() != Some("127.0.0.1")
    {
        return Err(PetError::Api("远程平台必须使用 HTTPS".into()));
    }
    base.push_str("/api/v1");
    Ok(base)
}

fn keyring_entry() -> Result<keyring::Entry, PetError> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|error| PetError::Keyring(error.to_string()))
}

fn save_tokens(tokens: &Tokens) -> Result<(), PetError> {
    let raw = serde_json::to_string(tokens).map_err(|_| PetError::InvalidResponse)?;
    keyring_entry()?
        .set_password(&raw)
        .map_err(|error| PetError::Keyring(error.to_string()))
}

fn load_tokens() -> Result<Tokens, PetError> {
    let raw = keyring_entry()?
        .get_password()
        .map_err(|_| PetError::Unauthorized)?;
    serde_json::from_str(&raw).map_err(|_| PetError::Unauthorized)
}

fn unwrap_api(value: Value) -> Result<Value, PetError> {
    if value.get("code").is_none() {
        return Ok(value);
    }
    if value.get("code").and_then(Value::as_i64) == Some(0) {
        return Ok(value.get("data").cloned().unwrap_or(Value::Null));
    }
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("请求失败");
    Err(PetError::Api(message.to_string()))
}

async fn decode_response(response: reqwest::Response) -> Result<Value, PetError> {
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|_| PetError::InvalidResponse)?;
    if !status.is_success() {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| value.get("error").and_then(Value::as_str))
            .unwrap_or("服务器请求失败");
        return Err(PetError::Api(message.to_string()));
    }
    unwrap_api(value)
}

async fn refresh_access_token(state: &ApiState, mut tokens: Tokens) -> Result<Tokens, PetError> {
    let refresh_token = tokens.refresh_token.clone().ok_or(PetError::Unauthorized)?;
    let response = state
        .client
        .post(format!("{}/auth/refresh", tokens.base_url))
        .json(&json!({ "refresh_token": refresh_token }))
        .send()
        .await?;
    let data = decode_response(response).await?;
    tokens.access_token = data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(PetError::InvalidResponse)?
        .to_string();
    if let Some(value) = data.get("refresh_token").and_then(Value::as_str) {
        tokens.refresh_token = Some(value.to_string());
    }
    save_tokens(&tokens)?;
    Ok(tokens)
}

async fn authorized_request(
    state: &ApiState,
    method: Method,
    path: &str,
) -> Result<Value, PetError> {
    let mut tokens = load_tokens()?;
    for attempt in 0..2 {
        let response = state
            .client
            .request(method.clone(), format!("{}{}", tokens.base_url, path))
            .bearer_auth(&tokens.access_token)
            .header("Accept-Language", "zh-CN")
            .header("X-Admin-UI-Request", "1")
            .send()
            .await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return decode_response(response).await;
        }
        if attempt == 0 {
            tokens = match refresh_access_token(state, tokens).await {
                Ok(new_tokens) => new_tokens,
                Err(_) => return Err(PetError::Unauthorized),
            };
        }
    }
    Err(PetError::Unauthorized)
}

fn tokens_from_auth(base_url: String, email: String, data: &Value) -> Result<Tokens, PetError> {
    let access_token = data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(PetError::InvalidResponse)?
        .to_string();
    let refresh_token = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Tokens {
        base_url,
        email,
        access_token,
        refresh_token,
    })
}

#[tauri::command]
async fn login(
    state: tauri::State<'_, ApiState>,
    base_url: String,
    email: String,
    password: String,
) -> Result<LoginResult, PetError> {
    let base_url = normalize_base_url(&base_url)?;
    let response = state
        .client
        .post(format!("{base_url}/auth/login"))
        .json(&json!({ "email": email, "password": password }))
        .send()
        .await?;
    let data = decode_response(response).await?;
    if data.get("requires_2fa").and_then(Value::as_bool) == Some(true) {
        return Ok(LoginResult {
            status: LoginStatus::Requires2fa,
            temp_token: data
                .get("temp_token")
                .and_then(Value::as_str)
                .map(str::to_string),
            email_masked: data
                .get("user_email_masked")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    let role = data.pointer("/user/role").and_then(Value::as_str);
    if role != Some("admin") {
        return Err(PetError::Api("该账号不是管理员，无法读取账号池额度".into()));
    }
    save_tokens(&tokens_from_auth(base_url, email, &data)?)?;
    Ok(LoginResult {
        status: LoginStatus::Connected,
        temp_token: None,
        email_masked: None,
    })
}

#[tauri::command]
async fn complete_login(
    state: tauri::State<'_, ApiState>,
    base_url: String,
    email: String,
    temp_token: String,
    totp_code: String,
) -> Result<LoginResult, PetError> {
    let base_url = normalize_base_url(&base_url)?;
    let response = state
        .client
        .post(format!("{base_url}/auth/login/2fa"))
        .json(&json!({ "temp_token": temp_token, "totp_code": totp_code }))
        .send()
        .await?;
    let data = decode_response(response).await?;
    let role = data.pointer("/user/role").and_then(Value::as_str);
    if role != Some("admin") {
        return Err(PetError::Api("该账号不是管理员，无法读取账号池额度".into()));
    }
    save_tokens(&tokens_from_auth(base_url, email, &data)?)?;
    Ok(LoginResult {
        status: LoginStatus::Connected,
        temp_token: None,
        email_masked: None,
    })
}

fn is_pool_platform(platform: &str) -> bool {
    // openai=Codex, anthropic=Claude, xai/grok=Grok (when the gateway exposes them).
    matches!(platform, "openai" | "anthropic" | "xai" | "grok")
}

fn account_from_value(value: &Value) -> Option<PoolAccount> {
    let platform = value.get("platform")?.as_str()?;
    if !is_pool_platform(platform) {
        return None;
    }
    let id = value.get("id")?.as_i64()?;
    let name = value.get("name")?.as_str()?.to_string();
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("inactive")
        .to_string();
    let account_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let last_used_at = value
        .get("last_used_at")
        .and_then(Value::as_str)
        .map(str::to_string);
    let plan = value
        .pointer("/extra/plan_type")
        .or_else(|| value.pointer("/extra/subscription_tier"))
        .or_else(|| value.pointer("/extra/grok_billing_snapshot/plan"))
        .or_else(|| value.get("subscription_tier"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(PoolAccount {
        id,
        name,
        status,
        plan,
        platform: platform.to_string(),
        account_type,
        last_used_at,
    })
}

async fn fetch_account_items_for_platform(
    state: &ApiState,
    platform: &str,
) -> Result<Vec<Value>, PetError> {
    let mut page = 1_i64;
    let mut items = Vec::new();
    loop {
        let data = authorized_request(
            state,
            Method::GET,
            &format!("/admin/accounts?page={page}&page_size=100&platform={platform}"),
        )
        .await?;
        let page_items = data
            .get("items")
            .and_then(Value::as_array)
            .ok_or(PetError::InvalidResponse)?;
        if page_items.is_empty() {
            break;
        }
        items.extend(page_items.iter().cloned());
        let total = data.get("total").and_then(Value::as_i64).unwrap_or(0);
        let page_size = data
            .get("page_size")
            .and_then(Value::as_i64)
            .unwrap_or(100)
            .max(1);
        let total_pages = data
            .get("total_pages")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| {
                if total <= 0 {
                    page
                } else {
                    (total + page_size - 1) / page_size
                }
            });
        if page >= total_pages || (total > 0 && items.len() as i64 >= total) {
            break;
        }
        page += 1;
        if page > 50 {
            break;
        }
    }
    Ok(items)
}

async fn fetch_pool_account_items(state: &ApiState) -> Result<Vec<Value>, PetError> {
    let mut items = Vec::new();
    // Fetch each known pool family; missing platforms simply return empty pages.
    for platform in ["openai", "anthropic", "xai", "grok"] {
        match fetch_account_items_for_platform(state, platform).await {
            Ok(page) => items.extend(page),
            // Ignore unknown-platform 4xx so older gateways without Grok still work.
            Err(PetError::Api(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(items)
}

#[tauri::command]
async fn list_codex_accounts(
    state: tauri::State<'_, ApiState>,
) -> Result<Vec<CodexAccount>, PetError> {
    let items = fetch_pool_account_items(&state).await?;
    let mut accounts: Vec<_> = items
        .iter()
        .filter_map(account_from_value)
        .map(|account| CodexAccount {
            id: account.id,
            name: account.name,
            status: account.status,
            plan: account.plan,
        })
        .collect();
    accounts.sort_by_key(|account| account.status != "active");
    Ok(accounts)
}

fn iso_from_epoch(seconds: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn parse_force_quota(account_id: i64, account_name: String, data: &Value) -> Option<QuotaSnapshot> {
    let rate_limit = data.get("rate_limit")?;
    let windows = [
        rate_limit.get("primary_window"),
        rate_limit.get("secondary_window"),
    ];
    let window = windows
        .into_iter()
        .flatten()
        .filter(|window| window.is_object())
        .max_by_key(|window| {
            window
                .get("limit_window_seconds")
                .and_then(Value::as_i64)
                .unwrap_or(0)
        })?;
    let used = window.get("used_percent")?.as_f64()?.clamp(0.0, 100.0);
    let reset_at = window
        .get("reset_at")
        .and_then(Value::as_i64)
        .and_then(iso_from_epoch);
    Some(QuotaSnapshot {
        account_id,
        account_name,
        used_percent: used,
        remaining_percent: (100.0 - used).max(0.0),
        reset_at,
        updated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        source: "active".into(),
        window_label: Some("7d".into()),
    })
}

fn parse_cached_quota(
    account_id: i64,
    account_name: String,
    data: &Value,
) -> Option<QuotaSnapshot> {
    let extra = data.get("extra")?;
    let canonical_used = extra.get("codex_7d_used_percent").and_then(Value::as_f64);
    let legacy_prefix = if canonical_used.is_none() {
        let primary_minutes = extra
            .get("codex_primary_window_minutes")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let secondary_minutes = extra
            .get("codex_secondary_window_minutes")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        Some(if primary_minutes >= secondary_minutes {
            "primary"
        } else {
            "secondary"
        })
    } else {
        None
    };
    let used = canonical_used
        .or_else(|| {
            extra
                .get(format!("codex_{}_used_percent", legacy_prefix?).as_str())
                .and_then(Value::as_f64)
        })?
        .clamp(0.0, 100.0);
    let reset_at = if canonical_used.is_some() {
        extra
            .get("codex_7d_reset_at")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                let remaining = extra.get("codex_7d_reset_after_seconds")?.as_i64()?;
                iso_from_epoch(Utc::now().timestamp() + remaining)
            })
    } else {
        let remaining = extra
            .get(format!("codex_{}_reset_after_seconds", legacy_prefix?).as_str())
            .and_then(Value::as_i64);
        remaining.and_then(|seconds| iso_from_epoch(Utc::now().timestamp() + seconds))
    };
    let updated_at = extra
        .get("codex_usage_updated_at")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
    Some(QuotaSnapshot {
        account_id,
        account_name,
        used_percent: used,
        remaining_percent: (100.0 - used).max(0.0),
        reset_at,
        updated_at,
        source: "cached".into(),
        window_label: Some("7d".into()),
    })
}

/// Collect OpenAI/Codex usage windows from active rate_limit response (5h session + 7d weekly).
fn parse_openai_force_windows(data: &Value) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    if let Some(rate_limit) = data.get("rate_limit") {
        let mut win_5h: Option<QuotaWindow> = None;
        let mut win_7d: Option<QuotaWindow> = None;

        for key in ["primary_window", "secondary_window"] {
            if let Some(w) = rate_limit.get(key).filter(|v| v.is_object()) {
                if let Some(used) = w.get("used_percent").and_then(Value::as_f64) {
                    let used = used.clamp(0.0, 100.0);
                    let seconds = w
                        .get("limit_window_seconds")
                        .and_then(Value::as_i64)
                        .or_else(|| {
                            w.get("window_minutes")
                                .and_then(Value::as_i64)
                                .map(|m| m * 60)
                        })
                        .unwrap_or(0);
                    let reset_at = w
                        .get("reset_at")
                        .and_then(Value::as_i64)
                        .and_then(iso_from_epoch)
                        .or_else(|| {
                            w.get("reset_at")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        });

                    if seconds > 0 && seconds <= 43200 {
                        win_5h = Some(QuotaWindow {
                            label: "5h".into(),
                            used_percent: used,
                            remaining_percent: (100.0 - used).max(0.0),
                            reset_at,
                        });
                    } else if seconds > 43200 {
                        win_7d = Some(QuotaWindow {
                            label: "7d".into(),
                            used_percent: used,
                            remaining_percent: (100.0 - used).max(0.0),
                            reset_at,
                        });
                    }
                }
            }
        }

        if let Some(w) = win_5h {
            windows.push(w);
        }
        if let Some(w) = win_7d {
            windows.push(w);
        }
    }

    if windows.is_empty() {
        let parsed = parse_usage_windows(data);
        if !parsed.is_empty() {
            return parsed;
        }
    }

    windows
}

/// Collect OpenAI/Codex usage windows from cached account extra fields (5h session + 7d weekly).
fn parse_openai_cached_windows(data: &Value) -> Vec<QuotaWindow> {
    let Some(extra) = data.get("extra") else {
        return Vec::new();
    };

    let mut win_5h: Option<QuotaWindow> = None;
    let mut win_7d: Option<QuotaWindow> = None;

    // Check canonical / direct fields first
    if let Some(used) = extra.get("codex_5h_used_percent").and_then(Value::as_f64) {
        let used = used.clamp(0.0, 100.0);
        let reset_at = extra
            .get("codex_5h_reset_at")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                let remaining = extra.get("codex_5h_reset_after_seconds")?.as_i64()?;
                iso_from_epoch(Utc::now().timestamp() + remaining)
            });
        win_5h = Some(QuotaWindow {
            label: "5h".into(),
            used_percent: used,
            remaining_percent: (100.0 - used).max(0.0),
            reset_at,
        });
    }

    if let Some(used) = extra.get("codex_7d_used_percent").and_then(Value::as_f64) {
        let used = used.clamp(0.0, 100.0);
        let reset_at = extra
            .get("codex_7d_reset_at")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                let remaining = extra.get("codex_7d_reset_after_seconds")?.as_i64()?;
                iso_from_epoch(Utc::now().timestamp() + remaining)
            });
        win_7d = Some(QuotaWindow {
            label: "7d".into(),
            used_percent: used,
            remaining_percent: (100.0 - used).max(0.0),
            reset_at,
        });
    }

    // Check primary / secondary window pairs (standard in Sub2API for Codex)
    for prefix in ["primary", "secondary"] {
        let minutes = extra
            .get(format!("codex_{}_window_minutes", prefix).as_str())
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let used_opt = extra
            .get(format!("codex_{}_used_percent", prefix).as_str())
            .and_then(Value::as_f64);

        if let Some(used) = used_opt {
            let used = used.clamp(0.0, 100.0);
            let reset_at = extra
                .get(format!("codex_{}_reset_at", prefix).as_str())
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    let remaining = extra
                        .get(format!("codex_{}_reset_after_seconds", prefix).as_str())?
                        .as_i64()?;
                    iso_from_epoch(Utc::now().timestamp() + remaining)
                });

            if minutes > 0 && minutes <= 720 && win_5h.is_none() {
                win_5h = Some(QuotaWindow {
                    label: "5h".into(),
                    used_percent: used,
                    remaining_percent: (100.0 - used).max(0.0),
                    reset_at,
                });
            } else if minutes > 720 && win_7d.is_none() {
                win_7d = Some(QuotaWindow {
                    label: "7d".into(),
                    used_percent: used,
                    remaining_percent: (100.0 - used).max(0.0),
                    reset_at,
                });
            }
        }
    }

    let mut windows = Vec::new();
    if let Some(w) = win_5h {
        windows.push(w);
    }
    if let Some(w) = win_7d {
        windows.push(w);
    }

    if windows.is_empty() {
        let parsed = parse_usage_windows(extra);
        if !parsed.is_empty() {
            return parsed;
        }
    }

    windows
}

fn grok_billing_snapshot(data: &Value) -> Option<&Value> {
    data.pointer("/extra/grok_billing_snapshot")
        .or_else(|| data.get("grok_billing_snapshot"))
        .or_else(|| {
            (data.get("usage_percent").is_some() || data.get("used_percent").is_some())
                .then_some(data)
        })
}

fn parse_grok_quota(
    account_id: i64,
    account_name: String,
    data: &Value,
    source: &str,
) -> Option<QuotaSnapshot> {
    let snapshot = grok_billing_snapshot(data)?;
    let percent = |key: &str| {
        snapshot.get(key).and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|raw| raw.parse::<f64>().ok()))
        })
    };

    // usage_percent is the current Grok billing period (weekly for SuperGrok).
    // used_percent is the monthly included-credit ratio and is only a fallback.
    let weekly_used = percent("usage_percent");
    let (used, reset_at, updated_at, window_label) = if let Some(used) = weekly_used {
        let period_type = snapshot
            .get("period_type")
            .and_then(Value::as_str)
            .unwrap_or("weekly");
        let label = if period_type == "monthly" {
            "月"
        } else {
            "7d"
        };
        (
            used,
            snapshot
                .get("period_end")
                .or_else(|| snapshot.get("billing_period_end"))
                .and_then(Value::as_str)
                .map(str::to_string),
            snapshot
                .get("weekly_updated_at")
                .or_else(|| snapshot.get("updated_at"))
                .or_else(|| snapshot.get("fetched_at"))
                .and_then(Value::as_str)
                .map(str::to_string),
            label,
        )
    } else {
        (
            percent("used_percent")?,
            snapshot
                .get("billing_period_end")
                .or_else(|| snapshot.get("period_end"))
                .and_then(Value::as_str)
                .map(str::to_string),
            snapshot
                .get("monthly_updated_at")
                .or_else(|| snapshot.get("updated_at"))
                .or_else(|| snapshot.get("fetched_at"))
                .and_then(Value::as_str)
                .map(str::to_string),
            "月",
        )
    };
    let used = used.clamp(0.0, 100.0);

    Some(QuotaSnapshot {
        account_id,
        account_name,
        used_percent: used,
        remaining_percent: (100.0 - used).max(0.0),
        reset_at,
        updated_at: updated_at
            .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
        source: source.to_string(),
        window_label: Some(window_label.to_string()),
    })
}

fn usage_window_reset_at(window: &Value) -> Option<String> {
    if let Some(resets_at) = window.get("resets_at").and_then(Value::as_str) {
        if !resets_at.is_empty() {
            return Some(resets_at.to_string());
        }
    }
    window
        .get("remaining_seconds")
        .and_then(Value::as_i64)
        .and_then(|seconds| iso_from_epoch(Utc::now().timestamp() + seconds.max(0)))
}

fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|n| n as f64))
        .or_else(|| value.as_u64().map(|n| n as f64))
        .or_else(|| value.as_str().and_then(|raw| raw.trim().parse::<f64>().ok()))
}

fn json_field_f64(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(json_f64)
}

/// Parse a Claude-style usage window. Keep fully-used (0% remaining) windows —
/// they must still appear as empty bars instead of vanishing from the panel.
fn parse_usage_window(value: &Value, label: &str) -> Option<QuotaWindow> {
    if value.is_null() || !value.is_object() {
        return None;
    }
    // Prefer utilization / used_percent; fall back to remaining so a depleted
    // window with only remaining=0 still renders.
    let used = json_field_f64(value, "utilization")
        .or_else(|| json_field_f64(value, "used_percent"))
        .or_else(|| {
            json_field_f64(value, "remaining_percent")
                .or_else(|| json_field_f64(value, "remaining"))
                .map(|remaining| (100.0 - remaining).max(0.0))
        })?;
    let used = used.max(0.0).min(100.0);
    Some(QuotaWindow {
        label: label.to_string(),
        used_percent: used,
        remaining_percent: (100.0 - used).max(0.0),
        reset_at: usage_window_reset_at(value),
    })
}

/// Collect Claude-style usage windows for the panel (5h then 7d).
fn parse_usage_windows(data: &Value) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    // Match admin UI order: 5h (session) then 7d (weekly).
    for (key, label) in [("five_hour", "5h"), ("seven_day", "7d")] {
        if let Some(window) = data
            .get(key)
            .and_then(|value| parse_usage_window(value, label))
        {
            windows.push(window);
        }
    }
    windows
}

/// Single-account command still prefers weekly, then 5h.
fn parse_usage_quota(
    account_id: i64,
    account_name: String,
    data: &Value,
    source: &str,
) -> Option<QuotaSnapshot> {
    let windows = parse_usage_windows(data);
    let primary = windows
        .iter()
        .find(|window| window.label == "7d")
        .or_else(|| windows.first())?;
    let updated_at = data
        .get("updated_at")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
    Some(QuotaSnapshot {
        account_id,
        account_name,
        used_percent: primary.used_percent,
        remaining_percent: primary.remaining_percent,
        reset_at: primary.reset_at.clone(),
        updated_at,
        source: source.to_string(),
        window_label: Some(primary.label.clone()),
    })
}

fn row_from_windows(
    account: &PoolAccount,
    windows: Vec<QuotaWindow>,
    updated_at: Option<String>,
    source: Option<String>,
) -> AccountQuotaRow {
    let remaining_percent = windows
        .iter()
        .map(|window| window.remaining_percent)
        .reduce(f64::min);
    AccountQuotaRow {
        id: account.id,
        name: account.name.clone(),
        status: account.status.clone(),
        plan: account.plan.clone(),
        platform: account.platform.clone(),
        account_type: account.account_type.clone(),
        last_used_at: account.last_used_at.clone(),
        remaining_percent,
        windows,
        updated_at,
        source,
    }
}

fn snapshot_to_row(snapshot: QuotaSnapshot, account: &PoolAccount) -> AccountQuotaRow {
    let windows = vec![QuotaWindow {
        label: snapshot.window_label.clone().unwrap_or_else(|| "7d".into()),
        used_percent: snapshot.used_percent,
        remaining_percent: snapshot.remaining_percent,
        reset_at: snapshot.reset_at.clone(),
    }];
    row_from_windows(
        account,
        windows,
        Some(snapshot.updated_at),
        Some(snapshot.source),
    )
}

fn empty_row(account: &PoolAccount) -> AccountQuotaRow {
    row_from_windows(account, Vec::new(), None, None)
}

fn usage_to_row(account: &PoolAccount, usage: &Value, source: &str) -> AccountQuotaRow {
    let mut row_account = account.clone();
    if row_account.plan.is_none() {
        row_account.plan = usage
            .get("subscription_tier")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let windows = parse_usage_windows(usage);
    if windows.is_empty() {
        return empty_row(&row_account);
    }
    let updated_at = usage
        .get("updated_at")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)));
    row_from_windows(&row_account, windows, updated_at, Some(source.to_string()))
}

fn supports_usage_endpoint(account: &PoolAccount) -> bool {
    match account.platform.as_str() {
        "anthropic" => matches!(account.account_type.as_str(), "oauth" | "setup-token"),
        "openai" => account.account_type == "oauth",
        _ => false,
    }
}

async fn quota_for_openai_account(
    state: &ApiState,
    account: &PoolAccount,
    list_item: Option<&Value>,
    force: bool,
) -> AccountQuotaRow {
    if force {
        if let Ok(active) = authorized_request(
            state,
            Method::GET,
            &format!("/admin/openai/accounts/{}/quota", account.id),
        )
        .await
        {
            let windows = parse_openai_force_windows(&active);
            if !windows.is_empty() {
                return row_from_windows(
                    account,
                    windows,
                    Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
                    Some("active".into()),
                );
            }
            if let Some(snapshot) = parse_force_quota(account.id, account.name.clone(), &active) {
                return snapshot_to_row(snapshot, account);
            }
        }
        // OpenAI OAuth may also expose usage windows via the generic usage endpoint.
        if supports_usage_endpoint(account) {
            if let Ok(usage) = authorized_request(
                state,
                Method::GET,
                &format!(
                    "/admin/accounts/{}/usage?source=active&force=true",
                    account.id
                ),
            )
            .await
            {
                let row = usage_to_row(account, &usage, "active");
                if !row.windows.is_empty() {
                    return row;
                }
            }
        }
    }

    if let Some(item) = list_item {
        let windows = parse_openai_cached_windows(item);
        if !windows.is_empty() {
            let updated_at = item
                .pointer("/extra/codex_usage_updated_at")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
            return row_from_windows(account, windows, Some(updated_at), Some("cached".into()));
        }
        if let Some(snapshot) = parse_cached_quota(account.id, account.name.clone(), item) {
            return snapshot_to_row(snapshot, account);
        }
    }

    if let Ok(detail) = authorized_request(
        state,
        Method::GET,
        &format!("/admin/accounts/{}", account.id),
    )
    .await
    {
        let windows = parse_openai_cached_windows(&detail);
        if !windows.is_empty() {
            let updated_at = detail
                .pointer("/extra/codex_usage_updated_at")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
            return row_from_windows(account, windows, Some(updated_at), Some("cached".into()));
        }
        if let Some(snapshot) = parse_cached_quota(account.id, account.name.clone(), &detail) {
            return snapshot_to_row(snapshot, account);
        }
    }

    if supports_usage_endpoint(account) {
        if let Ok(usage) = authorized_request(
            state,
            Method::GET,
            &format!("/admin/accounts/{}/usage?source=passive", account.id),
        )
        .await
        {
            let row = usage_to_row(account, &usage, "cached");
            if !row.windows.is_empty() {
                return row;
            }
        }
    }

    empty_row(account)
}


async fn quota_for_anthropic_account(
    state: &ApiState,
    account: &PoolAccount,
    force: bool,
) -> AccountQuotaRow {
    if !supports_usage_endpoint(account) {
        return empty_row(account);
    }

    let path = if force {
        format!(
            "/admin/accounts/{}/usage?source=active&force=true",
            account.id
        )
    } else {
        format!("/admin/accounts/{}/usage?source=passive", account.id)
    };

    match authorized_request(state, Method::GET, &path).await {
        Ok(usage) => {
            let source = if force { "active" } else { "cached" };
            usage_to_row(account, &usage, source)
        }
        Err(_) => empty_row(account),
    }
}

async fn quota_for_grok_account(
    state: &ApiState,
    account: &PoolAccount,
    list_item: Option<&Value>,
    force: bool,
) -> AccountQuotaRow {
    if force {
        // Newer gateways may refresh and return the Grok snapshot through this endpoint.
        // Older gateways return 500, in which case the account snapshot remains usable.
        if let Ok(active) = authorized_request(
            state,
            Method::GET,
            &format!(
                "/admin/accounts/{}/usage?source=active&force=true",
                account.id
            ),
        )
        .await
        {
            if let Some(snapshot) =
                parse_grok_quota(account.id, account.name.clone(), &active, "active")
            {
                return snapshot_to_row(snapshot, account);
            }
        }
    }

    if let Some(item) = list_item {
        if let Some(snapshot) = parse_grok_quota(account.id, account.name.clone(), item, "cached") {
            return snapshot_to_row(snapshot, account);
        }
    }

    if let Ok(detail) = authorized_request(
        state,
        Method::GET,
        &format!("/admin/accounts/{}", account.id),
    )
    .await
    {
        if let Some(snapshot) =
            parse_grok_quota(account.id, account.name.clone(), &detail, "cached")
        {
            return snapshot_to_row(snapshot, account);
        }
    }

    empty_row(account)
}

async fn quota_for_account(
    state: &ApiState,
    account: &PoolAccount,
    list_item: Option<&Value>,
    force: bool,
) -> AccountQuotaRow {
    match account.platform.as_str() {
        "anthropic" => quota_for_anthropic_account(state, account, force).await,
        "openai" => quota_for_openai_account(state, account, list_item, force).await,
        "xai" | "grok" => quota_for_grok_account(state, account, list_item, force).await,
        _ => empty_row(account),
    }
}

#[tauri::command]
async fn refresh_quota(
    state: tauri::State<'_, ApiState>,
    account_id: i64,
    force: bool,
) -> Result<QuotaSnapshot, PetError> {
    let account = authorized_request(
        &state,
        Method::GET,
        &format!("/admin/accounts/{account_id}"),
    )
    .await?;
    let account_name = account
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Account")
        .to_string();
    let platform = account
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or("openai")
        .to_string();

    if platform == "anthropic" {
        let path = if force {
            format!("/admin/accounts/{account_id}/usage?source=active&force=true")
        } else {
            format!("/admin/accounts/{account_id}/usage?source=passive")
        };
        let usage = authorized_request(&state, Method::GET, &path).await?;
        let source = if force { "active" } else { "cached" };
        return parse_usage_quota(account_id, account_name, &usage, source).ok_or_else(|| {
            PetError::Api("该 Claude 账号还没有可用的额度数据，请双击宠物主动刷新".into())
        });
    }

    if matches!(platform.as_str(), "xai" | "grok") {
        if force {
            if let Ok(active) = authorized_request(
                &state,
                Method::GET,
                &format!("/admin/accounts/{account_id}/usage?source=active&force=true"),
            )
            .await
            {
                if let Some(snapshot) =
                    parse_grok_quota(account_id, account_name.clone(), &active, "active")
                {
                    return Ok(snapshot);
                }
            }
        }

        return parse_grok_quota(account_id, account_name, &account, "cached").ok_or_else(|| {
            PetError::Api("该 Grok 账号还没有可用的账期额度数据，请刷新后重试".into())
        });
    }

    if force {
        let active = authorized_request(
            &state,
            Method::GET,
            &format!("/admin/openai/accounts/{account_id}/quota"),
        )
        .await?;
        if let Some(snapshot) = parse_force_quota(account_id, account_name.clone(), &active) {
            return Ok(snapshot);
        }
    }

    parse_cached_quota(account_id, account_name, &account).ok_or_else(|| {
        PetError::Api("该账号还没有可用的 Codex 周额度数据，请双击宠物主动刷新".into())
    })
}

#[tauri::command]
async fn refresh_pool_quotas(
    state: tauri::State<'_, ApiState>,
    force: bool,
) -> Result<Vec<AccountQuotaRow>, PetError> {
    let items = fetch_pool_account_items(&state).await?;
    let mut accounts: Vec<(PoolAccount, Value)> = items
        .into_iter()
        .filter_map(|item| account_from_value(&item).map(|account| (account, item)))
        .collect();
    let platform_rank = |platform: &str| match platform {
        "anthropic" | "claude" => 0,
        "xai" | "grok" => 1,
        _ => 2,
    };
    accounts.sort_by(|(a, _), (b, _)| {
        (a.status != "active")
            .cmp(&(b.status != "active"))
            .then_with(|| platform_rank(&a.platform).cmp(&platform_rank(&b.platform)))
            .then_with(|| a.name.cmp(&b.name))
    });

    if accounts.is_empty() {
        return Err(PetError::Api(
            "账号池中没有可用的 Codex、Claude 或 Grok 账号".into(),
        ));
    }

    // Fetch quotas with bounded concurrency so large pools don't block the UI
    // for tens of seconds on Windows (WebView2 stays responsive while we wait).
    let mut rows = Vec::with_capacity(accounts.len());
    for chunk in accounts.chunks(QUOTA_FETCH_CONCURRENCY) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|(account, item)| {
                let client = state.client.clone();
                let account = account.clone();
                let item = item.clone();
                async move {
                    let local = ApiState { client };
                    quota_for_account(&local, &account, Some(&item), force).await
                }
            })
            .collect();
        rows.extend(join_all(futs).await);
    }
    Ok(rows)
}

#[tauri::command]
fn has_session() -> bool {
    load_tokens().is_ok()
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
async fn logout(state: tauri::State<'_, ApiState>) -> Result<(), PetError> {
    if let Ok(tokens) = load_tokens() {
        if let Some(refresh_token) = tokens.refresh_token {
            let _ = state
                .client
                .post(format!("{}/auth/logout", tokens.base_url))
                .bearer_auth(tokens.access_token)
                .json(&json!({ "refresh_token": refresh_token }))
                .send()
                .await;
        }
    }
    match keyring_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(PetError::Keyring(error.to_string())),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TrayWindowPayload {
    label: String,
    remaining_percent: Option<f64>,
    reset_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrayAccountPayload {
    id: i64,
    name: String,
    platform: String,
    status: String,
    windows: Vec<TrayWindowPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TrayMenuPayload {
    accounts: Vec<TrayAccountPayload>,
    /// RFC3339 timestamp of the latest quota sync.
    synced_at: Option<String>,
    /// Frontend is mid force-refresh (syncing indicator).
    #[serde(default)]
    refreshing: bool,
}

/// Caches the latest tray payload so the account-panel window can fetch it on open.
/// `menu_fingerprint` avoids full native menu rebuilds when only the refreshing flag toggles.
#[derive(Default)]
struct TrayUiState {
    payload: Mutex<TrayMenuPayload>,
    menu_fingerprint: Mutex<String>,
}

/// Fingerprint of tray *menu structure* (accounts + labels), ignoring `refreshing`.
/// Rebuilding the Win32 tray menu while open makes clicks miss / lag — only rebuild when needed.
fn tray_menu_fingerprint(payload: &TrayMenuPayload) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(256);
    let _ = write!(out, "synced={:?}|", payload.synced_at);
    for account in &payload.accounts {
        let _ = write!(
            out,
            "{}:{}:{}:{}",
            account.id, account.platform, account.status, account.name
        );
        for window in &account.windows {
            let _ = write!(
                out,
                "[{}/{:?}/{:?}]",
                window.label, window.remaining_percent, window.reset_at
            );
        }
        out.push(';');
    }
    out
}

fn platform_display_name(platform: &str) -> &'static str {
    match platform {
        "anthropic" => "Claude",
        "openai" => "Codex",
        "xai" | "grok" => "Grok",
        _ => "账号",
    }
}

fn window_display_name(label: &str) -> &str {
    match label {
        "5h" => "5小时",
        "7d" => "7天",
        other => other,
    }
}

fn lowest_remaining(windows: &[TrayWindowPayload]) -> Option<f64> {
    windows
        .iter()
        .filter_map(|window| window.remaining_percent)
        .reduce(f64::min)
}

fn tray_status_dot(platform: &str, remaining: Option<f64>) -> &'static str {
    if remaining.is_some_and(|value| value <= 15.0) {
        return "🔴";
    }
    match platform {
        "anthropic" => "🟢",
        "openai" => "🔵",
        "xai" | "grok" => "⚫",
        _ => "⚪",
    }
}

fn format_tray_reset(reset_at: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(reset_at)
        .ok()
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            // Accept timestamps without offset as UTC.
            DateTime::parse_from_rfc3339(&format!("{reset_at}Z"))
                .ok()
                .map(|value| value.with_timezone(&Utc))
        })?;
    let local = dt.with_timezone(&chrono::Local);
    let weekday = match local.format("%u").to_string().as_str() {
        "1" => "周一",
        "2" => "周二",
        "3" => "周三",
        "4" => "周四",
        "5" => "周五",
        "6" => "周六",
        _ => "周日",
    };
    Some(format!("{} {}", weekday, local.format("%H:%M")))
}

fn format_tray_synced_at(synced_at: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(synced_at)
        .ok()
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            DateTime::parse_from_rfc3339(&format!("{synced_at}Z"))
                .ok()
                .map(|value| value.with_timezone(&Utc))
        })?;
    let local = dt.with_timezone(&chrono::Local);
    Some(format!("同步于 {}", local.format("%H:%M")))
}

/// Ten-segment Unicode meter for the tray menu (█ filled = remaining, ░ empty).
fn quota_bar(remaining: Option<f64>) -> String {
    const SEGMENTS: usize = 10;
    match remaining {
        Some(value) => {
            let clamped = value.clamp(0.0, 100.0);
            let filled = ((clamped / 100.0) * SEGMENTS as f64).round() as usize;
            let filled = filled.min(SEGMENTS);
            format!("{}{}", "█".repeat(filled), "░".repeat(SEGMENTS - filled))
        }
        None => "░".repeat(SEGMENTS),
    }
}

fn tray_window_segment(window: &TrayWindowPayload) -> String {
    let percent = window
        .remaining_percent
        .map(|value| format!("{}%", value.round().clamp(0.0, 100.0) as i64))
        .unwrap_or_else(|| "--%".into());
    let reset = window
        .reset_at
        .as_deref()
        .and_then(format_tray_reset)
        // Compact "周一 08:00" → "周一08:00" so the bar row stays tight.
        .map(|value| format!(" · {}", value.replace(' ', "")))
        .unwrap_or_default();
    format!(
        "{} {} {}{}",
        window_display_name(&window.label),
        quota_bar(window.remaining_percent),
        percent,
        reset
    )
}

/// One menu line per quota window so Claude shows both 5h and 7d.
fn tray_window_labels(account: &TrayAccountPayload) -> Vec<(String, String)> {
    let inactive = if account.status != "active" {
        " · 停用"
    } else {
        ""
    };
    let platform = platform_display_name(&account.platform);
    let lowest = lowest_remaining(&account.windows);
    let dot = tray_status_dot(&account.platform, lowest);

    if account.windows.is_empty() {
        return vec![(
            format!("account-{}", account.id),
            format!(
                "{} {} {} · {} --%{}",
                dot,
                platform,
                account.name,
                quota_bar(None),
                inactive
            ),
        )];
    }

    // Claude (and multi-window accounts): one tray row per window.
    if account.windows.len() > 1 {
        return account
            .windows
            .iter()
            .enumerate()
            .map(|(index, window)| {
                let window_dot = tray_status_dot(&account.platform, window.remaining_percent);
                (
                    format!("account-{}-{}", account.id, index),
                    format!(
                        "{} {} {} · {}{}",
                        window_dot,
                        platform,
                        account.name,
                        tray_window_segment(window),
                        inactive
                    ),
                )
            })
            .collect();
    }

    // Single-window accounts (typical Codex): one compact row.
    let window = &account.windows[0];
    vec![(
        format!("account-{}", account.id),
        format!(
            "{} {} {} · {}{}",
            dot,
            platform,
            account.name,
            tray_window_segment(window),
            inactive
        ),
    )]
}

fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(target_os = "macos")]
        macos_window::configure_spaces_window(&window);
        anchor_window_to_right(&window, None);
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Legacy label kept so any old secondary process can still be closed.
const ACTION_MENU_LABEL: &str = "action-menu";

fn handle_menu_action<R: Runtime>(app: &AppHandle<R>, id: &str) {
    match id {
        "pool" | "show" | "open-panel" => {
            let _ = show_account_panel_window(app);
        }
        "refresh" => {
            show_main_window(app);
            let _ = app.emit("pet-menu-action", "refresh");
        }
        "open-admin" => {
            show_main_window(app);
            let _ = app.emit("open-admin", ());
        }
        "settings" => {
            show_main_window(app);
            let _ = app.emit("open-settings", ());
        }
        "quit" => app.exit(0),
        "empty" => {}
        other if other.starts_with("account-") => {
            let _ = show_account_panel_window(app);
        }
        _ => {}
    }
}

fn build_action_menu<R: Runtime>(app: &AppHandle<R>) -> Result<Menu<R>, tauri::Error> {
    let panel = MenuItem::with_id(app, "open-panel", "打开账号面板", true, None::<&str>)?;
    let refresh = MenuItem::with_id(app, "refresh", "刷新额度", true, None::<&str>)?;
    let admin = MenuItem::with_id(app, "open-admin", "打开网页管理", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出应用", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    Menu::with_items(
        app,
        &[
            &panel,
            &refresh,
            &admin,
            &settings,
            &sep,
            &quit,
        ],
    )
}

/// Native context menu at the cursor — avoids a second transparent WebView (very slow / flaky on Windows).
///
/// Uses `popup_menu` (cursor position) rather than `popup_menu_at` with screen coords.
/// `popup_menu_at` expects coordinates **relative to the window's top-left**, so passing
/// absolute screen pixels (from JS `cursorPosition`) made the menu appear far below the pet
/// on macOS (and any display where the window is not at origin).
#[tauri::command]
fn show_action_menu(app: AppHandle) -> Result<(), PetError> {
    // Close any legacy transparent action-menu window from older builds.
    if let Some(window) = app.get_webview_window(ACTION_MENU_LABEL) {
        let _ = window.close();
    }

    let Some(window) = app.get_webview_window("main") else {
        return Err(PetError::Api("主窗口不可用".into()));
    };

    let menu = build_action_menu(&app).map_err(|error| PetError::Api(error.to_string()))?;
    // Native path: resolve cursor on the OS side so position matches the pet under the pointer.
    window
        .popup_menu(&menu)
        .map_err(|error| PetError::Api(error.to_string()))?;
    Ok(())
}

#[tauri::command]
fn hide_action_menu(app: AppHandle) -> Result<(), PetError> {
    if let Some(window) = app.get_webview_window(ACTION_MENU_LABEL) {
        let _ = window.close();
    }
    Ok(())
}

const SETTINGS_LABEL: &str = "settings";
const SETTINGS_WIDTH: f64 = 380.0;
const SETTINGS_HEIGHT: f64 = 640.0;

fn ensure_settings_window<R: Runtime>(app: &AppHandle<R>) -> Result<(), PetError> {
    if app.get_webview_window(SETTINGS_LABEL).is_some() {
        return Ok(());
    }

    WebviewWindowBuilder::new(app, SETTINGS_LABEL, WebviewUrl::App("settings.html".into()))
        .title("Aira 设置")
        .inner_size(SETTINGS_WIDTH, SETTINGS_HEIGHT)
        .decorations(true)
        .transparent(false)
        .always_on_top(true)
        .skip_taskbar(false)
        .resizable(false)
        .focused(false)
        .visible(false)
        .center()
        .build()
        .map_err(|error| PetError::Api(error.to_string()))?;
    Ok(())
}

/// Dedicated settings dialog so the transparent pet window stays visible and undocked.
#[tauri::command]
fn show_settings_window(app: AppHandle) -> Result<(), PetError> {
    ensure_settings_window(&app)?;

    let Some(window) = app.get_webview_window(SETTINGS_LABEL) else {
        return Err(PetError::Api("设置窗口创建失败".into()));
    };

    window
        .show()
        .map_err(|error| PetError::Api(error.to_string()))?;
    window
        .set_focus()
        .map_err(|error| PetError::Api(error.to_string()))?;
    let _ = window.center();
    // HTML ships a static skeleton so show() is never a blank frame; JS re-hydrates on this event.
    let _ = window.emit("settings-window-shown", ());
    Ok(())
}

#[tauri::command]
fn hide_settings_window(app: AppHandle) -> Result<(), PetError> {
    if let Some(window) = app.get_webview_window(SETTINGS_LABEL) {
        window
            .hide()
            .map_err(|error| PetError::Api(error.to_string()))?;
        let _ = app.emit("settings-closed", ());
    }
    Ok(())
}

const GITHUB_LATEST_RELEASE: &str =
    "https://api.github.com/repos/boycott96/sub2api-token/releases/latest";

#[derive(Debug, Serialize)]
struct AppUpdateInfo {
    available: bool,
    current_version: String,
    latest_version: Option<String>,
    html_url: Option<String>,
    download_url: Option<String>,
    notes: Option<String>,
}

fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim().trim_start_matches('v').trim_start_matches('V');
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    // Allow trailing pre-release suffix on patch: "4-beta" → 4
    let patch_raw = parts.next().unwrap_or("0");
    let patch = patch_raw
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => {
            let l = latest.trim().trim_start_matches(['v', 'V']);
            let c = current.trim().trim_start_matches(['v', 'V']);
            !l.is_empty() && l != c
        }
    }
}

fn pick_release_asset(assets: &[Value]) -> Option<String> {
    let urls: Vec<(&str, &str)> = assets
        .iter()
        .filter_map(|asset| {
            let name = asset.get("name").and_then(Value::as_str)?;
            let url = asset
                .get("browser_download_url")
                .and_then(Value::as_str)?;
            Some((name, url))
        })
        .collect();

    // Prefer installers for the current OS.
    #[cfg(target_os = "windows")]
    let prefs: &[&str] = &["-setup.exe", ".msi", ".exe"];
    #[cfg(target_os = "macos")]
    let prefs: &[&str] = &[".dmg", ".app.tar.gz", ".zip"];
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let prefs: &[&str] = &[".AppImage", ".deb", ".rpm"];

    for suffix in prefs {
        if let Some((_, url)) = urls
            .iter()
            .find(|(name, _)| name.ends_with(suffix) && !name.contains("latest.json"))
        {
            return Some((*url).to_string());
        }
    }
    urls.first().map(|(_, url)| (*url).to_string())
}

/// Check GitHub Releases for a newer app version (no signed updater required).
#[tauri::command]
async fn check_app_update(app: AppHandle) -> Result<AppUpdateInfo, PetError> {
    let current_version = app.package_info().version.to_string();
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(format!("Aira/{}", current_version))
        .build()?;

    let response = client.get(GITHUB_LATEST_RELEASE).send().await?;
    if !response.status().is_success() {
        return Err(PetError::Api(format!(
            "检查更新失败（HTTP {}）",
            response.status()
        )));
    }

    let release: Value = response.json().await.map_err(|_| PetError::InvalidResponse)?;
    let tag = release
        .get("tag_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let latest_version = tag.trim().trim_start_matches(['v', 'V']).to_string();
    if latest_version.is_empty() {
        return Err(PetError::Api("未找到最新版本信息".into()));
    }

    let available = is_newer_version(&latest_version, &current_version);
    let html_url = release
        .get("html_url")
        .and_then(Value::as_str)
        .map(str::to_string);
    let notes = release
        .get("body")
        .and_then(Value::as_str)
        .map(|body| body.chars().take(400).collect::<String>());
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .map(|items| items.as_slice())
        .unwrap_or(&[]);
    let download_url = if available {
        pick_release_asset(assets).or_else(|| html_url.clone())
    } else {
        None
    };

    Ok(AppUpdateInfo {
        available,
        current_version,
        latest_version: Some(latest_version),
        html_url,
        download_url,
        notes,
    })
}

fn build_tray_menu<R: Runtime>(
    app: &AppHandle<R>,
    payload: &TrayMenuPayload,
) -> Result<Menu<R>, tauri::Error> {
    let pool = MenuItem::with_id(app, "pool", "账号池", true, None::<&str>)?;
    let mut items: Vec<Box<dyn tauri::menu::IsMenuItem<R>>> = vec![Box::new(pool)];

    if let Some(synced) = payload.synced_at.as_deref().and_then(format_tray_synced_at) {
        items.push(Box::new(MenuItem::with_id(
            app,
            "synced-at",
            format!("  {synced}"),
            false,
            None::<&str>,
        )?));
    }

    if payload.accounts.is_empty() {
        items.push(Box::new(MenuItem::with_id(
            app,
            "empty",
            "  暂无账号数据",
            false,
            None::<&str>,
        )?));
    } else {
        for account in &payload.accounts {
            for (id, label) in tray_window_labels(account) {
                items.push(Box::new(MenuItem::with_id(
                    app,
                    id,
                    label,
                    true,
                    None::<&str>,
                )?));
            }
        }
    }

    items.push(Box::new(PredefinedMenuItem::separator(app)?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "open-panel",
        "打开账号面板",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "refresh",
        "刷新额度",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "open-admin",
        "打开网页管理",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "settings",
        "设置",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "quit",
        "退出应用",
        true,
        None::<&str>,
    )?));

    let refs: Vec<&dyn tauri::menu::IsMenuItem<R>> =
        items.iter().map(|item| item.as_ref()).collect();
    Menu::with_items(app, &refs)
}

fn tray_counts(payload: &TrayMenuPayload) -> (usize, usize, usize) {
    let total = payload.accounts.len();
    let online = payload
        .accounts
        .iter()
        .filter(|account| account.status == "active")
        .count();
    let abnormal = payload
        .accounts
        .iter()
        .filter(|account| {
            if account.status != "active" {
                return true;
            }
            lowest_remaining(&account.windows).is_some_and(|value| value <= 0.0)
        })
        .count();
    (total, online, abnormal)
}

fn tray_overall_remaining(payload: &TrayMenuPayload) -> Option<f64> {
    let values: Vec<f64> = payload
        .accounts
        .iter()
        .filter(|account| account.status == "active")
        .filter_map(|account| lowest_remaining(&account.windows))
        .collect();
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// One-line quota status for external Touch Bar helpers (MTMR / BetterTouchTool).
fn touchbar_status_line(payload: &TrayMenuPayload) -> String {
    let (total, online, abnormal) = tray_counts(payload);
    let overall = tray_overall_remaining(payload)
        .map(|value| format!("{}%", value.round().clamp(0.0, 100.0) as i64))
        .unwrap_or_else(|| "--%".into());

    if payload.refreshing {
        return "同步中…".to_string();
    }
    if total == 0 {
        return "暂无账号".to_string();
    }
    let mut line = format!("额度 {overall} · 在线 {online}/{total}");
    if abnormal > 0 {
        line.push_str(&format!(" · 异常 {abnormal}"));
    }
    line
}

/// Publish the quota to a plain-text file so a persistent Touch Bar helper can
/// `cat` it and stay always-on. Free helpers (MTMR) and paid ones (BetterTouchTool)
/// both consume this same file.
#[cfg(target_os = "macos")]
fn publish_touchbar_status_file(payload: &TrayMenuPayload) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = std::path::PathBuf::from(home).join(".sub2api-pet-quota.txt");
    let _ = std::fs::write(path, format!("{}\n", touchbar_status_line(payload)));
}

fn apply_tray_title_and_tooltip<R: Runtime>(
    app: &AppHandle<R>,
    payload: &TrayMenuPayload,
) -> Result<(), PetError> {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return Ok(());
    };
    let (total, online, abnormal) = tray_counts(payload);
    let overall = tray_overall_remaining(payload)
        .map(|value| format!("{}%", value.round().clamp(0.0, 100.0) as i64))
        .unwrap_or_else(|| "--%".into());
    let tooltip = if payload.refreshing {
        format!("Aira · 同步中… · {online}/{total}")
    } else if total == 0 {
        "Aira · 暂无账号".into()
    } else {
        format!("Aira · {online}/{total} 在线 · {abnormal} 异常 · 额度 {overall}")
    };
    // Menu-bar text beside the tray icon is disabled per user request (icon only, no 50% text).
    #[cfg(target_os = "macos")]
    {
        tray.set_title(None::<&str>)
            .map_err(|error| PetError::Api(error.to_string()))?;
    }
    tray.set_tooltip(Some(&tooltip))
        .map_err(|error| PetError::Api(error.to_string()))?;

    // Keep the native Touch Bar (macOS) in sync with the latest quota.
    #[cfg(target_os = "macos")]
    touchbar::apply(app, payload);

    // Publish the same status for persistent Touch Bar helpers (MTMR / BTT).
    #[cfg(target_os = "macos")]
    publish_touchbar_status_file(payload);

    Ok(())
}

fn apply_tray_menu<R: Runtime>(
    app: &AppHandle<R>,
    state: &TrayUiState,
    payload: &TrayMenuPayload,
) -> Result<(), PetError> {
    let fingerprint = tray_menu_fingerprint(payload);
    let menu_changed = {
        match state.menu_fingerprint.lock() {
            Ok(mut last) => {
                if *last != fingerprint {
                    *last = fingerprint;
                    true
                } else {
                    false
                }
            }
            Err(_) => true,
        }
    };

    // Only rebuild the native tray menu when account rows actually change.
    // Toggling `refreshing` alone used to replace the whole menu twice per sync,
    // which freezes Win32 tray menus and drops clicks mid-interaction.
    if menu_changed {
        let menu =
            build_tray_menu(app, payload).map_err(|error| PetError::Api(error.to_string()))?;
        if let Some(tray) = app.tray_by_id(TRAY_ID) {
            tray.set_menu(Some(menu))
                .map_err(|error| PetError::Api(error.to_string()))?;
        }
    }

    apply_tray_title_and_tooltip(app, payload)?;
    // Push live data to the account-panel window if it is open.
    if let Some(window) = app.get_webview_window(ACCOUNT_PANEL_LABEL) {
        let _ = window.emit("tray-data", payload);
    }
    Ok(())
}

#[tauri::command]
fn update_tray_menu(
    app: tauri::AppHandle,
    state: tauri::State<'_, TrayUiState>,
    payload: TrayMenuPayload,
) -> Result<(), PetError> {
    if let Ok(mut guard) = state.payload.lock() {
        *guard = payload.clone();
    }
    apply_tray_menu(&app, state.inner(), &payload)
}

#[tauri::command]
fn get_tray_payload(state: tauri::State<'_, TrayUiState>) -> TrayMenuPayload {
    state
        .payload
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

fn center_popup_position<R: Runtime>(app: &AppHandle<R>, w: i32, h: i32) -> PhysicalPosition<i32> {
    let monitor = app
        .get_webview_window("main")
        .and_then(|window| window.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    if let Some(monitor) = monitor {
        let area = monitor.work_area();
        let x = area.position.x + (area.size.width as i32 - w).max(0) / 2;
        let y = area.position.y + (area.size.height as i32 - h).max(0) / 2;
        return PhysicalPosition::new(x, y);
    }
    PhysicalPosition::new(120, 80)
}

fn anchor_window_to_right<R: Runtime>(window: &WebviewWindow<R>, target_y: Option<i32>) {
    let _ = window.set_shadow(false);
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten());
    if let Some(monitor) = monitor {
        let area = monitor.work_area();
        let margin = 0;
        if let Ok(win_size) = window.outer_size() {
            let x = area.position.x + (area.size.width as i32 - win_size.width as i32 - margin);
            let max_y = (area.position.y + area.size.height as i32 - win_size.height as i32).max(area.position.y);
            let y = match target_y {
                Some(ty) => ty.clamp(area.position.y, max_y),
                None => area.position.y + (area.size.height as i32 - win_size.height as i32).max(0) / 2,
            };
            let _ = window.set_position(PhysicalPosition::new(x, y));
        }
    }
}

#[tauri::command]
fn anchor_main_window_right<R: Runtime>(app: AppHandle<R>, y: Option<i32>) -> Result<(), PetError> {
    if let Some(window) = app.get_webview_window("main") {
        anchor_window_to_right(&window, y);
    }
    Ok(())
}

fn ensure_account_panel_window<R: Runtime>(app: &AppHandle<R>) -> Result<(), PetError> {
    if app.get_webview_window(ACCOUNT_PANEL_LABEL).is_some() {
        return Ok(());
    }

    WebviewWindowBuilder::new(app, ACCOUNT_PANEL_LABEL, WebviewUrl::App("panel.html".into()))
        .title("AI 账号池")
        .inner_size(ACCOUNT_PANEL_WIDTH, ACCOUNT_PANEL_HEIGHT)
        .min_inner_size(ACCOUNT_PANEL_MIN_WIDTH, ACCOUNT_PANEL_MIN_HEIGHT)
        .decorations(true)
        .transparent(false)
        .always_on_top(false)
        .skip_taskbar(false)
        .resizable(true)
        .focused(false)
        .visible(false)
        .build()
        .map_err(|error| PetError::Api(error.to_string()))?;
    Ok(())
}

/// Normal OS window listing every account's quota. Reuses the window if already built.
fn show_account_panel_window<R: Runtime>(app: &AppHandle<R>) -> Result<(), PetError> {
    ensure_account_panel_window(app)?;

    let w = ACCOUNT_PANEL_WIDTH.round() as i32;
    let h = ACCOUNT_PANEL_HEIGHT.round() as i32;
    let position = center_popup_position(app, w, h);

    let Some(window) = app.get_webview_window(ACCOUNT_PANEL_LABEL) else {
        return Err(PetError::Api("账号面板创建失败".into()));
    };

    window
        .set_position(Position::Physical(position))
        .map_err(|error| PetError::Api(error.to_string()))?;
    window
        .show()
        .map_err(|error| PetError::Api(error.to_string()))?;
    window
        .set_focus()
        .map_err(|error| PetError::Api(error.to_string()))?;
    // Skeleton HTML is already painted; panel.ts reloads tray payload on this event.
    let _ = window.emit("tray-panel-shown", ());

    // Also push the latest cached payload so the panel is never empty after open.
    if let Some(state) = app.try_state::<TrayUiState>() {
        if let Ok(payload) = state.payload.lock() {
            let _ = window.emit("tray-data", &*payload);
        }
    }
    Ok(())
}

#[tauri::command]
fn show_account_panel(app: AppHandle) -> Result<(), PetError> {
    show_account_panel_window(&app)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("Aira/0.1")
        .build()
        .expect("failed to build HTTP client");

    tauri::Builder::default()
        .manage(ApiState { client })
        .manage(TrayUiState::default())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin({
            #[cfg(target_os = "macos")]
            {
                tauri_plugin_autostart::Builder::new()
                    .macos_launcher(tauri_plugin_autostart::MacosLauncher::LaunchAgent)
                    .build()
            }
            #[cfg(not(target_os = "macos"))]
            {
                tauri_plugin_autostart::Builder::new().build()
            }
        })
        .plugin(tauri_plugin_opener::init())
        // Context menus (native pet right-click) share the same ids as the tray menu.
        .on_menu_event(|app, event| {
            handle_menu_action(app, event.id().as_ref());
        })
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                macos_window::speed_up_tooltips();
                let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                if let Some(main_win) = app.get_webview_window("main") {
                    macos_window::configure_spaces_window(&main_win);
                    anchor_window_to_right(&main_win, None);
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                if let Some(main_win) = app.get_webview_window("main") {
                    anchor_window_to_right(&main_win, None);
                }
            }

            let empty = TrayMenuPayload::default();
            let menu = build_tray_menu(&app.handle(), &empty)?;
            // Dedicated 64x64 tray art fills the menu-bar slot better than the window icon
            // (less empty padding, higher subject contrast at 18–22pt). Unchanged pet art.
            let tray_icon = Image::from_bytes(include_bytes!("../icons/tray-icon-color.png"))
                .expect("tray icon");
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(tray_icon)
                .tooltip("Aira")
                .menu(&menu)
                // Native menu on both left and right click.
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    handle_menu_action(app, event.id.as_ref());
                })
                .build(app)?;
            let _ = apply_tray_title_and_tooltip(&app.handle(), &empty);

            #[cfg(target_os = "macos")]
            touchbar::setup();

            // Warm secondary WebViews at startup so the first open is not a cold blank frame
            // (WebView2 on Windows is especially slow to create on demand).
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                // Small delay so the pet window paints first.
                std::thread::sleep(Duration::from_millis(400));
                let h = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    let _ = ensure_settings_window(&h);
                    let _ = ensure_account_panel_window(&h);
                });
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    if window.label() == "main" {
                        // Pet never quits from the close chrome (it has none); tray owns lifecycle.
                        let _ = window.emit("main-close-requested", ());
                    } else if window.label() == SETTINGS_LABEL {
                        let _ = window.hide();
                        let _ = window.app_handle().emit("settings-closed", ());
                    } else {
                        let _ = window.hide();
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            login,
            complete_login,
            list_codex_accounts,
            refresh_quota,
            refresh_pool_quotas,
            has_session,
            logout,
            quit_app,
            update_tray_menu,
            get_tray_payload,
            show_account_panel,
            show_action_menu,
            hide_action_menu,
            show_settings_window,
            hide_settings_window,
            check_app_update,
            anchor_main_window_right
        ])
        .run(tauri::generate_context!())
        .expect("error while running Aira");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_last_used_at_reaches_quota_row() {
        let item = json!({
            "id": 7,
            "name": "Codex Main",
            "status": "active",
            "platform": "openai",
            "type": "oauth",
            "last_used_at": "2026-09-24T01:00:00Z"
        });
        let account = account_from_value(&item).unwrap();
        let row = empty_row(&account);
        assert_eq!(row.last_used_at.as_deref(), Some("2026-09-24T01:00:00Z"));
    }

    #[test]
    fn cached_quota_prefers_canonical_weekly_fields() {
        let account = json!({
            "extra": {
                "codex_7d_used_percent": 42.5,
                "codex_7d_reset_at": "2026-07-26T08:00:00Z",
                "codex_primary_used_percent": 91.0,
                "codex_primary_window_minutes": 300,
                "codex_usage_updated_at": "2026-07-22T08:00:00Z"
            }
        });

        let snapshot = parse_cached_quota(7, "Main".into(), &account).unwrap();
        assert_eq!(snapshot.used_percent, 42.5);
        assert_eq!(snapshot.remaining_percent, 57.5);
        assert_eq!(snapshot.reset_at.as_deref(), Some("2026-07-26T08:00:00Z"));
        assert_eq!(snapshot.window_label.as_deref(), Some("7d"));
    }

    #[test]
    fn cached_quota_uses_the_longest_legacy_window() {
        let account = json!({
            "extra": {
                "codex_primary_used_percent": 38.0,
                "codex_primary_window_minutes": 10080,
                "codex_primary_reset_after_seconds": 3600,
                "codex_secondary_used_percent": 79.0,
                "codex_secondary_window_minutes": 300
            }
        });

        let snapshot = parse_cached_quota(7, "Legacy".into(), &account).unwrap();
        assert_eq!(snapshot.used_percent, 38.0);
        assert_eq!(snapshot.remaining_percent, 62.0);
    }

    #[test]
    fn active_quota_uses_the_longest_server_window() {
        let quota = json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 82.0,
                    "limit_window_seconds": 18000,
                    "reset_at": 1785052800
                },
                "secondary_window": {
                    "used_percent": 27.0,
                    "limit_window_seconds": 604800,
                    "reset_at": 1785571200
                }
            }
        });

        let snapshot = parse_force_quota(9, "Team".into(), &quota).unwrap();
        assert_eq!(snapshot.used_percent, 27.0);
        assert_eq!(snapshot.remaining_percent, 73.0);
        assert_eq!(snapshot.source, "active");
    }

    #[test]
    fn grok_quota_prefers_weekly_billing_period() {
        let account = json!({
            "extra": {
                "grok_billing_snapshot": {
                    "period_type": "weekly",
                    "period_end": "2026-07-29T03:04:09Z",
                    "usage_percent": 43,
                    "used_percent": 27.32,
                    "weekly_updated_at": "2026-07-24T09:18:52Z"
                }
            }
        });

        let snapshot = parse_grok_quota(13, "Grok Main".into(), &account, "cached").unwrap();
        assert_eq!(snapshot.used_percent, 43.0);
        assert_eq!(snapshot.remaining_percent, 57.0);
        assert_eq!(snapshot.window_label.as_deref(), Some("7d"));
        assert_eq!(snapshot.reset_at.as_deref(), Some("2026-07-29T03:04:09Z"));
        assert_eq!(snapshot.updated_at, "2026-07-24T09:18:52Z");
    }

    #[test]
    fn grok_quota_falls_back_to_monthly_included_usage() {
        let account = json!({
            "extra": {
                "grok_billing_snapshot": {
                    "billing_period_end": "2026-08-01T00:00:00Z",
                    "used_percent": "27.32",
                    "monthly_updated_at": "2026-07-24T09:18:52Z"
                }
            }
        });

        let snapshot = parse_grok_quota(13, "Grok Main".into(), &account, "cached").unwrap();
        assert_eq!(snapshot.used_percent, 27.32);
        assert!((snapshot.remaining_percent - 72.68).abs() < f64::EPSILON);
        assert_eq!(snapshot.window_label.as_deref(), Some("月"));
        assert_eq!(snapshot.reset_at.as_deref(), Some("2026-08-01T00:00:00Z"));
    }

    #[test]
    fn grok_quota_requires_a_billing_snapshot() {
        let account = json!({
            "extra": {
                "grok_usage_snapshot": {
                    "requests": { "limit": 8300, "remaining": 8300 }
                }
            }
        });

        assert!(parse_grok_quota(13, "Grok Main".into(), &account, "cached").is_none());
    }

    #[test]
    fn claude_usage_prefers_weekly_window() {
        let usage = json!({
            "updated_at": "2026-07-22T10:00:00Z",
            "five_hour": {
                "utilization": 88.0,
                "resets_at": "2026-07-22T14:00:00Z",
                "remaining_seconds": 3600
            },
            "seven_day": {
                "utilization": 41.0,
                "resets_at": "2026-07-28T08:00:00Z",
                "remaining_seconds": 500000
            },
            "seven_day_sonnet": {
                "utilization": 12.0,
                "resets_at": "2026-07-28T08:00:00Z",
                "remaining_seconds": 500000
            }
        });

        let snapshot = parse_usage_quota(3, "Claude Main".into(), &usage, "cached").unwrap();
        assert_eq!(snapshot.used_percent, 41.0);
        assert_eq!(snapshot.remaining_percent, 59.0);
        assert_eq!(snapshot.window_label.as_deref(), Some("7d"));
        assert_eq!(snapshot.reset_at.as_deref(), Some("2026-07-28T08:00:00Z"));
    }

    #[test]
    fn claude_usage_falls_back_to_five_hour_window() {
        let usage = json!({
            "updated_at": "2026-07-22T10:00:00Z",
            "five_hour": {
                "utilization": 55.0,
                "resets_at": "2026-07-22T14:00:00Z",
                "remaining_seconds": 3600
            },
            "seven_day": null
        });

        let snapshot = parse_usage_quota(4, "Claude Setup".into(), &usage, "active").unwrap();
        assert_eq!(snapshot.used_percent, 55.0);
        assert_eq!(snapshot.remaining_percent, 45.0);
        assert_eq!(snapshot.window_label.as_deref(), Some("5h"));
        assert_eq!(snapshot.source, "active");
    }

    #[test]
    fn claude_usage_keeps_fully_depleted_windows() {
        let usage = json!({
            "updated_at": "2026-07-22T10:00:00Z",
            "five_hour": {
                "utilization": 100.0,
                "resets_at": "2026-07-22T14:00:00Z",
                "remaining_seconds": 1200
            },
            "seven_day": {
                "utilization": "100",
                "resets_at": "2026-07-28T08:00:00Z"
            }
        });

        let account = PoolAccount {
            id: 5,
            name: "Claude Depleted".into(),
            status: "active".into(),
            plan: Some("max".into()),
            platform: "anthropic".into(),
            account_type: "oauth".into(),
            last_used_at: None,
        };
        let row = usage_to_row(&account, &usage, "cached");
        assert_eq!(row.windows.len(), 2);
        assert_eq!(row.windows[0].label, "5h");
        assert_eq!(row.windows[0].remaining_percent, 0.0);
        assert_eq!(row.windows[1].label, "7d");
        assert_eq!(row.windows[1].remaining_percent, 0.0);
        assert_eq!(row.remaining_percent, Some(0.0));
    }

    #[test]
    fn claude_usage_accepts_remaining_percent_zero() {
        let window = parse_usage_window(
            &json!({ "remaining_percent": 0, "resets_at": "2026-07-22T14:00:00Z" }),
            "5h",
        )
        .unwrap();
        assert_eq!(window.used_percent, 100.0);
        assert_eq!(window.remaining_percent, 0.0);
    }

    #[test]
    fn version_compare_detects_newer_release() {
        assert!(is_newer_version("0.1.5", "0.1.4"));
        assert!(is_newer_version("v0.2.0", "0.1.9"));
        assert!(!is_newer_version("0.1.4", "0.1.4"));
        assert!(!is_newer_version("0.1.3", "0.1.4"));
        assert!(is_newer_version("1.0.0", "0.9.9"));
    }

    #[test]
    fn pick_release_asset_prefers_platform_installer() {
        let assets = vec![
            json!({
                "name": "Aira_0.1.5_amd64.AppImage",
                "browser_download_url": "https://example.com/appimage"
            }),
            json!({
                "name": "Aira_0.1.5_x64-setup.exe",
                "browser_download_url": "https://example.com/setup.exe"
            }),
            json!({
                "name": "Aira_0.1.5_universal.dmg",
                "browser_download_url": "https://example.com/app.dmg"
            }),
        ];
        let picked = pick_release_asset(&assets).unwrap();
        #[cfg(target_os = "windows")]
        assert!(picked.ends_with("setup.exe"));
        #[cfg(target_os = "macos")]
        assert!(picked.ends_with(".dmg"));
        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        assert!(picked.ends_with(".AppImage"));
    }
}
