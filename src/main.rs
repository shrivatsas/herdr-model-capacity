use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    mpsc::{self, Receiver},
    OnceLock,
};
use std::thread;
use std::time::{Duration as StdDuration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_REFRESH_SECONDS: i64 = 180;
const AMP_USAGE_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const AMP_USAGE_TEXT_VERSION: u8 = 1;

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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    collector_fingerprint: String,
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
    secret_ref: Option<SecretRef>,
    #[serde(default)]
    codex_home: Option<PathBuf>,
    #[serde(default)]
    token_env: Option<String>,
    #[serde(default)]
    management_key_env: Option<String>,
    #[serde(default)]
    pi_auth_path: Option<PathBuf>,
    #[serde(default)]
    amp_settings_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SecretRef {
    kind: String,
    service: String,
    account: String,
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
    accounts: Vec<AccountSpec>,
    #[serde(default)]
    bindings: Vec<AgentBinding>,
    #[serde(default)]
    show_bindings: bool,
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

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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

fn keychain_item_exists(reference: &SecretRef) -> Result<()> {
    if reference.kind != "macos-keychain" {
        return Err(anyhow!(
            "unsupported secret reference kind: {}",
            reference.kind
        ));
    }
    if !cfg!(target_os = "macos") {
        return Err(anyhow!("macOS Keychain secret references require macOS"));
    }
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            &reference.service,
            "-a",
            &reference.account,
        ])
        .output()
        .context("read macOS Keychain secret")?;
    if !output.status.success() {
        return Err(anyhow!(
            "no macOS Keychain item for the configured service and account"
        ));
    }
    Ok(())
}

fn unavailable_account(spec: &AccountSpec, detail: String) -> CapacityAccount {
    CapacityAccount {
        provider: if matches!(
            spec.provider.as_str(),
            "amp" | "anthropic" | "openai" | "openrouter"
        ) {
            spec.provider.clone()
        } else {
            "invalid".into()
        },
        account_id: spec.account_id.clone(),
        label: if spec.label.is_empty() {
            "Invalid account".into()
        } else {
            spec.label.clone()
        },
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
        collector_fingerprint: String::new(),
    }
}

fn configured_accounts(config: &Config) -> (Vec<AccountSpec>, Vec<CapacityAccount>) {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut errors = Vec::new();
    for mut account in config.accounts.clone() {
        account.provider = account.provider.to_lowercase();
        if !matches!(
            account.provider.as_str(),
            "amp" | "anthropic" | "openai" | "openrouter"
        ) {
            let detail = format!("unknown provider: {}", account.provider);
            errors.push(unavailable_account(&account, detail));
            continue;
        }
        if account.account_id.is_empty() || account.label.is_empty() {
            errors.push(unavailable_account(
                &account,
                "accountId and label must both be non-empty".into(),
            ));
            continue;
        }
        if !seen.insert((account.provider.clone(), account.account_id.clone())) {
            let detail = format!(
                "duplicate capacity account: {}/{}",
                account.provider, account.account_id
            );
            errors.push(unavailable_account(&account, detail));
            continue;
        }
        result.push(account);
    }
    (result, errors)
}

fn client() -> Result<&'static Client> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = Client::builder()
        .timeout(StdDuration::from_secs(10))
        .build()
        .context("build HTTP client")?;
    let _ = CLIENT.set(client);
    Ok(CLIENT.get().expect("HTTP client was initialized"))
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
    if let Some(reference) = &spec.secret_ref {
        // setup-token credentials can authenticate Claude Code inference, but the
        // official OAuth usage endpoint rejects them. Verify the reference exists
        // without exposing the token, then report that capability honestly.
        keychain_item_exists(reference)?;
        return Ok(vec![CapacityLimit {
            name: "subscription quota".into(),
            kind: "quota".into(),
            unit: "percent".into(),
            remaining: None,
            total: None,
            remaining_percent: None,
            resets_at: None,
            status: LimitStatus::Unknown,
            detail: "setup-token authentication works for inference, but Claude's quota endpoint does not authorize this credential type".into(),
        }]);
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

fn window_name(window: &Value, fallback: &str) -> String {
    let minutes = window
        .get("windowDurationMins")
        .or_else(|| window.get("window_minutes"))
        .and_then(Value::as_f64)
        .or_else(|| {
            window
                .get("limit_window_seconds")
                .and_then(Value::as_f64)
                .map(|seconds| seconds / 60.0)
        });
    match minutes {
        Some(value) if (7.0 * 1440.0 - 60.0..=7.0 * 1440.0 + 60.0).contains(&value) => "7d".into(),
        Some(value) if value >= 60.0 => format!("{}h", (value / 60.0).round()),
        Some(value) => format!("{}m", value.round()),
        None => fallback.into(),
    }
}

fn codex_windows(limits: &Value) -> Vec<CapacityLimit> {
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
        let reset = parse_time(
            window
                .get("resetsAt")
                .or_else(|| window.get("resets_at"))
                .or_else(|| window.get("reset_at")),
        );
        let limit = quota_limit(
            window_name(window, fallback),
            window
                .get("usedPercent")
                .or_else(|| window.get("used_percent"))
                .and_then(Value::as_f64),
            reset,
        );
        result.insert(limit.name.clone(), limit);
    }
    let mut result: Vec<_> = result.into_values().collect();
    result.sort_by(|left, right| {
        let rank = |limit: &CapacityLimit| match limit.name.as_str() {
            "5h" => 0,
            "7d" => 1,
            _ => 2,
        };
        rank(left)
            .cmp(&rank(right))
            .then_with(|| left.name.cmp(&right.name))
    });
    result
}

struct AppServer {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Result<Value, String>>,
}

impl AppServer {
    fn spawn(home: &Path) -> Result<Self> {
        let mut child = Command::new("codex")
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", expand_home(home))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("start codex app-server")?;
        let stdin = child.stdin.take().context("open codex app-server stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("open codex app-server stdout")?;
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let message = line.map_err(|error| error.to_string()).and_then(|line| {
                    serde_json::from_str(&line).map_err(|error| error.to_string())
                });
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            messages,
        })
    }

    fn send(&mut self, message: Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, &message).context("write app-server request")?;
        self.stdin
            .write_all(b"\n")
            .and_then(|()| self.stdin.flush())
            .context("flush app-server request")
    }

    fn response(&self, id: i64) -> Result<Value> {
        let deadline = Instant::now() + StdDuration::from_secs(10);
        loop {
            let message = self.receive_until(deadline)?;
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(anyhow!("codex app-server request failed: {error}"));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("codex app-server response omitted result"));
        }
    }

    fn receive_until(&self, deadline: Instant) -> Result<Value> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("codex app-server response timeout"));
        }
        self.messages
            .recv_timeout(remaining)
            .map_err(|error| anyhow!("codex app-server response timeout: {error}"))?
            .map_err(|error| anyhow!("invalid codex app-server output: {error}"))
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn collect_codex_app_server(home: &Path) -> Result<(Value, Value)> {
    let mut server = AppServer::spawn(home)?;
    server.send(serde_json::json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {"name": "herdr-model-capacity", "version": VERSION}
        }
    }))?;
    server.response(1).context("initialize codex app-server")?;
    server.send(serde_json::json!({"method": "initialized"}))?;
    server.send(serde_json::json!({
        "id": 2,
        "method": "account/read",
        "params": {"refreshToken": false}
    }))?;
    server.send(serde_json::json!({
        "id": 3,
        "method": "account/rateLimits/read"
    }))?;
    let deadline = Instant::now() + StdDuration::from_secs(10);
    collect_app_server_responses(|| server.receive_until(deadline))
}

fn collect_app_server_responses(mut next: impl FnMut() -> Result<Value>) -> Result<(Value, Value)> {
    let mut account = None;
    let mut limits = None;
    while account.is_none() || limits.is_none() {
        let message = next()?;
        match message.get("id").and_then(Value::as_i64) {
            Some(2) => account = Some(app_server_result(message)?),
            Some(3) => limits = Some(app_server_result(message)?),
            _ => {}
        }
    }
    Ok((
        account.expect("account response collected"),
        limits.expect("rate-limit response collected"),
    ))
}

fn app_server_result(message: Value) -> Result<Value> {
    if let Some(error) = message.get("error") {
        return Err(anyhow!("codex app-server request failed: {error}"));
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("codex app-server response omitted result"))
}

fn collect_openai(spec: &AccountSpec) -> Result<Vec<CapacityLimit>> {
    if spec.auth_type == "api" || spec.source == "api" {
        return Ok(unknown_balance("OpenAI exposes organization costs, not a reliable prepaid balance for ordinary API keys"));
    }
    let home = spec
        .codex_home
        .clone()
        .ok_or_else(|| anyhow!("codexHome is required for a ChatGPT account"))?;
    let (account, response) = collect_codex_app_server(&home)?;
    if account.get("account").is_none_or(Value::is_null) {
        return Err(anyhow!("Codex is not authenticated in {}", home.display()));
    }
    let limits = response
        .get("rateLimits")
        .ok_or_else(|| anyhow!("Codex rate-limit response omitted rateLimits"))?;
    let windows = codex_windows(limits);
    if windows.is_empty() {
        return Err(anyhow!("Codex returned no quota windows"));
    }
    Ok(windows)
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

fn parse_amp_decimal(value: &str) -> Option<f64> {
    let mut parts = value.split('.');
    let integer = parts.next()?;
    let fraction = parts.next();
    if parts.next().is_some()
        || fraction
            .is_some_and(|value| value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let groups: Vec<_> = integer.split(',').collect();
    let valid_integer = if groups.len() == 1 {
        !integer.is_empty() && integer.bytes().all(|c| c.is_ascii_digit())
    } else {
        (1..=3).contains(&groups[0].len())
            && groups[0].bytes().all(|c| c.is_ascii_digit())
            && groups[1..]
                .iter()
                .all(|group| group.len() == 3 && group.bytes().all(|c| c.is_ascii_digit()))
    };
    if !valid_integer {
        return None;
    }
    let value: f64 = value.replace(',', "").parse().ok()?;
    value.is_finite().then_some(value)
}

fn parse_amp_percent(value: &str) -> Option<f64> {
    parse_amp_decimal(value).filter(|value| *value <= 100.0)
}

fn amp_money_limit(name: &str, remaining: &str, total: Option<&str>) -> Option<CapacityLimit> {
    let total = match total {
        Some(value) => Some(parse_amp_decimal(value)?),
        None => None,
    };
    let remaining = parse_amp_decimal(remaining)?;
    let mut limit = money_limit(name, remaining, total);
    limit.remaining_percent = total
        .filter(|total| *total > 0.0)
        .map(|total| clamp_percent(remaining / total * 100.0));
    Some(limit)
}

fn amp_metadata_line(line: &str) -> bool {
    line.is_empty()
        || [
            "Signed in",
            "Logged in as ",
            "Account: ",
            "Learn more:",
            "Manage billing:",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        || line.starts_with("https://")
        || line.starts_with("http://")
}

fn valid_amp_advice_suffix(suffix: &str) -> bool {
    suffix.is_empty()
        || ((suffix.starts_with(" (") || suffix.starts_with(" - "))
            && (suffix.contains("https://") || suffix.contains("http://")))
}

fn valid_amp_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_control)
}

fn parse_amp_usage_at(output: &str, now: DateTime<Utc>) -> Result<Vec<CapacityLimit>> {
    let lower = output.to_ascii_lowercase();
    if [
        "not signed in",
        "not logged in",
        "run amp login",
        "sign in to amp",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return Err(anyhow!("Amp CLI is signed out"));
    }
    let mut limits = Vec::new();
    let mut contract_lines = 0;
    let mut parsed_lines = 0;
    for line in output.lines().map(str::trim) {
        if amp_metadata_line(line) {
            continue;
        }
        contract_lines += 1;
        if let Some(rest) = line.strip_prefix("Amp Free: $") {
            let Some((remaining, rest)) = rest.split_once("/$") else {
                continue;
            };
            let Some((total, detail)) = rest.split_once(" remaining (replenishes +$") else {
                continue;
            };
            let Some(rate) = detail.strip_suffix("/hour)") else {
                continue;
            };
            let Some(mut limit) = amp_money_limit("Amp Free", remaining, Some(total)) else {
                continue;
            };
            let Some(rate) = parse_amp_decimal(rate) else {
                continue;
            };
            limit.detail = format!("replenishes ${rate:.2}/hour");
            limits.push(limit);
            parsed_lines += 1;
            continue;
        }
        if let Some(percent) = line
            .strip_prefix("Amp Free: ")
            .and_then(|value| value.strip_suffix("% remaining today (resets daily)"))
            .and_then(parse_amp_percent)
        {
            limits.push(CapacityLimit {
                name: "Amp Free".into(),
                kind: "quota".into(),
                unit: "percent".into(),
                remaining: None,
                total: None,
                remaining_percent: Some(percent),
                resets_at: None,
                status: LimitStatus::Ok,
                detail: "resets daily".into(),
            });
            parsed_lines += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("Subscription ") {
            let Some((plan, rest)) = rest.split_once(": ") else {
                continue;
            };
            if !valid_amp_name(plan) {
                continue;
            }
            let Some((other, rest)) = rest.split_once("% other usage and ") else {
                continue;
            };
            let Some((orb, days)) = rest
                .split_once("% orb usage remaining - resets upon renewal in ")
                .and_then(|(orb, days)| Some((orb, days.strip_suffix(" days")?)))
            else {
                continue;
            };
            let (Some(other), Some(orb), Some(days)) = (
                parse_amp_percent(other),
                parse_amp_percent(orb),
                days.bytes()
                    .all(|c| c.is_ascii_digit())
                    .then(|| days.parse::<i64>().ok())
                    .flatten(),
            ) else {
                continue;
            };
            let Some(reset) =
                Duration::try_days(days).and_then(|days| now.checked_add_signed(days))
            else {
                continue;
            };
            for (lane, percent, detail) in [
                ("other", other, format!("renewal reported in {days} days")),
                ("orb", orb, String::new()),
            ] {
                limits.push(CapacityLimit {
                    name: format!("{plan} · {lane}"),
                    kind: "quota".into(),
                    unit: "percent".into(),
                    remaining: None,
                    total: None,
                    remaining_percent: Some(percent),
                    resets_at: Some(reset),
                    status: LimitStatus::Ok,
                    detail,
                });
            }
            parsed_lines += 1;
            continue;
        }
        if let Some((remaining, suffix)) = line
            .strip_prefix("Individual credits: $")
            .and_then(|value| value.split_once(" remaining"))
        {
            if valid_amp_advice_suffix(suffix) {
                if let Some(limit) = amp_money_limit("Individual credits", remaining, None) {
                    limits.push(limit);
                    parsed_lines += 1;
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("Workspace ") {
            let Some((name, value)) = rest.split_once(": $") else {
                continue;
            };
            let Some((remaining, suffix)) = value.split_once(" remaining") else {
                continue;
            };
            if valid_amp_name(name) && valid_amp_advice_suffix(suffix) {
                if let Some(limit) = amp_money_limit(&format!("Workspace {name}"), remaining, None)
                {
                    limits.push(limit);
                    parsed_lines += 1;
                }
            }
        }
    }
    if limits.is_empty() || parsed_lines != contract_lines {
        return Err(anyhow!(
            "Amp usage output did not match text contract v{AMP_USAGE_TEXT_VERSION}"
        ));
    }
    Ok(limits)
}

fn read_pipe(mut pipe: impl Read + Send + 'static) -> Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = pipe.read_to_end(&mut output).map(|_| output);
        let _ = sender.send(result);
    });
    receiver
}

fn stop_running_command(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn command_stdout_with_timeout(
    program: &Path,
    args: &[String],
    timeout: StdDuration,
) -> Result<Vec<u8>> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start {}", program.display()))?;
    let stdout = read_pipe(child.stdout.take().context("capture command stdout")?);
    let stderr = read_pipe(child.stderr.take().context("capture command stderr")?);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for command")? {
            break status;
        }
        if Instant::now() >= deadline {
            stop_running_command(&mut child);
            return Err(anyhow!(
                "command timed out after {}s",
                timeout.as_secs_f64()
            ));
        }
        thread::sleep(StdDuration::from_millis(10));
    };
    let receive = |reader: &Receiver<io::Result<Vec<u8>>>| {
        reader
            .recv_timeout(StdDuration::from_secs(1))
            .map_err(|_| anyhow!("command output collection timed out"))?
            .map_err(anyhow::Error::from)
    };
    let stdout = receive(&stdout)?;
    receive(&stderr)?;
    if !status.success() {
        return Err(anyhow!("command failed with status {status}"));
    }
    Ok(stdout)
}

fn collect_amp(spec: &AccountSpec) -> Result<Vec<CapacityLimit>> {
    let mut args = Vec::new();
    if let Some(path) = &spec.amp_settings_path {
        args.extend([
            "--settings-file".into(),
            expand_home(path).to_string_lossy().into_owned(),
        ]);
    }
    args.push("usage".into());
    let stdout = command_stdout_with_timeout(Path::new("amp"), &args, AMP_USAGE_TIMEOUT)
        .context("run official amp usage command")?;
    let output = String::from_utf8(stdout).context("amp usage output is not UTF-8")?;
    parse_amp_usage_at(&output, Utc::now())
}

fn collect_limits(spec: &AccountSpec) -> Result<Vec<CapacityLimit>> {
    match spec.provider.as_str() {
        "amp" => collect_amp(spec),
        "anthropic" => collect_anthropic(spec),
        "openai" => collect_openai(spec),
        "openrouter" => collect_openrouter(spec),
        provider => Err(anyhow!("unknown provider: {provider}")),
    }
}

fn cache_path(spec: &AccountSpec) -> PathBuf {
    let digest = Sha256::digest(format!("{}\0{}", spec.provider, spec.account_id).as_bytes());
    state_dir().join(format!("account-{}.json", hex_lower(&digest[..16])))
}

fn collector_fingerprint(spec: &AccountSpec) -> String {
    let secret = spec
        .secret_ref
        .as_ref()
        .map(|reference| {
            format!(
                "{}\0{}\0{}",
                reference.kind, reference.service, reference.account
            )
        })
        .unwrap_or_default();
    let material = format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
        spec.provider,
        spec.auth_type,
        spec.source,
        spec.config_dir
            .as_ref()
            .map(expand_home)
            .unwrap_or_default()
            .display(),
        spec.allow_keychain,
        spec.codex_home
            .as_ref()
            .map(expand_home)
            .unwrap_or_default()
            .display(),
        spec.token_env.as_deref().unwrap_or_default(),
        spec.management_key_env.as_deref().unwrap_or_default(),
        spec.pi_auth_path
            .as_ref()
            .map(expand_home)
            .unwrap_or_default()
            .display(),
        spec.amp_settings_path
            .as_ref()
            .map(expand_home)
            .unwrap_or_default()
            .display(),
        secret
    );
    hex_lower(&Sha256::digest(material.as_bytes()))
}

fn read_cached(spec: &AccountSpec) -> Option<CapacityAccount> {
    let account: CapacityAccount = serde_json::from_value(read_json(cache_path(spec))?).ok()?;
    (account.collector_fingerprint == collector_fingerprint(spec)).then_some(account)
}

fn collect_account(spec: &AccountSpec, refresh_seconds: i64, force: bool) -> CapacityAccount {
    let cached = read_cached(spec).map(|mut account| {
        account.provider.clone_from(&spec.provider);
        account.account_id.clone_from(&spec.account_id);
        account.label.clone_from(&spec.label);
        account.auth_type.clone_from(&spec.auth_type);
        account
    });
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
                collector_fingerprint: collector_fingerprint(spec),
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
                    collector_fingerprint: collector_fingerprint(spec),
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
    let (specs, mut accounts) = configured_accounts(config);
    accounts.extend(
        specs
            .iter()
            .map(|spec| collect_account(spec, refresh, force)),
    );
    Ok(accounts)
}

fn provider_name(provider: &str) -> &'static str {
    match provider {
        "amp" => "AMP",
        "anthropic" => "CLAUDE",
        "openai" => "CHATGPT / OPENAI",
        "openrouter" => "OPENROUTER",
        _ => "CONFIG",
    }
}

fn provider_color(provider: &str) -> &'static str {
    match provider {
        "amp" => "\x1b[38;5;141m",
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

fn truncate_text(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut used = 0;
    let mut result = String::new();
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width - 1 {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
}

fn pad_text(text: &str, width: usize) -> String {
    let text = truncate_text(text, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
    format!("{text}{}", " ".repeat(padding))
}

fn render_limit(limit: &CapacityLimit, config: &Config, compact: bool, width: usize) -> String {
    let stale = if limit.status == LimitStatus::Stale {
        " ~"
    } else {
        ""
    };
    if matches!(
        limit.status,
        LimitStatus::Unknown | LimitStatus::Unavailable
    ) {
        let summary = limit_summary(limit);
        let name_width = width.saturating_sub(summary.chars().count() + 1).max(1);
        return format!("{} {summary}", pad_text(&limit.name, name_width));
    }
    if limit.unit == "usd" {
        let remaining = limit.remaining.unwrap_or(0.0);
        if compact {
            let summary = format!("${remaining:.2}{stale}");
            let name_width = width.saturating_sub(summary.chars().count() + 1).max(1);
            return format!("{} {summary}", pad_text(&limit.name, name_width));
        }
        let color = if remaining < config.critical_usd.unwrap_or(5.0) {
            "\x1b[31m"
        } else if remaining < config.warning_usd.unwrap_or(10.0) {
            "\x1b[33m"
        } else {
            "\x1b[32m"
        };
        let summary = format!("${remaining:.2} remaining{stale}");
        let name_width = width.saturating_sub(summary.chars().count() + 1).max(1);
        return format!(
            "{} {color}${remaining:.2}\x1b[0m remaining{stale}",
            pad_text(&limit.name, name_width)
        );
    }
    let Some(percent) = limit.remaining_percent else {
        let name = truncate_text(&limit.name, width.saturating_sub(8).max(1));
        return format!("{name} unknown");
    };
    if compact {
        let summary = format!("{:>3}%{stale}", percent.round());
        let name = truncate_text(
            &limit.name,
            width.saturating_sub(summary.chars().count() + 1).max(1),
        );
        return format!("{name} {summary}");
    }
    let reset = format_reset(limit.resets_at);
    let reset = if reset.is_empty() {
        String::new()
    } else {
        format!("  ↻ {reset}")
    };
    let fixed_width = 12 + 1 + 5 + stale.len() + reset.chars().count();
    let bar_width = width.saturating_sub(fixed_width).max(4);
    format!(
        "{} {} {:>3}%{stale}{reset}",
        pad_text(&limit.name, 12),
        render_bar(
            percent,
            bar_width,
            config.warning_percent.unwrap_or(20.0),
            config.critical_percent.unwrap_or(10.0)
        ),
        percent.round()
    )
}

fn render_amp_limits(
    limits: &[CapacityLimit],
    config: &Config,
    compact: bool,
    width: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut index = 0;
    while index < limits.len() {
        let limit = &limits[index];
        let subscription = limit.name.strip_suffix(" · other").filter(|plan| {
            limits
                .get(index + 1)
                .is_some_and(|next| next.name == format!("{plan} · orb"))
        });
        if !compact {
            if let Some(plan) = subscription {
                let renewal = limit
                    .detail
                    .strip_prefix("renewal reported in ")
                    .map(|value| format!(" [renewal in {value}]"))
                    .unwrap_or_default();
                lines.push(format!(
                    "  \x1b[1m{}\x1b[0m",
                    truncate_text(&format!("{plan}{renewal}"), width.saturating_sub(2))
                ));
                for (lane, name) in [(&limits[index], "Other"), (&limits[index + 1], "Orbs")] {
                    let mut lane = lane.clone();
                    lane.name = name.into();
                    lane.resets_at = None;
                    lane.detail.clear();
                    lines.push(format!(
                        "    {}",
                        render_limit(&lane, config, false, width.saturating_sub(4))
                    ));
                }
                index += 2;
                continue;
            }
        }

        let mut rendered = limit.clone();
        if rendered.name == "Individual credits" {
            rendered.name = "Available Credits".into();
        }
        lines.push(format!(
            "  {}",
            render_limit(
                &rendered,
                config,
                compact || rendered.name == "Available Credits",
                width.saturating_sub(2)
            )
        ));
        if !compact && !rendered.detail.is_empty() {
            lines.push(format!(
                "  \x1b[2m{}\x1b[0m",
                truncate_text(&rendered.detail, width.saturating_sub(2))
            ));
        }
        index += 1;
    }
    lines
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

fn render_agents(config: &Config, accounts: &[CapacityAccount], width: usize) -> Vec<String> {
    let panes = herdr_panes();
    let mut lines = Vec::new();
    for pane in panes
        .iter()
        .filter(|pane| !agent_name(&pane.agent).is_empty())
    {
        if lines.is_empty() {
            lines.extend([
                "\x1b[1mAgents\x1b[0m".into(),
                format!(
                    "\x1b[2m{}\x1b[0m",
                    "─".repeat(width.saturating_sub(1).max(1))
                ),
            ]);
        }
        let label = if !pane.label.is_empty() {
            &pane.label
        } else if !pane.display_agent.is_empty() {
            &pane.display_agent
        } else {
            agent_name(&pane.agent)
        };
        lines.push(truncate_text(&format!("● {label}"), width));
        let Some(binding) = resolve_binding(pane, config, accounts) else {
            let reason = if matches!(pane.agent.as_str(), "amp" | "ampcode") {
                "dynamic route; configure a binding"
            } else {
                "account unresolved"
            };
            let detail = truncate_text(
                &format!("{} · {reason}", agent_name(&pane.agent)),
                width.saturating_sub(2),
            );
            lines.push(format!("  \x1b[2m{detail}\x1b[0m"));
            continue;
        };
        let Some(account) = accounts.iter().find(|account| {
            account.provider == binding.provider && account.account_id == binding.account_id
        }) else {
            let detail =
                truncate_text("configured account is unavailable", width.saturating_sub(2));
            lines.push(format!("  \x1b[2m{detail}\x1b[0m"));
            continue;
        };
        lines.push(truncate_text(
            &format!("  {} · {}", provider_name(&account.provider), account.label),
            width,
        ));
        if let Some(limit) = account.limits.first() {
            lines.push(truncate_text(
                &format!("  {} remaining", limit_summary(limit)),
                width,
            ));
        }
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render(config: &Config, accounts: &[CapacityAccount], compact: bool, width: usize) -> String {
    if width < 20 {
        let mut lines = vec![truncate_text("Capacity", width)];
        for account in accounts {
            let summary = account
                .limits
                .first()
                .map(limit_summary)
                .unwrap_or_else(|| "unknown".into());
            lines.push(truncate_text(
                &format!("{} {summary}", account.label),
                width,
            ));
        }
        if accounts.is_empty() {
            lines.push(truncate_text("No accounts", width));
        }
        return lines.join("\n");
    }
    let compact = compact || width < 36;
    let mut lines = vec![
        if compact {
            "\x1b[1mCapacity\x1b[0m".into()
        } else {
            "\x1b[1mModel Capacity\x1b[0m".into()
        },
        String::new(),
    ];
    for provider in ["invalid", "amp", "anthropic", "openai", "openrouter"] {
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
        lines.push(format!(
            "\x1b[2m{}\x1b[0m",
            "─".repeat(width.saturating_sub(1).max(8))
        ));
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
            let label_width = width.saturating_sub(if stale.is_empty() { 1 } else { 9 });
            lines.push(format!(
                "\x1b[1m{}{stale}\x1b[0m",
                truncate_text(&account.label, label_width)
            ));
            if account.provider == "amp" {
                lines.extend(render_amp_limits(&account.limits, config, compact, width));
            } else {
                for limit in &account.limits {
                    lines.push(format!(
                        "  {}",
                        render_limit(limit, config, compact, width.saturating_sub(2))
                    ));
                    if !compact && !limit.detail.is_empty() {
                        lines.push(format!(
                            "  \x1b[2m{}\x1b[0m",
                            truncate_text(&limit.detail, width.saturating_sub(2))
                        ));
                    }
                }
            }
            if !compact && !account.error.is_empty() {
                lines.push(format!(
                    "  \x1b[2mlast refresh failed: {}\x1b[0m",
                    truncate_text(&account.error, width.saturating_sub(24))
                ));
            }
            lines.push(String::new());
        }
    }
    if accounts.is_empty() {
        lines.extend([
            truncate_text("No accounts configured.", width),
            truncate_text(&format!("Configure {}", config_path().display()), width),
        ]);
    }
    if config.show_bindings && !compact {
        lines.push(String::new());
        lines.extend(render_agents(config, accounts, width));
    }
    lines.join("\n").trim_end().into()
}

fn terminal_width() -> usize {
    if let Some(width) = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|width: &usize| *width > 0)
    {
        return width;
    }
    if let Ok(tty) = fs::File::open("/dev/tty") {
        if let Ok(output) = Command::new("stty")
            .arg("size")
            .stdin(Stdio::from(tty))
            .output()
        {
            if output.status.success() {
                if let Some(width) = String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .nth(1)
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|width| *width > 0)
                {
                    return width;
                }
            }
        }
    }
    20
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
    let interactive = io::stdin().is_terminal();
    let mut force = false;
    loop {
        let accounts = collect_all(&config, force)?;
        let width = terminal_width();
        let output = render(&config, &accounts, compact, width);
        if !interactive {
            println!("{output}");
            return Ok(());
        }
        let prompt = truncate_text("[r] refresh · other closes", width);
        println!("\x1b[2J\x1b[H{output}\n\n\x1b[2m{prompt}\x1b[0m");
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
            "secondary": {"window_minutes": 10080, "used_percent": 18},
            "secondary_window": {"window_minutes": 43200, "used_percent": 10}
        });
        let windows = codex_windows(&limits);
        assert_eq!(
            windows
                .iter()
                .map(|limit| (&limit.name, limit.remaining_percent))
                .collect::<Vec<_>>(),
            vec![
                (&"5h".into(), Some(63.0)),
                (&"7d".into(), Some(82.0)),
                (&"720h".into(), Some(90.0))
            ]
        );
    }

    #[test]
    fn parses_amp_free_dollar_contract() {
        let limits = parse_amp_usage_at(
            include_str!("../tests/fixtures/amp/free_dollars.txt"),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].name, "Amp Free");
        assert_eq!(limits[0].remaining, Some(4.71));
        assert_eq!(limits[0].total, Some(10.0));
        assert!(limits[0]
            .remaining_percent
            .is_some_and(|percent| (percent - 47.1).abs() < 0.001));
        assert_eq!(limits[0].detail, "replenishes $0.42/hour");
    }

    #[test]
    fn parses_amp_free_daily_contract_without_inventing_reset_time() {
        let limits = parse_amp_usage_at(
            include_str!("../tests/fixtures/amp/free_daily.txt"),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(limits[0].remaining_percent, Some(61.0));
        assert_eq!(limits[0].resets_at, None);
        assert_eq!(limits[0].detail, "resets daily");
    }

    #[test]
    fn parses_amp_subscription_lanes_and_approximate_renewal() {
        let now = DateTime::parse_from_rfc3339("2026-08-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let limits =
            parse_amp_usage_at(include_str!("../tests/fixtures/amp/subscription.txt"), now)
                .unwrap();
        assert_eq!(
            limits
                .iter()
                .map(|limit| (limit.name.as_str(), limit.remaining_percent))
                .collect::<Vec<_>>(),
            vec![
                ("Megawatt · other", Some(97.0)),
                ("Megawatt · orb", Some(100.0))
            ]
        );
        assert!(limits
            .iter()
            .all(|limit| limit.resets_at == Some(now + Duration::days(29))));
        assert_eq!(
            limits
                .iter()
                .filter(|limit| limit.detail == "renewal reported in 29 days")
                .count(),
            1
        );
    }

    #[test]
    fn parses_amp_individual_and_multiple_workspace_balances() {
        let credits = parse_amp_usage_at(
            include_str!("../tests/fixtures/amp/credits.txt"),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(credits[0].remaining, Some(25.64));

        let workspaces = parse_amp_usage_at(
            include_str!("../tests/fixtures/amp/workspaces.txt"),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].name, "Workspace Example One");
        assert_eq!(workspaces[0].remaining, Some(1234.56));
        assert_eq!(workspaces[1].remaining, Some(78.90));
    }

    #[test]
    fn parses_combined_amp_contract_and_ignores_identity_and_advice() {
        let limits = parse_amp_usage_at(
            include_str!("../tests/fixtures/amp/combined.txt"),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(limits.len(), 5);
        assert!(limits.iter().all(|limit| !limit.name.contains('@')));
    }

    #[test]
    fn rejects_amp_signed_out_and_parse_drift_instead_of_returning_zero() {
        for output in [
            "You are not signed in. Run amp login.",
            "You are not signed in.\nIndividual credits: $5.00 remaining",
            "Amp Free now has plenty remaining",
            "Individual credits: $5.00 remaining\nWorkspace changed format",
            "Amp Free: $4.71/$oops remaining (replenishes +$0.42/hour)",
            "Amp Free: -1% remaining today (resets daily)",
            "Amp Free: 101% remaining today (resets daily)",
            "Individual credits: $NaN remaining",
            "Individual credits: $5.00 remaining unexpected text",
            "Individual credits: $5.00 remaining\nTeam Demo: $50.00 remaining",
            "Workspace Demo\u{1b}[2J: $5.00 remaining",
            "Subscription Demo\u{7}: 97% other usage and 100% orb usage remaining - resets upon renewal in 29 days",
            "Subscription Megawatt: 97% other usage and 100% orb usage remaining - resets upon renewal in 999999999999999999 days",
        ] {
            assert!(parse_amp_usage_at(output, Utc::now()).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn amp_process_contract_sets_no_color_and_null_stdin() {
        let output = command_stdout_with_timeout(
            Path::new("sh"),
            &[
                "-c".into(),
                "test \"$NO_COLOR\" = 1 && ! read value && printf ok".into(),
            ],
            StdDuration::from_secs(1),
        )
        .unwrap();
        assert_eq!(output, b"ok");
    }

    #[cfg(unix)]
    #[test]
    fn amp_process_contract_surfaces_missing_failure_and_timeout() {
        assert!(command_stdout_with_timeout(
            Path::new("definitely-not-a-real-amp-command"),
            &[],
            StdDuration::from_secs(1)
        )
        .is_err());
        assert!(command_stdout_with_timeout(
            Path::new("sh"),
            &["-c".into(), "exit 7".into()],
            StdDuration::from_secs(1)
        )
        .is_err());
        let started = Instant::now();
        assert!(command_stdout_with_timeout(
            Path::new("sh"),
            &["-c".into(), "sleep 2 & wait".into()],
            StdDuration::from_millis(30)
        )
        .is_err());
        assert!(started.elapsed() < StdDuration::from_millis(500));
    }

    #[test]
    fn config_mistakes_become_unavailable_accounts() {
        let config: Config = serde_json::from_value(json!({
            "accounts": [
                {"provider": "anthropic", "accountId": "ok", "label": "OK"},
                {"provider": "claude", "accountId": "typo", "label": "Typo"},
                {"provider": "openai", "accountId": "", "label": "Empty"},
                {"provider": "anthropic", "accountId": "ok", "label": "Duplicate"}
            ]
        }))
        .unwrap();
        let (specs, errors) = configured_accounts(&config);
        assert_eq!(specs.len(), 1);
        assert_eq!(errors.len(), 3);
        assert!(errors
            .iter()
            .all(|account| account.limits[0].status == LimitStatus::Unavailable));
    }

    #[test]
    fn renders_multiple_accounts_as_separate_capacity_lines() {
        let accounts = vec![
            test_account("personal", "Personal", 28.0),
            test_account("work", "Work", 9.0),
        ];
        let output = render(&Config::default(), &accounts, true, 40);
        assert!(output.contains("Personal") && output.contains("72%"));
        assert!(output.contains("Work") && output.contains("91%"));
    }

    #[test]
    fn ultra_compact_render_never_exceeds_pty_width() {
        let accounts = vec![test_account("personal", "個人 subscription", 28.0)];
        let output = render(&Config::default(), &accounts, false, 4);
        assert!(output.lines().all(|line| UnicodeWidthStr::width(line) <= 4));
    }

    #[test]
    fn renders_amp_cards_in_narrow_and_wide_layouts() {
        let now = DateTime::parse_from_rfc3339("2026-08-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut limits =
            parse_amp_usage_at(include_str!("../tests/fixtures/amp/subscription.txt"), now)
                .unwrap();
        limits.push(money_limit("Individual credits", 25.64, None));
        let account = CapacityAccount {
            provider: "amp".into(),
            account_id: "billing".into(),
            label: "AMP billing".into(),
            auth_type: "cli".into(),
            limits,
            fetched_at: Utc::now(),
            error: String::new(),
            collector_fingerprint: String::new(),
        };
        for width in [30, 80] {
            let output = render(
                &Config::default(),
                std::slice::from_ref(&account),
                false,
                width,
            );
            assert!(output.contains("AMP"));
            assert!(output.contains("AMP billing"));
            assert!(output.contains("$25.64"));
        }
        let output = strip_ansi(&render(
            &Config::default(),
            std::slice::from_ref(&account),
            false,
            80,
        ));
        assert!(output.contains("Megawatt [renewal in 29 days]"));
        assert!(output.contains("Other"));
        assert!(output.contains("Orbs"));
        assert!(output.contains("Available Credits"));
        assert!(!output.contains("Individual credits"));
        assert!(!output.contains("renewal reported"));
        assert!(!output.contains("$25.64 remaining"));
        assert_eq!(output.matches("renewal in").count(), 1);
    }

    #[test]
    fn long_ascii_and_cjk_amp_names_stay_within_the_pane() {
        let account = CapacityAccount {
            provider: "amp".into(),
            account_id: "billing".into(),
            label: "Very long Amp billing account label".into(),
            auth_type: "cli".into(),
            limits: vec![
                money_limit("Workspace A very long workspace capacity name", 24.0, None),
                CapacityLimit {
                    name: "非常に長いワークスペース名 · orb".into(),
                    kind: "quota".into(),
                    unit: "percent".into(),
                    remaining: None,
                    total: None,
                    remaining_percent: Some(94.0),
                    resets_at: None,
                    status: LimitStatus::Ok,
                    detail: String::new(),
                },
            ],
            fetched_at: Utc::now(),
            error: String::new(),
            collector_fingerprint: String::new(),
        };
        for width in [20, 30, 36, 80] {
            let output = render(
                &Config::default(),
                std::slice::from_ref(&account),
                false,
                width,
            );
            assert!(output
                .lines()
                .all(|line| UnicodeWidthStr::width(strip_ansi(line).as_str()) <= width));
        }
    }

    #[test]
    fn every_limit_branch_stays_within_the_pane_width() {
        let mut account = test_account("personal", "Personal", 28.0);
        account.limits = vec![
            CapacityLimit {
                name: "7d · A very long model display name".into(),
                status: LimitStatus::Stale,
                ..quota_limit("unused", Some(28.0), None)
            },
            CapacityLimit {
                name: "A very long API credit balance name".into(),
                kind: "credits".into(),
                unit: "usd".into(),
                remaining: Some(12.34),
                total: None,
                remaining_percent: None,
                resets_at: None,
                status: LimitStatus::Stale,
                detail: String::new(),
            },
            CapacityLimit {
                name: "A very long unknown capacity name".into(),
                kind: "quota".into(),
                unit: "percent".into(),
                remaining: None,
                total: None,
                remaining_percent: None,
                resets_at: None,
                status: LimitStatus::Unknown,
                detail: String::new(),
            },
        ];
        for width in 20..=60 {
            let output = render(
                &Config::default(),
                std::slice::from_ref(&account),
                false,
                width,
            );
            assert!(output
                .lines()
                .all(|line| UnicodeWidthStr::width(strip_ansi(line).as_str()) <= width));
        }
    }

    #[test]
    fn correlates_app_server_responses_and_ignores_notifications() {
        let mut messages = vec![
            json!({"method": "account/rateLimits/updated", "params": {}}),
            json!({"id": 3, "result": {"rateLimits": {"primary": {}}}}),
            json!({"id": 99, "result": {}}),
            json!({"id": 2, "result": {"account": {"type": "chatgpt"}}}),
        ]
        .into_iter();
        let (account, limits) =
            collect_app_server_responses(|| Ok(messages.next().expect("test message"))).unwrap();
        assert_eq!(account.pointer("/account/type"), Some(&json!("chatgpt")));
        assert!(limits.get("rateLimits").is_some());
    }

    #[test]
    fn surfaces_app_server_errors_without_token_fallback() {
        let mut messages = vec![
            json!({"id": 2, "error": {"code": -32000, "message": "not authenticated"}}),
            json!({"id": 3, "result": {"rateLimits": {}}}),
        ]
        .into_iter();
        let error = collect_app_server_responses(|| Ok(messages.next().expect("test message")))
            .unwrap_err();
        assert!(error.to_string().contains("not authenticated"));
    }

    #[test]
    fn cache_keys_do_not_collapse_sanitized_account_ids() {
        let first = test_spec("work/us", "~/.codex-a");
        let second = test_spec("work-us", "~/.codex-a");
        assert_ne!(cache_path(&first), cache_path(&second));
    }

    #[test]
    fn collector_fingerprint_changes_with_codex_home() {
        let first = test_spec("work", "~/.codex-a");
        let second = test_spec("work", "~/.codex-b");
        assert_ne!(
            collector_fingerprint(&first),
            collector_fingerprint(&second)
        );
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
            collector_fingerprint: String::new(),
        }
    }

    fn test_spec(id: &str, codex_home: &str) -> AccountSpec {
        AccountSpec {
            provider: "openai".into(),
            account_id: id.into(),
            label: "Test".into(),
            auth_type: "oauth".into(),
            source: "codex".into(),
            config_dir: None,
            allow_keychain: false,
            secret_ref: None,
            codex_home: Some(codex_home.into()),
            token_env: None,
            management_key_env: None,
            pi_auth_path: None,
            amp_settings_path: None,
        }
    }

    fn strip_ansi(text: &str) -> String {
        let mut result = String::new();
        let mut characters = text.chars();
        while let Some(character) = characters.next() {
            if character == '\u{1b}' {
                for character in characters.by_ref() {
                    if character.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                result.push(character);
            }
        }
        result
    }
}
