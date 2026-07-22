use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager,
};
use thiserror::Error;

const KEYRING_SERVICE: &str = "com.sub2api.pet";
const KEYRING_USER: &str = "active-session";

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
            tokens = refresh_access_token(state, tokens).await?;
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

fn account_from_value(value: &Value) -> Option<CodexAccount> {
    let platform = value.get("platform")?.as_str()?;
    if platform != "openai" {
        return None;
    }
    let id = value.get("id")?.as_i64()?;
    let name = value.get("name")?.as_str()?.to_string();
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("inactive")
        .to_string();
    let plan = value
        .pointer("/extra/plan_type")
        .or_else(|| value.pointer("/extra/subscription_tier"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(CodexAccount {
        id,
        name,
        status,
        plan,
    })
}

#[tauri::command]
async fn list_codex_accounts(
    state: tauri::State<'_, ApiState>,
) -> Result<Vec<CodexAccount>, PetError> {
    let data = authorized_request(
        &state,
        Method::GET,
        "/admin/accounts?page=1&page_size=100&platform=openai",
    )
    .await?;
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .ok_or(PetError::InvalidResponse)?;
    let mut accounts: Vec<_> = items.iter().filter_map(account_from_value).collect();
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
    })
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
        .unwrap_or("Codex")
        .to_string();

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
fn has_session() -> bool {
    load_tokens().is_ok()
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("Sub2API-Pet/0.1")
        .build()
        .expect("failed to build HTTP client");

    tauri::Builder::default()
        .manage(ApiState { client })
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .macos_launcher(tauri_plugin_autostart::MacosLauncher::LaunchAgent)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let show = MenuItem::with_id(app, "show", "显示宠物", true, None::<&str>)?;
            let settings = MenuItem::with_id(app, "settings", "连接设置", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &settings, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().expect("missing app icon").clone())
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "settings" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                            let _ = window.emit("open-settings", ());
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            login,
            complete_login,
            list_codex_accounts,
            refresh_quota,
            has_session,
            logout
        ])
        .run(tauri::generate_context!())
        .expect("error while running Sub2API Pet");
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
