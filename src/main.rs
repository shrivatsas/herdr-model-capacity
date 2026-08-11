use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use glob::glob;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration as StdDuration, SystemTime};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_REFRESH_SECONDS: i64 = 180;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct CapacityLimit {
    name: String,
    kind: String,
    unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remaining_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resets_at: Option<DateTime<Utc>>,
    status: LimitStatus,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum LimitStatus {
    Ok,
    Unknown,
    Unavailable,
    Stale,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CapacityAccount {
    provider: String,
    account_id: String,
    label: String,
    auth_type: String,
    limits: Vec<CapacityLimit>,
    fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    error: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountSpec {
    provider: String,
    account_id: String,
    label: String,
    #[serde(default = "unknown_auth")]
    auth_type: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    config_dir: Option<PathBuf>,
    #[serde(default)]
    allow_keychain: bool,
    #[serde(default)]
    codex_home: Option<PathBuf>,
    #[serde(default)]
    token_env: Option<String>,
    #[serde(default)]
    management_key_env: Option<String>,
    #[serde(default)]
    pi_auth_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentBinding {
    agent: String,
    provider: String,
    account_id: String,
    #[serde(default)]
    pane_id: String,
    #[allow(dead_code)]
    #[serde(default)]
    model: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    #[serde(default)]
    accounts: Option<Vec<AccountSpec>>,
    #[serde(default)]
    bindings: Vec<AgentBinding>,
    #[serde(default)]
    refresh_seconds: Option<i64>,
    #[serde(default)]
    warning_percent: Option<f64>,
    #[serde(default)]
    critical_percent: Option<f64>,
    #[serde(default)]
    warning_usd: Option<f64>,
    #[serde(default)]
    critical_usd: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
struct Pane {
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    display_agent: String,
}

fn unknown_auth() -> String {
    "unknown".into()
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_home(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let text = path.to_string_lossy();
    if text == "~" {
        home_dir()
    } else if let Some(rest) = text.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        path.to_path_buf()
    }
}

fn config_path() -> PathBuf {
    if let Some(path) = env::var_os("HERDR_CAPACITY_CONFIG") {
        return PathBuf::from(path);
    }
    env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .map(|directory| directory.join("model-capacity.json"))
        .unwrap_or_else(|| home_dir().join(".config/herdr/model-capacity.json"))
}

fn state_dir() -> PathBuf {
    env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/state/herdr-model-capacity"))
}

fn read_json(path: impl AsRef<Path>) -> Option<Value> {
    fs::read_to_string(expand_home(path))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn load_config() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    serde_json::from_str(
        &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))
}

fn stable_id(prefix: &str, path: &Path) -> String {
    let digest = Sha256::digest(expand_home(path).to_string_lossy().as_bytes());
    format!("{prefix}-{}", hex_lower(&digest[..5]))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn claude_credentials(config_dir: &Path, allow_keychain: bool) -> Option<Value> {
    let file = expand_home(config_dir).join(".credentials.json");
    if let Some(oauth) = read_json(file).and_then(|root| root.get("claudeAiOauth").cloned()) {
        if oauth.get("accessToken").and_then(Value::as_str).is_some() {
            return Some(oauth);
        }
    }
    if !allow_keychain || !cfg!(target_os = "macos") {
        return None;
    }
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<Value>(&output.stdout)
        .ok()?
        .get("claudeAiOauth")
        .cloned()
}

fn discover_accounts() -> Vec<AccountSpec> {
    let mut result = Vec::new();
    let claude_dir = env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"));
    if claude_credentials(&claude_dir, true).is_some() {
        result.push(AccountSpec {
            provider: "anthropic".into(),
            account_id: stable_id("claude", &claude_dir),
            label: "Claude subscription".into(),
            auth_type: "oauth".into(),
            source: "claude-code".into(),
            config_dir: Some(claude_dir),
            allow_keychain: true,
            codex_home: None,
            token_env: None,
            management_key_env: None,
            pi_auth_path: None,
        });
    }
    if env::var_os("ANTHROPIC_API_KEY").is_some() {
        result.push(api_account(
            "anthropic",
            "anthropic-api-env",
            "Anthropic API",
        ));
    }

    let codex_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"));
    let auth = read_json(codex_home.join("auth.json"));
    let has_sessions = !glob(&format!("{}/sessions/**/*.jsonl", codex_home.display()))
        .map(|mut paths| paths.next().is_none())
        .unwrap_or(true);
    if auth.is_some() || has_sessions {
        let oauth = auth.as_ref().is_some_and(|value| {
            matches!(
                value.get("auth_mode").and_then(Value::as_str),
                Some("chatgpt" | "chatgpt_auth_tokens")
            ) || value
                .pointer("/tokens/access_token")
                .and_then(Value::as_str)
                .is_some()
        });
        result.push(AccountSpec {
            provider: "openai".into(),
            account_id: stable_id("codex", &codex_home),
            label: if oauth {
                "ChatGPT / Codex"
            } else {
                "OpenAI API"
            }
            .into(),
            auth_type: if oauth { "oauth" } else { "api" }.into(),
            source: "codex".into(),
            config_dir: None,
            allow_keychain: false,
            codex_home: Some(codex_home),
            token_env: None,
            management_key_env: None,
            pi_auth_path: None,
        });
    } else if env::var_os("OPENAI_API_KEY").is_some() {
        result.push(api_account("openai", "openai-api-env", "OpenAI API"));
    }
    if env::var_os("OPENROUTER_API_KEY").is_some() {
        result.push(AccountSpec {
            provider: "openrouter".into(),
            account_id: "openrouter-env".into(),
            label: "OpenRouter".into(),
            auth_type: "api".into(),
            source: "openrouter".into(),
            config_dir: None,
            allow_keychain: false,
            codex_home: None,
            token_env: Some("OPENROUTER_API_KEY".into()),
            management_key_env: None,
            pi_auth_path: None,
        });
    }
    result
}

fn api_account(provider: &str, account_id: &str, label: &str) -> AccountSpec {
    AccountSpec {
        provider: provider.into(),
        account_id: account_id.into(),
        label: label.into(),
        auth_type: "api".into(),
        source: "api".into(),
        config_dir: None,
        allow_keychain: false,
        codex_home: None,
        token_env: None,
        management_key_env: None,
        pi_auth_path: None,
    }
}

fn configured_accounts(config: &Config) -> Result<Vec<AccountSpec>> {
    let accounts = config.accounts.clone().unwrap_or_else(discover_accounts);
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for mut account in accounts {
        account.provider = account.provider.to_lowercase();
        if !matches!(
            account.provider.as_str(),
            "anthropic" | "openai" | "openrouter"
        ) {
            continue;
        }
        if account.account_id.is_empty() || account.label.is_empty() {
            continue;
        }
        if !seen.insert((account.provider.clone(), account.account_id.clone())) {
            return Err(anyhow!(
                "duplicate capacity account: {}/{}",
                account.provider,
                account.account_id
            ));
        }
        result.push(account);
    }
    Ok(result)
}

fn client() -> Result<Client> {
    Client::builder()
        .timeout(StdDuration::from_secs(10))
        .build()
        .context("build HTTP client")
}

fn get_json(url: &str, mut headers: HeaderMap) -> Result<Value> {
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&format!("herdr-model-capacity/{VERSION}"))?,
    );
    let response = client()?
        .get(url)
        .headers(headers)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response
            .text()
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect::<String>();
        return Err(anyhow!("HTTP {}: {}", status.as_u16(), detail));
    }
    response.json().context("parse provider response")
}

fn clamp_percent(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

fn parse_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let value = value?;
    if let Some(number) = value.as_f64() {
        let seconds = if number > 10_000_000_000.0 {
            number / 1000.0
        } else {
            number
        };
        return DateTime::from_timestamp(seconds as i64, 0);
    }
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn quota_limit(
    name: impl Into<String>,
    used: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
) -> CapacityLimit {
    let remaining = used.map(|value| 100.0 - clamp_percent(value));
    CapacityLimit {
        name: name.into(),
        kind: "quota".into(),
        unit: "percent".into(),
        remaining: None,
        total: None,
        remaining_percent: remaining,
        resets_at,
        status: if remaining.is_some() {
            LimitStatus::Ok
        } else {
            LimitStatus::Unknown
        },
        detail: String::new(),
    }
}

fn unknown_balance(detail: &str) -> Vec<CapacityLimit> {
    vec![CapacityLimit {
        name: "API credits".into(),
        kind: "credits".into(),
        unit: "usd".into(),
        remaining: None,
        total: None,
        remaining_percent: None,
        resets_at: None,
        status: LimitStatus::Unknown,
        detail: detail.into(),
    }]
}

fn collect_anthropic(spec: &AccountSpec) -> Result<Vec<CapacityLimit>> {
    if spec.auth_type == "api" || spec.source == "api" {
        return Ok(unknown_balance(
            "Anthropic exposes no API credit-balance endpoint for ordinary API keys",
        ));
    }
    let dir = spec
        .config_dir
        .clone()
        .unwrap_or_else(|| home_dir().join(".claude"));
    let oauth = claude_credentials(&dir, spec.allow_keychain || spec.config_dir.is_none())
        .ok_or_else(|| anyhow!("no Claude OAuth credential in {}", dir.display()))?;
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Claude OAuth token is missing"))?;
    if parse_time(oauth.get("expiresAt")).is_some_and(|expires| expires <= Utc::now()) {
        return Err(anyhow!(
            "Claude OAuth access token expired; run Claude Code to refresh it"
        ));
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    headers.insert(
        HeaderName::from_static("anthropic-beta"),
        HeaderValue::from_static("oauth-2025-04-20"),
    );
    let data = get_json("https://api.anthropic.com/api/oauth/usage", headers)?;
    let mut limits = Vec::new();
    if let Some(items) = data.get("limits").and_then(Value::as_array) {
        for item in items {
            let Some(used) = item.get("percent").and_then(Value::as_f64) else {
                continue;
            };
            let model = item
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str);
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or("quota");
            let name = model
                .map(|name| format!("7d · {name}"))
                .unwrap_or_else(|| match kind {
                    "session" => "5h".into(),
                    "weekly_all" => "7d".into(),
                    other => other.into(),
                });
            let reset = parse_time(item.get("resets_at")).or_else(|| {
                item.get("resets_in_seconds")
                    .and_then(Value::as_i64)
                    .map(|seconds| Utc::now() + Duration::seconds(seconds))
            });
            limits.push(quota_limit(name, Some(used), reset));
        }
    } else {
        for (key, name) in [("five_hour", "5h"), ("seven_day", "7d")] {
            let window = data.get(key).unwrap_or(&Value::Null);
            limits.push(quota_limit(
                name,
                window.get("utilization").and_then(Value::as_f64),
                parse_time(window.get("resets_at")),
            ));
        }
    }
    Ok(limits)
}

fn find_rate_limits(value: &Value) -> Option<&Value> {
    match value {
        Value::Object(map) => {
            if map.get("rate_limits").is_some_and(Value::is_object) {
                return map.get("rate_limits");
            }
            if map.get("primary_window").is_some_and(Value::is_object) {
                return Some(value);
            }
            map.values().find_map(find_rate_limits)
        }
        Value::Array(items) => items.iter().find_map(find_rate_limits),
        _ => None,
    }
}

fn window_name(window: &Value, fallback: &str) -> String {
    let minutes = window
        .get("window_minutes")
        .and_then(Value::as_f64)
        .or_else(|| {
            window
                .get("limit_window_seconds")
                .and_then(Value::as_f64)
                .map(|seconds| seconds / 60.0)
        });
    match minutes {
        Some(value) if value >= 7.0 * 1440.0 - 60.0 => "7d".into(),
        Some(value) if value >= 60.0 => format!("{}h", (value / 60.0).round()),
        Some(value) => format!("{}m", value.round()),
        None => fallback.into(),
    }
}

fn codex_windows(limits: &Value, observed_at: Option<SystemTime>) -> Vec<CapacityLimit> {
    let mut result = HashMap::new();
    for (key, fallback) in [
        ("primary", "5h"),
        ("primary_window", "5h"),
        ("secondary", "7d"),
        ("secondary_window", "7d"),
    ] {
        let Some(window) = limits.get(key).filter(|value| value.is_object()) else {
            continue;
        };
        let mut reset = parse_time(window.get("resets_at").or_else(|| window.get("reset_at")));
        if reset.is_none() {
            let seconds = window
                .get("resets_in_seconds")
                .or_else(|| window.get("reset_after_seconds"))
                .and_then(Value::as_i64);
            if let Some(seconds) = seconds {
                let age = observed_at
                    .and_then(|time| time.elapsed().ok())
                    .map(|duration| duration.as_secs() as i64)
                    .unwrap_or(0);
                reset = Some(Utc::now() + Duration::seconds(seconds - age));
            }
        }
        let limit = quota_limit(
            window_name(window, fallback),
            window.get("used_percent").and_then(Value::as_f64),
            reset,
        );
        result.insert(limit.name.clone(), limit);
    }
    let mut result: Vec<_> = result.into_values().collect();
    result.sort_by_key(|limit| if limit.name == "5h" { 0 } else { 1 });
    result
}

fn latest_codex_snapshot(home: &Path) -> Option<(Value, SystemTime)> {
    let pattern = format!("{}/sessions/**/*.jsonl", expand_home(home).display());
    let mut files: Vec<_> = glob(&pattern).ok()?.flatten().collect();
    files.sort_by_key(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok());
    files.reverse();
    for path in files {
        let text = fs::read_to_string(&path).ok()?;
        let mut latest = None;
        for line in text.lines().filter(|line| line.contains("\"rate_limits\"")) {
            let parsed: Value = serde_json::from_str(line).ok()?;
            if let Some(found) = find_rate_limits(&parsed) {
                latest = Some(found.clone());
            }
        }
        if let Some(value) = latest {
            let modified = fs::metadata(path)
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::now());
            return Some((value, modified));
        }
    }
    None
}

fn collect_openai(spec: &AccountSpec) -> Result<Vec<CapacityLimit>> {
    if spec.auth_type == "api" || spec.source == "api" {
        return Ok(unknown_balance("OpenAI exposes organization costs, not a reliable prepaid balance for ordinary API keys"));
    }
    let home = spec
        .codex_home
        .clone()
        .unwrap_or_else(|| home_dir().join(".codex"));
    if let Some(auth) = read_json(home.join("auth.json")) {
        if let Some(token) = auth.pointer("/tokens/access_token").and_then(Value::as_str) {
            let mut headers = HeaderMap::new();
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))?,
            );
            if let Some(account_id) = auth.pointer("/tokens/account_id").and_then(Value::as_str) {
                headers.insert(
                    HeaderName::from_static("chatgpt-account-id"),
                    HeaderValue::from_str(account_id)?,
                );
            }
            if let Ok(data) = get_json("https://chatgpt.com/backend-api/wham/usage", headers) {
                if let Some(limits) = find_rate_limits(&data) {
                    let windows = codex_windows(limits, None);
                    if !windows.is_empty() {
                        return Ok(windows);
                    }
                }
            }
        }
    }
    if let Some((snapshot, observed)) = latest_codex_snapshot(&home) {
        return Ok(codex_windows(&snapshot, Some(observed)));
    }
    Err(anyhow!(
        "no Codex quota response or rate-limit snapshot in {}",
        home.display()
    ))
}

fn openrouter_token(spec: &AccountSpec) -> Option<String> {
    let env_name = spec
        .management_key_env
        .as_ref()
        .or(spec.token_env.as_ref())
        .map(String::as_str)
        .unwrap_or("OPENROUTER_API_KEY");
    if let Ok(token) = env::var(env_name) {
        if !token.trim().is_empty() {
            return Some(token.trim().into());
        }
    }
    let path = spec.pi_auth_path.as_ref()?;
    let auth = read_json(path)?;
    let entry = auth.get("openrouter")?;
    (entry.get("type").and_then(Value::as_str) == Some("api_key"))
        .then(|| entry.get("key").and_then(Value::as_str).map(str::to_owned))
        .flatten()
}

fn collect_openrouter(spec: &AccountSpec) -> Result<Vec<CapacityLimit>> {
    let token = openrouter_token(spec)
        .ok_or_else(|| anyhow!("OpenRouter key environment variable is not set"))?;
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    if spec.management_key_env.is_some() {
        let data = get_json("https://openrouter.ai/api/v1/credits", headers)?;
        let total = data
            .pointer("/data/total_credits")
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow!("credits response omitted total_credits"))?;
        let used = data
            .pointer("/data/total_usage")
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow!("credits response omitted total_usage"))?;
        return Ok(vec![money_limit(
            "balance",
            (total - used).max(0.0),
            Some(total),
        )]);
    }
    let data = get_json("https://openrouter.ai/api/v1/key", headers)?;
    let remaining = data
        .pointer("/data/limit_remaining")
        .and_then(Value::as_f64);
    let total = data.pointer("/data/limit").and_then(Value::as_f64);
    if let Some(remaining) = remaining {
        let mut limit = money_limit("key limit", remaining, total);
        limit.remaining_percent = total
            .filter(|total| *total > 0.0)
            .map(|total| remaining / total * 100.0);
        return Ok(vec![limit]);
    }
    Ok(vec![CapacityLimit {
        name: "key limit".into(),
        kind: "balance".into(),
        unit: "usd".into(),
        remaining: None,
        total,
        remaining_percent: None,
        resets_at: None,
        status: LimitStatus::Unknown,
        detail: "this key has no spending limit; use a management key for account credits".into(),
    }])
}

fn money_limit(name: &str, remaining: f64, total: Option<f64>) -> CapacityLimit {
    CapacityLimit {
        name: name.into(),
        kind: "balance".into(),
        unit: "usd".into(),
        remaining: Some(remaining),
        total,
        remaining_percent: None,
        resets_at: None,
        status: LimitStatus::Ok,
        detail: String::new(),
    }
}

fn collect_limits(spec: &AccountSpec) -> Result<Vec<CapacityLimit>> {
    match spec.provider.as_str() {
        "anthropic" => collect_anthropic(spec),
        "openai" => collect_openai(spec),
        "openrouter" => collect_openrouter(spec),
        provider => Err(anyhow!("unknown provider: {provider}")),
    }
}

fn cache_path(spec: &AccountSpec) -> PathBuf {
    let raw = format!("{}-{}", spec.provider, spec.account_id);
    let safe: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || "_.-".contains(ch) {
                ch
            } else {
                '-'
            }
        })
        .collect();
    state_dir().join(format!("{safe}.json"))
}

fn read_cached(spec: &AccountSpec) -> Option<CapacityAccount> {
    serde_json::from_value(read_json(cache_path(spec))?).ok()
}

fn collect_account(spec: &AccountSpec, refresh_seconds: i64, force: bool) -> CapacityAccount {
    let cached = read_cached(spec);
    if !force
        && cached.as_ref().is_some_and(|account| {
            (Utc::now() - account.fetched_at).num_seconds() < refresh_seconds
        })
    {
        return cached.unwrap();
    }
    match collect_limits(spec) {
        Ok(limits) => {
            let account = CapacityAccount {
                provider: spec.provider.clone(),
                account_id: spec.account_id.clone(),
                label: spec.label.clone(),
                auth_type: spec.auth_type.clone(),
                limits,
                fetched_at: Utc::now(),
                error: String::new(),
            };
            if let Some(parent) = cache_path(spec).parent() {
                let _ = fs::create_dir_all(parent);
            }
            let path = cache_path(spec);
            let temporary = path.with_extension("json.tmp");
            if fs::write(&temporary, serde_json::to_vec(&account).unwrap_or_default()).is_ok() {
                let _ = fs::rename(temporary, path);
            }
            account
        }
        Err(error) => {
            if let Some(mut cached) = cached {
                cached.error = format!("{error:#}");
                for limit in &mut cached.limits {
                    limit.status = LimitStatus::Stale;
                }
                cached
            } else {
                let detail = format!("{error:#}");
                CapacityAccount {
                    provider: spec.provider.clone(),
                    account_id: spec.account_id.clone(),
                    label: spec.label.clone(),
                    auth_type: spec.auth_type.clone(),
                    limits: vec![CapacityLimit {
                        name: "capacity".into(),
                        kind: "quota".into(),
                        unit: "percent".into(),
                        remaining: None,
                        total: None,
                        remaining_percent: None,
                        resets_at: None,
                        status: LimitStatus::Unavailable,
                        detail: detail.clone(),
                    }],
                    fetched_at: Utc::now(),
                    error: detail,
                }
            }
        }
    }
}

fn collect_all(config: &Config, force: bool) -> Result<Vec<CapacityAccount>> {
    let refresh = config
        .refresh_seconds
        .unwrap_or(DEFAULT_REFRESH_SECONDS)
        .max(60);
    Ok(configured_accounts(config)?
        .iter()
        .map(|spec| collect_account(spec, refresh, force))
        .collect())
}

fn provider_name(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "CLAUDE",
        "openai" => "CODEX / OPENAI",
        "openrouter" => "OPENROUTER",
        _ => "UNKNOWN",
    }
}

fn provider_color(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "\x1b[38;5;173m",
        "openai" => "\x1b[38;5;37m",
        "openrouter" => "\x1b[38;5;75m",
        _ => "",
    }
}

fn render_bar(percent: f64, width: usize, warning: f64, critical: f64) -> String {
    let percent = clamp_percent(percent);
    let filled = (percent / 100.0 * width as f64).round() as usize;
    let color = if percent < critical {
        "\x1b[31m"
    } else if percent < warning {
        "\x1b[33m"
    } else {
        "\x1b[32m"
    };
    format!(
        "{color}{}\x1b[2m{}\x1b[0m",
        "█".repeat(filled),
        "░".repeat(width - filled)
    )
}

fn format_reset(reset: Option<DateTime<Utc>>) -> String {
    let Some(seconds) = reset
        .map(|time| (time - Utc::now()).num_seconds())
        .filter(|value| *value > 0)
    else {
        return String::new();
    };
    let minutes = seconds / 60;
    let days = minutes / 1440;
    let hours = (minutes % 1440) / 60;
    let minutes = minutes % 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn limit_summary(limit: &CapacityLimit) -> String {
    match limit.status {
        LimitStatus::Unavailable => "unavailable ⚠".into(),
        LimitStatus::Unknown => "unknown".into(),
        _ if limit.unit == "usd" && limit.remaining.is_some() => {
            format!("${:.2}", limit.remaining.unwrap())
        }
        _ if limit.remaining_percent.is_some() => {
            format!("{}%", limit.remaining_percent.unwrap().round())
        }
        _ => "unknown".into(),
    }
}

fn render_limit(limit: &CapacityLimit, config: &Config, compact: bool) -> String {
    let stale = if limit.status == LimitStatus::Stale {
        " ~"
    } else {
        ""
    };
    if matches!(
        limit.status,
        LimitStatus::Unknown | LimitStatus::Unavailable
    ) {
        let detail = if compact || limit.detail.is_empty() {
            String::new()
        } else {
            format!(" · {}", limit.detail)
        };
        return format!("{:<12} {}{detail}", limit.name, limit_summary(limit));
    }
    if limit.unit == "usd" {
        let remaining = limit.remaining.unwrap_or(0.0);
        let color = if remaining < config.critical_usd.unwrap_or(5.0) {
            "\x1b[31m"
        } else if remaining < config.warning_usd.unwrap_or(10.0) {
            "\x1b[33m"
        } else {
            "\x1b[32m"
        };
        return format!(
            "{:<12} {color}${remaining:.2}\x1b[0m remaining{stale}",
            limit.name
        );
    }
    let Some(percent) = limit.remaining_percent else {
        return format!("{:<12} unknown", limit.name);
    };
    if compact {
        return format!("{:<6} {:>3}%{stale}", limit.name, percent.round());
    }
    let reset = format_reset(limit.resets_at);
    let reset = if reset.is_empty() {
        String::new()
    } else {
        format!("  ↻ {reset}")
    };
    format!(
        "{:<12} {} {:>3}%{stale}{reset}",
        limit.name,
        render_bar(
            percent,
            10,
            config.warning_percent.unwrap_or(20.0),
            config.critical_percent.unwrap_or(10.0)
        ),
        percent.round()
    )
}

fn herdr_panes() -> Vec<Pane> {
    let Ok(output) = Command::new("herdr").args(["pane", "list"]).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|value| serde_json::from_value(value.pointer("/result/panes")?.clone()).ok())
        .unwrap_or_default()
}

fn automatic_provider(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" | "claude-code" => Some("anthropic"),
        "codex" => Some("openai"),
        "pi" => {
            let settings = read_json(home_dir().join(".pi/agent/settings.json"))?;
            match settings.get("defaultProvider").and_then(Value::as_str)? {
                "anthropic" => Some("anthropic"),
                "openai" | "openai-codex" => Some("openai"),
                "openrouter" => Some("openrouter"),
                _ => None,
            }
        }
        // Amp routing is dynamic server-side state. Model branding is not
        // sufficient evidence of the user's billing account.
        _ => None,
    }
}

fn resolve_binding(
    pane: &Pane,
    config: &Config,
    accounts: &[CapacityAccount],
) -> Option<AgentBinding> {
    if let Some(binding) = config
        .bindings
        .iter()
        .find(|binding| !binding.pane_id.is_empty() && binding.pane_id == pane.pane_id)
    {
        return Some(binding.clone());
    }
    let by_agent: Vec<_> = config
        .bindings
        .iter()
        .filter(|binding| binding.pane_id.is_empty() && binding.agent == pane.agent)
        .collect();
    if by_agent.len() == 1 {
        return Some(by_agent[0].clone());
    }
    let provider = automatic_provider(&pane.agent)?;
    let candidates: Vec<_> = accounts
        .iter()
        .filter(|account| account.provider == provider)
        .collect();
    (candidates.len() == 1).then(|| AgentBinding {
        agent: pane.agent.clone(),
        provider: provider.into(),
        account_id: candidates[0].account_id.clone(),
        pane_id: pane.pane_id.clone(),
        model: String::new(),
    })
}

fn agent_name(agent: &str) -> &'static str {
    match agent {
        "claude" | "claude-code" => "Claude Code",
        "codex" => "Codex",
        "pi" => "Pi",
        "amp" | "ampcode" => "Ampcode",
        _ => "",
    }
}

fn render_agents(config: &Config, accounts: &[CapacityAccount]) -> Vec<String> {
    let panes = herdr_panes();
    let mut lines = Vec::new();
    for pane in panes
        .iter()
        .filter(|pane| !agent_name(&pane.agent).is_empty())
    {
        if lines.is_empty() {
            lines.extend([
                "\x1b[1mAgents\x1b[0m".into(),
                "\x1b[2m────────────────────────────────────────\x1b[0m".into(),
            ]);
        }
        let label = if !pane.label.is_empty() {
            &pane.label
        } else if !pane.display_agent.is_empty() {
            &pane.display_agent
        } else {
            agent_name(&pane.agent)
        };
        lines.push(format!("● {label}"));
        let Some(binding) = resolve_binding(pane, config, accounts) else {
            let reason = if matches!(pane.agent.as_str(), "amp" | "ampcode") {
                "dynamic route; configure a binding"
            } else {
                "account unresolved"
            };
            lines.push(format!(
                "  \x1b[2m{} · {reason}\x1b[0m",
                agent_name(&pane.agent)
            ));
            continue;
        };
        let Some(account) = accounts.iter().find(|account| {
            account.provider == binding.provider && account.account_id == binding.account_id
        }) else {
            lines.push("  \x1b[2mconfigured account is unavailable\x1b[0m".into());
            continue;
        };
        lines.push(format!(
            "  {} · {}",
            provider_name(&account.provider),
            account.label
        ));
        if let Some(limit) = account.limits.first() {
            lines.push(format!("  {} remaining", limit_summary(limit)));
        }
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render(config: &Config, accounts: &[CapacityAccount], compact: bool) -> String {
    let mut lines = vec![
        if compact {
            "\x1b[1mCapacity\x1b[0m".into()
        } else {
            "\x1b[1mModel Capacity\x1b[0m".into()
        },
        String::new(),
    ];
    if !compact {
        lines.extend(render_agents(config, accounts));
    }
    for provider in ["anthropic", "openai", "openrouter"] {
        let group: Vec<_> = accounts
            .iter()
            .filter(|account| account.provider == provider)
            .collect();
        if group.is_empty() {
            continue;
        }
        lines.push(format!(
            "{}\x1b[1m{}\x1b[0m",
            provider_color(provider),
            provider_name(provider)
        ));
        if !compact {
            lines.push(String::new());
        }
        for account in group {
            let stale = if account
                .limits
                .iter()
                .any(|limit| limit.status == LimitStatus::Stale)
            {
                " \x1b[33m(stale)\x1b[0m"
            } else {
                ""
            };
            lines.push(format!("{}{stale}", account.label));
            for limit in &account.limits {
                lines.push(format!("  {}", render_limit(limit, config, compact)));
            }
            if !compact && !account.error.is_empty() {
                lines.push(format!(
                    "  \x1b[2mlast refresh failed: {}\x1b[0m",
                    account.error
                ));
            }
            lines.push(String::new());
        }
    }
    if accounts.is_empty() {
        lines.extend([
            "No accounts discovered.".into(),
            format!("Configure {}", config_path().display()),
        ]);
    }
    lines.join("\n").trim_end().into()
}

fn read_key() -> Result<char> {
    let _ = Command::new("stty").args(["raw", "-echo"]).status();
    let mut byte = [0_u8; 1];
    let result = io::stdin().read_exact(&mut byte);
    let _ = Command::new("stty").arg("sane").status();
    result.context("read pane key")?;
    Ok(byte[0] as char)
}

fn pane_view(compact: bool) -> Result<()> {
    let config = load_config()?;
    let interactive = Command::new("test")
        .args(["-t", "0"])
        .stdin(Stdio::inherit())
        .status()
        .is_ok_and(|status| status.success());
    let mut force = false;
    loop {
        let accounts = collect_all(&config, force)?;
        let output = render(&config, &accounts, compact);
        if !interactive {
            println!("{output}");
            return Ok(());
        }
        println!("\x1b[2J\x1b[H{output}\n\n\x1b[2m[r] refresh · any other key closes\x1b[0m");
        if !matches!(read_key()?, 'r' | 'R') {
            return Ok(());
        }
        force = true;
    }
}

fn probe() -> Result<()> {
    let config = load_config()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&collect_all(&config, true)?)?
    );
    Ok(())
}

fn main() -> Result<()> {
    match env::args().nth(1).as_deref().unwrap_or("pane") {
        "pane" => pane_view(false),
        "compact" => pane_view(true),
        "probe" => probe(),
        "version" => {
            println!("{VERSION}");
            Ok(())
        }
        command => Err(anyhow!("unknown command: {command}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_used_to_remaining_and_keeps_zero_distinct() {
        assert_eq!(
            quota_limit("5h", Some(28.0), None).remaining_percent,
            Some(72.0)
        );
        assert_eq!(
            quota_limit("5h", Some(100.0), None).remaining_percent,
            Some(0.0)
        );
        assert_eq!(quota_limit("5h", None, None).status, LimitStatus::Unknown);
    }

    #[test]
    fn parses_codex_windows() {
        let limits = json!({
            "primary": {"window_minutes": 300, "used_percent": 37},
            "secondary": {"window_minutes": 10080, "used_percent": 18}
        });
        let windows = codex_windows(&limits, None);
        assert_eq!(
            windows
                .iter()
                .map(|limit| (&limit.name, limit.remaining_percent))
                .collect::<Vec<_>>(),
            vec![(&"5h".into(), Some(63.0)), (&"7d".into(), Some(82.0))]
        );
    }

    #[test]
    fn renders_multiple_accounts_as_separate_capacity_lines() {
        let accounts = vec![
            test_account("personal", "Personal", 28.0),
            test_account("work", "Work", 9.0),
        ];
        let output = render(&Config::default(), &accounts, true);
        assert!(output.contains("Personal") && output.contains("72%"));
        assert!(output.contains("Work") && output.contains("91%"));
    }

    #[test]
    fn explicit_binding_wins_and_amp_is_never_guessed() {
        let accounts = vec![
            test_account("personal", "Personal", 20.0),
            test_account("work", "Work", 10.0),
        ];
        let config = Config {
            bindings: vec![AgentBinding {
                agent: "pi".into(),
                provider: "anthropic".into(),
                account_id: "work".into(),
                pane_id: String::new(),
                model: String::new(),
            }],
            ..Config::default()
        };
        assert_eq!(
            resolve_binding(
                &Pane {
                    pane_id: "p".into(),
                    agent: "pi".into(),
                    label: String::new(),
                    display_agent: String::new()
                },
                &config,
                &accounts
            )
            .unwrap()
            .account_id,
            "work"
        );
        assert!(resolve_binding(
            &Pane {
                pane_id: "a".into(),
                agent: "amp".into(),
                label: String::new(),
                display_agent: String::new()
            },
            &Config::default(),
            &accounts
        )
        .is_none());
    }

    fn test_account(id: &str, label: &str, used: f64) -> CapacityAccount {
        CapacityAccount {
            provider: "anthropic".into(),
            account_id: id.into(),
            label: label.into(),
            auth_type: "oauth".into(),
            limits: vec![quota_limit("5h", Some(used), None)],
            fetched_at: Utc::now(),
            error: String::new(),
        }
    }
}
