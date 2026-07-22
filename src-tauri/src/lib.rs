use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, Runtime,
};
use thiserror::Error;

const KEYRING_SERVICE: &str = "com.sub2api.pet";
const KEYRING_USER: &str = "active-session";
const TRAY_ID: &str = "main-tray";

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

fn is_pool_platform(platform: &str) -> bool {
    matches!(platform, "openai" | "anthropic")
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
    let plan = value
        .pointer("/extra/plan_type")
        .or_else(|| value.pointer("/extra/subscription_tier"))
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
    for platform in ["openai", "anthropic"] {
        items.extend(fetch_account_items_for_platform(state, platform).await?);
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

fn parse_usage_window(value: &Value, label: &str) -> Option<QuotaWindow> {
    if value.is_null() || !value.is_object() {
        return None;
    }
    let utilization = value.get("utilization").and_then(Value::as_f64)?;
    let used = utilization.max(0.0);
    Some(QuotaWindow {
        label: label.to_string(),
        used_percent: used.min(100.0),
        remaining_percent: (100.0 - used).max(0.0),
        reset_at: usage_window_reset_at(value),
    })
}

/// Collect Claude-style usage windows for the panel (5h then 7d).
fn parse_usage_windows(data: &Value) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    // Match admin UI order: 5h (session) then 7d (weekly).
    for (key, label) in [("five_hour", "5h"), ("seven_day", "7d")] {
        if let Some(window) = data.get(key).and_then(|value| parse_usage_window(value, label)) {
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
        remaining_percent,
        windows,
        updated_at,
        source,
    }
}

fn snapshot_to_row(snapshot: QuotaSnapshot, account: &PoolAccount) -> AccountQuotaRow {
    let windows = vec![QuotaWindow {
        label: snapshot
            .window_label
            .clone()
            .unwrap_or_else(|| "7d".into()),
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
            if let Some(snapshot) = parse_force_quota(account.id, account.name.clone(), &active) {
                return snapshot_to_row(snapshot, account);
            }
        }
        // OpenAI OAuth may also expose usage windows via the generic usage endpoint.
        if supports_usage_endpoint(account) {
            if let Ok(usage) = authorized_request(
                state,
                Method::GET,
                &format!("/admin/accounts/{}/usage?source=active&force=true", account.id),
            )
            .await
            {
                let row = usage_to_row(account, &usage, "active");
                if !row.windows.is_empty() {
                    // Panel shows a single weekly-style bar for Codex.
                    return collapse_openai_row(row);
                }
            }
        }
    }

    if let Some(item) = list_item {
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
                return collapse_openai_row(row);
            }
        }
    }

    empty_row(account)
}

fn collapse_openai_row(mut row: AccountQuotaRow) -> AccountQuotaRow {
    if row.windows.len() <= 1 {
        return row;
    }
    // Prefer weekly window for Codex display.
    if let Some(weekly) = row
        .windows
        .iter()
        .find(|window| window.label == "7d")
        .cloned()
    {
        row.remaining_percent = Some(weekly.remaining_percent);
        row.windows = vec![weekly];
    } else {
        row.windows.truncate(1);
        row.remaining_percent = row.windows.first().map(|window| window.remaining_percent);
    }
    row
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

async fn quota_for_account(
    state: &ApiState,
    account: &PoolAccount,
    list_item: Option<&Value>,
    force: bool,
) -> AccountQuotaRow {
    match account.platform.as_str() {
        "anthropic" => quota_for_anthropic_account(state, account, force).await,
        _ => quota_for_openai_account(state, account, list_item, force).await,
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
        .unwrap_or("openai");

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
    accounts.sort_by(|(a, _), (b, _)| {
        (a.status != "active")
            .cmp(&(b.status != "active"))
            .then_with(|| a.platform.cmp(&b.platform))
            .then_with(|| a.name.cmp(&b.name))
    });

    if accounts.is_empty() {
        return Err(PetError::Api(
            "账号池中没有可用的 OpenAI/Codex 或 Claude 账号".into(),
        ));
    }

    let mut rows = Vec::with_capacity(accounts.len());
    for (account, item) in &accounts {
        rows.push(quota_for_account(&state, account, Some(item), force).await);
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

#[derive(Debug, Deserialize)]
struct TrayWindowPayload {
    label: String,
    remaining_percent: Option<f64>,
    reset_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TrayAccountPayload {
    id: i64,
    name: String,
    platform: String,
    status: String,
    windows: Vec<TrayWindowPayload>,
}

#[derive(Debug, Deserialize)]
struct TrayMenuPayload {
    accounts: Vec<TrayAccountPayload>,
    /// RFC3339 timestamp of the latest quota sync.
    synced_at: Option<String>,
}

fn platform_display_name(platform: &str) -> &'static str {
    match platform {
        "anthropic" => "Claude",
        "openai" => "Codex",
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

fn tray_window_segment(window: &TrayWindowPayload) -> String {
    let percent = window
        .remaining_percent
        .map(|value| format!("{}%", value.round().clamp(0.0, 100.0) as i64))
        .unwrap_or_else(|| "--%".into());
    let reset = window
        .reset_at
        .as_deref()
        .and_then(format_tray_reset)
        .map(|value| format!(" · {value} 重置"))
        .unwrap_or_default();
    format!(
        "{} {}{}",
        window_display_name(&window.label),
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
                "{} {} ({})  --%{}",
                dot, platform, account.name, inactive
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
                        "{} {} ({}) · {}{}",
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
            "{} {} ({})  {}{}",
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
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn build_tray_menu<R: Runtime>(
    app: &AppHandle<R>,
    payload: &TrayMenuPayload,
) -> Result<Menu<R>, tauri::Error> {
    let pool = MenuItem::with_id(app, "pool", "账号池", true, None::<&str>)?;
    let mut items: Vec<Box<dyn tauri::menu::IsMenuItem<R>>> = vec![Box::new(pool)];

    if let Some(synced) = payload
        .synced_at
        .as_deref()
        .and_then(format_tray_synced_at)
    {
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
        "open-admin",
        "打开管理面板",
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

fn apply_tray_menu<R: Runtime>(
    app: &AppHandle<R>,
    payload: &TrayMenuPayload,
) -> Result<(), PetError> {
    let menu = build_tray_menu(app, payload).map_err(|error| PetError::Api(error.to_string()))?;
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu))
            .map_err(|error| PetError::Api(error.to_string()))?;
    }
    Ok(())
}

#[tauri::command]
fn update_tray_menu(app: tauri::AppHandle, payload: TrayMenuPayload) -> Result<(), PetError> {
    apply_tray_menu(&app, &payload)
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

            let menu = build_tray_menu(
                &app.handle(),
                &TrayMenuPayload {
                    accounts: Vec::new(),
                    synced_at: None,
                },
            )?;
            // Dedicated 64x64 tray art fills the menu-bar slot better than the window icon
            // (less empty padding, higher subject contrast at 18–22pt).
            let tray_icon = Image::from_bytes(include_bytes!("../icons/tray-icon-color.png"))
                .expect("tray icon");
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(tray_icon)
                .tooltip("Sub2API Pet")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    let id = event.id.as_ref();
                    match id {
                        "pool" | "show" => {
                            show_main_window(app);
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
                            show_main_window(app);
                        }
                        _ => {}
                    }
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
            refresh_pool_quotas,
            has_session,
            logout,
            quit_app,
            update_tray_menu
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
}
