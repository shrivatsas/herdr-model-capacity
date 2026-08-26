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
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::AtomicUsize;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    mpsc::{self, Receiver},
    Arc, OnceLock,
};
use std::thread;
use std::time::{Duration as StdDuration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_REFRESH_SECONDS: i64 = 180;
// Keep cold-cache refreshes responsive without letting configured accounts create an
// unbounded number of HTTP clients or collector subprocesses at once.
const MAX_CONCURRENT_COLLECTORS: usize = 4;
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    credential_label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    capacity_scope: String,
    limits: Vec<CapacityLimit>,
    fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    error: String,
    /// A non-fatal parse-contract warning (e.g. unrecognized Amp usage lines).
    /// Never derived from raw provider output, so it is safe to persist to
    /// cache and probe JSON alongside successfully parsed limits.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    warning: String,
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
    token_secret_ref: Option<SecretRef>,
    #[serde(default)]
    management_secret_ref: Option<SecretRef>,
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

#[derive(Debug)]
struct CollectedCapacity {
    limits: Vec<CapacityLimit>,
    credential_label: String,
    capacity_scope: String,
    warning: String,
}

impl CollectedCapacity {
    fn limits(limits: Vec<CapacityLimit>) -> Self {
        Self {
            limits,
            credential_label: String::new(),
            capacity_scope: String::new(),
            warning: String::new(),
        }
    }
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

fn claude_config_dir(spec: &AccountSpec) -> PathBuf {
    if let Some(dir) = &spec.config_dir {
        return expand_home(dir);
    }
    // Honor `CLAUDE_CONFIG_DIR` so the per-account `.claude-accounts/<name>`
    // prefix workflow Claude Code uses keeps working without a per-account
    // `configDir` override. Explicit `configDir` wins; the env var is the
    // fallback before the historical `~/.claude` default.
    if let Some(dir) = env::var_os("CLAUDE_CONFIG_DIR") {
        let dir = PathBuf::from(dir);
        return if dir.is_absolute() {
            dir
        } else {
            expand_home(&dir)
        };
    }
    home_dir().join(".claude")
}

/// Resolve a Claude OAuth credential the same way Claude Code does.
///
/// On macOS, when `CLAUDE_CONFIG_DIR` points somewhere other than the default
/// `~/.claude`, Claude Code stores the OAuth payload in a *namespaced*
/// Keychain item: `Claude Code-credentials-<sha256(abs_config_dir)[:8]>`. The
/// default config dir keeps the un-suffixed `Claude Code-credentials` service.
fn claude_credentials(config_dir: &Path, allow_keychain: bool) -> Option<Value> {
    let resolved = expand_home(config_dir);
    let file = resolved.join(".credentials.json");
    if let Some(oauth) = read_json(file).and_then(|root| root.get("claudeAiOauth").cloned()) {
        if oauth.get("accessToken").and_then(Value::as_str).is_some() {
            return Some(oauth);
        }
    }
    if !allow_keychain || !cfg!(target_os = "macos") {
        return None;
    }
    let default_dir = home_dir().join(".claude");
    let service = if resolved == default_dir {
        "Claude Code-credentials".to_string()
    } else {
        let fingerprint = claude_config_dir_fingerprint(&resolved);
        format!("Claude Code-credentials-{fingerprint}")
    };
    let output = Command::new("security")
        .args(["find-generic-password", "-s", &service, "-w"])
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

/// Match Claude Code's Keychain service suffix: the first 8 hex chars of the
/// SHA-256 of the absolute config dir. The path is made absolute against the
/// working directory but symlinks are intentionally NOT resolved, mirroring
/// Node's `path.resolve` (which Claude Code uses to derive the namespace).
fn claude_config_dir_fingerprint(dir: &Path) -> String {
    let absolute = make_absolute(dir);
    let normalized = normalize_path(&absolute);
    let display = normalized.display().to_string();
    hex_lower(&Sha256::digest(display.as_bytes())).get(..8).unwrap_or("").to_string()
}

fn make_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(path)
    }
}

/// Collapse `.` and `..` components without touching symlinks, so two paths
/// that Claude Code would hash identically produce the same fingerprint.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = Vec::new();
    for component in path.components() {
        use std::path::Component as C;
        match component {
            C::CurDir => {}
            C::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str().to_owned()),
        }
    }
    let mut result = PathBuf::new();
    for part in out {
        result.push(part);
    }
    if result.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        result
    }
}

fn resolve_secret_ref(reference: &SecretRef) -> Result<String> {
    resolve_secret_ref_with(reference, cfg!(target_os = "macos"), |reference| {
        let output = Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                &reference.service,
                "-a",
                &reference.account,
                "-w",
            ])
            .output()
            .context("read macOS Keychain secret")?;
        if !output.status.success() {
            return Err(anyhow!(
                "no macOS Keychain item for the configured service and account"
            ));
        }
        String::from_utf8(output.stdout).context("macOS Keychain secret is not UTF-8")
    })
}

fn resolve_secret_ref_with(
    reference: &SecretRef,
    is_macos: bool,
    lookup: impl FnOnce(&SecretRef) -> Result<String>,
) -> Result<String> {
    if reference.kind != "macos-keychain" {
        return Err(anyhow!(
            "unsupported secret reference kind: {}",
            reference.kind
        ));
    }
    if !is_macos {
        return Err(anyhow!("macOS Keychain secret references require macOS"));
    }
    let secret = lookup(reference)?;
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(anyhow!("configured macOS Keychain item is empty"));
    }
    Ok(secret.into())
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
        credential_label: String::new(),
        capacity_scope: String::new(),
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
        warning: String::new(),
        collector_fingerprint: String::new(),
    }
}

fn configured_accounts(config: &Config) -> (Vec<AccountSpec>, Vec<CapacityAccount>) {
    let mut seen = HashSet::new();
    let mut seen_anthropic_secret_refs = HashSet::new();
    let mut amp_configured = false;
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
        if account.provider == "amp" {
            if account.amp_settings_path.is_some() {
                errors.push(unavailable_account(
                    &account,
                    "ampSettingsPath cannot select an Amp identity and is not supported".into(),
                ));
                continue;
            }
            if amp_configured {
                errors.push(unavailable_account(
                    &account,
                    "only one Amp account can be configured because Amp CLI exposes one authenticated identity".into(),
                ));
                continue;
            }
            amp_configured = true;
        }
        if account.provider == "anthropic" {
            if let Some(reference) = &account.secret_ref {
                let identity = (
                    reference.kind.clone(),
                    reference.service.clone(),
                    reference.account.clone(),
                );
                if !seen_anthropic_secret_refs.insert(identity) {
                    let detail = format!(
                        "duplicate Keychain reference: {}/{} is already registered under another account and cannot become a separate subscription",
                        reference.service, reference.account
                    );
                    errors.push(unavailable_account(&account, detail));
                    continue;
                }
            }
        }
        if account.provider == "openrouter" {
            if account.secret_ref.is_some() {
                errors.push(unavailable_account(
                    &account,
                    "secretRef is not valid for OpenRouter; use tokenSecretRef or managementSecretRef".into(),
                ));
                continue;
            }
            let configured_sources = [
                account.token_env.is_some(),
                account.management_key_env.is_some(),
                account.token_secret_ref.is_some(),
                account.management_secret_ref.is_some(),
            ]
            .into_iter()
            .filter(|configured| *configured)
            .count();
            if configured_sources > 1 {
                errors.push(unavailable_account(
                    &account,
                    "configure exactly one OpenRouter environment-variable or secret-reference credential source".into(),
                ));
                continue;
            }
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

fn get_json(url: &str, headers: HeaderMap) -> Result<Value> {
    let mut headers = headers;
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
        return collect_anthropic_secret_ref_with(reference, resolve_secret_ref, get_json);
    }
    let dir = claude_config_dir(spec);
    let oauth = claude_credentials(&dir, spec.allow_keychain)
        .ok_or_else(|| anyhow!("no Claude OAuth credential in {}", dir.display()))?;
    claude_oauth_usage_with(&oauth, get_json)
}

/// A named Keychain credential may hold either a copy of a Claude Code OAuth
/// payload (independently collectible, same as the standard credential) or a
/// setup-token-style secret that authenticates inference but is rejected by
/// the OAuth usage endpoint. The two are distinguished by shape, never by
/// guessing from the account/service selector.
fn collect_anthropic_secret_ref_with(
    reference: &SecretRef,
    secret_lookup: impl FnOnce(&SecretRef) -> Result<String>,
    request: impl FnMut(&str, HeaderMap) -> Result<Value>,
) -> Result<Vec<CapacityLimit>> {
    let secret = secret_lookup(reference)?;
    match parse_claude_oauth_secret(&secret) {
        Some(oauth) => claude_oauth_usage_with(&oauth, request),
        None => Ok(vec![unsupported_claude_credential_limit()]),
    }
}

fn parse_claude_oauth_secret(secret: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(secret).ok()?;
    let oauth = value.get("claudeAiOauth").cloned().unwrap_or(value);
    oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .is_some()
        .then_some(oauth)
}

fn unsupported_claude_credential_limit() -> CapacityLimit {
    CapacityLimit {
        name: "subscription quota".into(),
        kind: "quota".into(),
        unit: "percent".into(),
        remaining: None,
        total: None,
        remaining_percent: None,
        resets_at: None,
        status: LimitStatus::Unknown,
        detail: "named Claude credential is not an OAuth-shaped payload; setup-token and other credential types authenticate inference but are not authorized by Claude's quota endpoint".into(),
    }
}

fn claude_oauth_usage_with(
    oauth: &Value,
    mut request: impl FnMut(&str, HeaderMap) -> Result<Value>,
) -> Result<Vec<CapacityLimit>> {
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
    let data = request("https://api.anthropic.com/api/oauth/usage", headers)
        .map_err(|error| redact_claude_error(error, token))?;
    Ok(parse_claude_usage(&data))
}

/// Provider error bodies are not a trusted boundary: redact the bearer token
/// before an error can be persisted to the cache or rendered in the pane.
fn redact_claude_error(error: anyhow::Error, token: &str) -> anyhow::Error {
    anyhow!(format!("{error:#}").replace(token, "[redacted]"))
}

fn parse_claude_usage(data: &Value) -> Vec<CapacityLimit> {
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
    limits
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum OpenRouterScope {
    Key,
    Management,
}

fn openrouter_credential(spec: &AccountSpec) -> Result<(OpenRouterScope, String)> {
    openrouter_credential_with(spec, |name| env::var(name).ok(), resolve_secret_ref)
}

fn openrouter_credential_with(
    spec: &AccountSpec,
    env_value: impl Fn(&str) -> Option<String>,
    secret_value: impl Fn(&SecretRef) -> Result<String>,
) -> Result<(OpenRouterScope, String)> {
    if spec.secret_ref.is_some() {
        return Err(anyhow!(
            "secretRef is not valid for OpenRouter; use tokenSecretRef or managementSecretRef"
        ));
    }
    let configured_sources = [
        spec.token_env.is_some(),
        spec.management_key_env.is_some(),
        spec.token_secret_ref.is_some(),
        spec.management_secret_ref.is_some(),
    ]
    .into_iter()
    .filter(|configured| *configured)
    .count();
    if configured_sources > 1 {
        return Err(anyhow!(
            "configure exactly one OpenRouter environment-variable or secret-reference credential source"
        ));
    }
    let from_env = |name: &str, scope| {
        env_value(name)
            .filter(|value| !value.trim().is_empty())
            .map(|value| (scope, value.trim().into()))
            .ok_or_else(|| anyhow!("configured OpenRouter environment variable {name} is not set"))
    };
    if let Some(name) = &spec.token_env {
        return from_env(name, OpenRouterScope::Key);
    }
    if let Some(name) = &spec.management_key_env {
        return from_env(name, OpenRouterScope::Management);
    }
    if let Some(reference) = &spec.token_secret_ref {
        return Ok((OpenRouterScope::Key, secret_value(reference)?));
    }
    if let Some(reference) = &spec.management_secret_ref {
        return Ok((OpenRouterScope::Management, secret_value(reference)?));
    }
    if let Some(token) = env_value("OPENROUTER_API_KEY").filter(|value| !value.trim().is_empty()) {
        return Ok((OpenRouterScope::Key, token.trim().into()));
    }
    if let Some(path) = &spec.pi_auth_path {
        let auth = read_json(path).ok_or_else(|| anyhow!("read configured Pi auth file"))?;
        let entry = auth
            .get("openrouter")
            .ok_or_else(|| anyhow!("Pi auth file has no OpenRouter credential"))?;
        if entry.get("type").and_then(Value::as_str) == Some("api_key") {
            if let Some(token) = entry.get("key").and_then(Value::as_str) {
                if !token.trim().is_empty() {
                    return Ok((OpenRouterScope::Key, token.trim().into()));
                }
            }
        }
        return Err(anyhow!("Pi auth file has no OpenRouter API key"));
    }
    Err(anyhow!(
        "OpenRouter credential is unavailable; configure tokenEnv or tokenSecretRef"
    ))
}

fn openrouter_headers(token: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    Ok(headers)
}

fn parse_openrouter_key(data: &Value, expected: OpenRouterScope) -> Result<CollectedCapacity> {
    let key = data
        .get("data")
        .ok_or_else(|| anyhow!("key response omitted data"))?;
    let is_management = key
        .get("is_management_key")
        .and_then(Value::as_bool)
        .or_else(|| key.get("is_provisioning_key").and_then(Value::as_bool));
    if let Some(is_management) = is_management {
        let actual = if is_management {
            OpenRouterScope::Management
        } else {
            OpenRouterScope::Key
        };
        if actual != expected {
            return Err(anyhow!(match expected {
                OpenRouterScope::Key => {
                    "configured ordinary OpenRouter credential is a management key; use managementKeyEnv or managementSecretRef"
                }
                OpenRouterScope::Management => {
                    "configured OpenRouter management credential is an ordinary API key; use tokenEnv or tokenSecretRef"
                }
            }));
        }
    }
    let credential_label = key
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if expected == OpenRouterScope::Management {
        return Ok(CollectedCapacity {
            limits: Vec::new(),
            credential_label,
            capacity_scope: "account_credits".into(),
            warning: String::new(),
        });
    }

    let remaining = key.get("limit_remaining").and_then(Value::as_f64);
    let total = key.get("limit").and_then(Value::as_f64);
    let reset_policy = key.get("limit_reset").and_then(Value::as_str);
    let reset_detail = reset_policy
        .map(|policy| format!("resets {policy}"))
        .unwrap_or_default();
    let limit = if let Some(remaining) = remaining {
        let mut limit = money_limit("key spending limit", remaining, total);
        limit.remaining_percent = total
            .filter(|total| *total > 0.0)
            .map(|total| remaining / total * 100.0);
        limit.detail = reset_detail;
        limit
    } else {
        CapacityLimit {
            name: "key spending limit".into(),
            kind: "unlimited".into(),
            unit: "usd".into(),
            remaining: None,
            total,
            remaining_percent: None,
            resets_at: None,
            status: LimitStatus::Unknown,
            detail: [
                "key is valid and unlimited at key scope; account-wide credits require a management key",
                &reset_detail,
            ]
            .into_iter()
            .filter(|detail| !detail.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        }
    };
    Ok(CollectedCapacity {
        limits: vec![limit],
        credential_label,
        capacity_scope: "key_spending_limit".into(),
        warning: String::new(),
    })
}

fn parse_openrouter_credits(data: &Value, credential_label: String) -> Result<CollectedCapacity> {
    let total = data
        .pointer("/data/total_credits")
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("credits response omitted total_credits"))?;
    let used = data
        .pointer("/data/total_usage")
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("credits response omitted total_usage"))?;
    let mut limit = money_limit("account credits", (total - used).max(0.0), Some(total));
    limit.kind = "credits".into();
    Ok(CollectedCapacity {
        limits: vec![limit],
        credential_label,
        capacity_scope: "account_credits".into(),
        warning: String::new(),
    })
}

fn collect_openrouter(spec: &AccountSpec) -> Result<CollectedCapacity> {
    let (scope, token) = openrouter_credential(spec)?;
    collect_openrouter_with(scope, &token, get_json)
}

fn redact_openrouter_error(error: anyhow::Error, token: &str) -> anyhow::Error {
    anyhow!(format!("{error:#}").replace(token, "[redacted]"))
}

fn collect_openrouter_with(
    scope: OpenRouterScope,
    token: &str,
    mut request: impl FnMut(&str, HeaderMap) -> Result<Value>,
) -> Result<CollectedCapacity> {
    let key_data = request(
        "https://openrouter.ai/api/v1/key",
        openrouter_headers(token)?,
    )
    .map_err(|error| redact_openrouter_error(error, token))?;
    let key = parse_openrouter_key(&key_data, scope)?;
    if scope == OpenRouterScope::Key {
        return Ok(key);
    }
    let credits = request(
        "https://openrouter.ai/api/v1/credits",
        openrouter_headers(token)?,
    )
    .map_err(|error| redact_openrouter_error(error, token))?;
    parse_openrouter_credits(&credits, key.credential_label)
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

/// The exact contracted `amp usage --details` advisory line. Lines are
/// trimmed before this comparison (see `parse_amp_usage_at`'s per-line
/// `str::trim`), so this only needs to match the trimmed contract text
/// itself, not a prefix: an unrecognized `# Run ...` line must still count
/// as unrecognized output rather than being silently swallowed as metadata.
const AMP_USAGE_DETAILS_ADVICE: &str = "# Run `amp usage --details` for more detailed information.";

fn amp_metadata_line(line: &str) -> bool {
    line.is_empty()
        || line == AMP_USAGE_DETAILS_ADVICE
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

/// Conclusive signed-out/authentication markers only. "run amp login" alone is
/// deliberately excluded: Amp CLI also surfaces it as account-switching advice
/// while already signed in (see `signed_in_amp_usage_can_include_login_advice`),
/// so it cannot be treated as unconditional proof of a signed-out session.
fn amp_signed_out_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "not signed in",
        "not logged in",
        "sign in to amp",
        "authentication required",
        "authentication failed",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn valid_amp_advice_suffix(suffix: &str) -> bool {
    suffix.is_empty()
        || ((suffix.starts_with(" (") || suffix.starts_with(" - "))
            && (suffix.contains("https://") || suffix.contains("http://")))
}

fn valid_amp_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_control)
}

/// A generic, non-sensitive notice shown when Amp usage output contained text
/// the parser did not understand. Never derived from the raw output, so it is
/// safe to render and to persist in cache/probe JSON.
const AMP_PARTIAL_WARNING: &str =
    "Amp usage output included lines that were not understood; some balances may be missing.";

/// The exact, conclusive Amp signed-out/authentication error message. Shared
/// between `parse_amp_usage_at` (which raises it) and `collect_account`
/// (which recognizes it to suppress stale-cache fallback for this provider),
/// so the two stay in sync without brittle substring matching.
const AMP_SIGNED_OUT_ERROR: &str = "Amp CLI is signed out";

#[derive(Debug)]
struct AmpUsage {
    limits: Vec<CapacityLimit>,
    warning: Option<String>,
}

fn parse_amp_usage_at(output: &str, now: DateTime<Utc>) -> Result<AmpUsage> {
    let mut limits = Vec::new();
    let mut contract_lines = 0;
    let mut parsed_lines = 0;
    let mut signed_out = false;
    for line in output.lines().map(str::trim) {
        // Checked before metadata classification: a conclusive signed-out or
        // authentication-failure marker must win even on a line that also
        // matches a metadata prefix like "Logged in as ", so metadata
        // classification can never suppress it.
        if amp_signed_out_line(line) {
            signed_out = true;
            continue;
        }
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
        let subscription_line = line
            .strip_prefix("Subscription ")
            .and_then(|rest| rest.split_once(": "))
            .or_else(|| {
                line.strip_prefix("**")
                    .and_then(|rest| rest.split_once(" Subscription:** "))
            });
        if let Some((plan, rest)) = subscription_line {
            if !valid_amp_name(plan) {
                continue;
            }
            let Some((other, rest)) = rest.split_once("% other usage and ") else {
                continue;
            };
            let Some((orb, days)) =
                rest.split_once("% orb usage remaining - resets upon renewal in ")
            else {
                continue;
            };
            let days = days
                .strip_suffix(" days")
                .or_else(|| days.strip_suffix(" day"));
            let (Some(other), Some(orb), Some(days)) = (
                parse_amp_percent(other),
                parse_amp_percent(orb),
                days.and_then(|days| {
                    days.bytes()
                        .all(|c| c.is_ascii_digit())
                        .then(|| days.parse::<i64>().ok())
                        .flatten()
                }),
            ) else {
                continue;
            };
            let Some(reset) =
                Duration::try_days(days).and_then(|days| now.checked_add_signed(days))
            else {
                continue;
            };
            let day_unit = if days == 1 { "day" } else { "days" };
            for (lane, percent, detail) in [
                (
                    "other",
                    other,
                    format!("renewal reported in {days} {day_unit}"),
                ),
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
            .or_else(|| line.strip_prefix("**Individual credits:** $"))
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
    if signed_out {
        return Err(anyhow!(AMP_SIGNED_OUT_ERROR));
    }
    if limits.is_empty() {
        return Err(anyhow!(
            "Amp usage output did not match text contract v{AMP_USAGE_TEXT_VERSION}"
        ));
    }
    let warning = (parsed_lines != contract_lines).then(|| AMP_PARTIAL_WARNING.to_string());
    Ok(AmpUsage { limits, warning })
}

#[cfg(unix)]
fn read_pipe(
    mut pipe: impl Read + AsRawFd + Send + 'static,
    child_exited: Arc<AtomicBool>,
) -> io::Result<Receiver<io::Result<Vec<u8>>>> {
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0_u8; 8192];
        let result = loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break Ok(output),
                Ok(read) => output.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if child_exited.load(Ordering::Acquire) {
                        break Ok(output);
                    }
                    thread::sleep(StdDuration::from_millis(10));
                }
                Err(error) => break Err(error),
            }
        };
        let _ = sender.send(result);
    });
    Ok(receiver)
}

#[cfg(not(unix))]
fn read_pipe(
    mut pipe: impl Read + Send + 'static,
    _child_exited: Arc<()>,
) -> io::Result<Receiver<io::Result<Vec<u8>>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = pipe.read_to_end(&mut output).map(|_| output);
        let _ = sender.send(result);
    });
    Ok(receiver)
}

fn command_diagnostic(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    truncate_text(&text.split_whitespace().collect::<Vec<_>>().join(" "), 300)
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
    #[cfg(unix)]
    let child_exited = Arc::new(AtomicBool::new(false));
    #[cfg(not(unix))]
    let child_exited = Arc::new(());
    let stdout = read_pipe(
        child.stdout.take().context("capture command stdout")?,
        Arc::clone(&child_exited),
    )?;
    let stderr = read_pipe(
        child.stderr.take().context("capture command stderr")?,
        Arc::clone(&child_exited),
    )?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for command")? {
            break status;
        }
        if Instant::now() >= deadline {
            stop_running_command(&mut child);
            #[cfg(unix)]
            child_exited.store(true, Ordering::Release);
            return Err(anyhow!(
                "command timed out after {}s",
                timeout.as_secs_f64()
            ));
        }
        thread::sleep(StdDuration::from_millis(10));
    };
    #[cfg(unix)]
    child_exited.store(true, Ordering::Release);
    let receive = |reader: &Receiver<io::Result<Vec<u8>>>| {
        reader
            .recv_timeout(StdDuration::from_secs(1))
            .map_err(|_| anyhow!("command output collection timed out"))?
            .map_err(anyhow::Error::from)
    };
    let stdout = receive(&stdout)?;
    let stderr = receive(&stderr)?;
    if !status.success() {
        let diagnostic = command_diagnostic(&stderr);
        return if diagnostic.is_empty() {
            Err(anyhow!("command failed with status {status}"))
        } else {
            Err(anyhow!("command failed with status {status}: {diagnostic}"))
        };
    }
    Ok(stdout)
}

fn collect_amp(_spec: &AccountSpec) -> Result<CollectedCapacity> {
    let stdout =
        command_stdout_with_timeout(Path::new("amp"), &["usage".into()], AMP_USAGE_TIMEOUT)
            .context("run official amp usage command")?;
    let output = String::from_utf8(stdout).context("amp usage output is not UTF-8")?;
    let usage = parse_amp_usage_at(&output, Utc::now())?;
    Ok(CollectedCapacity {
        limits: usage.limits,
        credential_label: String::new(),
        capacity_scope: String::new(),
        warning: usage.warning.unwrap_or_default(),
    })
}

fn collect_limits(spec: &AccountSpec) -> Result<CollectedCapacity> {
    match spec.provider.as_str() {
        "amp" => collect_amp(spec),
        "anthropic" => collect_anthropic(spec).map(CollectedCapacity::limits),
        "openai" => collect_openai(spec).map(CollectedCapacity::limits),
        "openrouter" => collect_openrouter(spec),
        provider => Err(anyhow!("unknown provider: {provider}")),
    }
}

/// True only for the exact, conclusive Amp signed-out/authentication error on
/// an Amp account. Deliberately narrow (provider match plus an exact message
/// match against the single error `parse_amp_usage_at` raises for this case)
/// so transient Amp failures (timeout, command error, unrecognized text) and
/// every other provider keep the normal stale-cache fallback in
/// `collect_account`; only this conclusive case must never show stale limits,
/// per issue #8.
fn is_amp_signed_out_error(spec: &AccountSpec, error: &anyhow::Error) -> bool {
    spec.provider == "amp" && error.to_string() == AMP_SIGNED_OUT_ERROR
}

fn cache_path(spec: &AccountSpec) -> PathBuf {
    let digest = Sha256::digest(format!("{}\0{}", spec.provider, spec.account_id).as_bytes());
    state_dir().join(format!("account-{}.json", hex_lower(&digest[..16])))
}

fn collector_fingerprint(spec: &AccountSpec) -> String {
    let secret_fingerprint = |reference: Option<&SecretRef>| {
        reference
            .map(|reference| {
                format!(
                    "{}\0{}\0{}",
                    reference.kind, reference.service, reference.account
                )
            })
            .unwrap_or_default()
    };
    // For Claude Code accounts the credential location depends on the resolved
    // config dir (which may come from `CLAUDE_CONFIG_DIR`), not just
    // `spec.config_dir`. Fold that resolved dir into the fingerprint so the
    // cache entry invalidates when the account follows a different
    // `.claude-accounts/<name>` prefix.
    let config_dir_display = if spec.provider == "anthropic" && spec.secret_ref.is_none() {
        claude_config_dir(spec).display().to_string()
    } else {
        spec.config_dir
            .as_ref()
            .map(expand_home)
            .unwrap_or_default()
            .display()
            .to_string()
    };
    let material = format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
        spec.provider,
        spec.auth_type,
        spec.source,
        config_dir_display,
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
        secret_fingerprint(spec.secret_ref.as_ref()),
        secret_fingerprint(spec.token_secret_ref.as_ref()),
        secret_fingerprint(spec.management_secret_ref.as_ref())
    );
    hex_lower(&Sha256::digest(material.as_bytes()))
}

fn read_cached(spec: &AccountSpec) -> Option<CapacityAccount> {
    let account: CapacityAccount = serde_json::from_value(read_json(cache_path(spec))?).ok()?;
    (account.collector_fingerprint == collector_fingerprint(spec)).then_some(account)
}

const MAX_STALE_CACHE_AGE: Duration = Duration::hours(24);

fn stale_cache_is_usable(fetched_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let age = now.signed_duration_since(fetched_at);
    age >= Duration::zero() && age <= MAX_STALE_CACHE_AGE
}

fn stale_age_label(fetched_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(fetched_at).num_seconds().max(0);
    if seconds < 60 {
        "now".into()
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / (60 * 60))
    }
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
        Ok(collected) => {
            let account = CapacityAccount {
                provider: spec.provider.clone(),
                account_id: spec.account_id.clone(),
                label: spec.label.clone(),
                auth_type: spec.auth_type.clone(),
                credential_label: collected.credential_label,
                capacity_scope: collected.capacity_scope,
                limits: collected.limits,
                fetched_at: Utc::now(),
                error: String::new(),
                warning: collected.warning,
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
        Err(error) => account_for_collection_error(spec, cached, error),
    }
}

/// Turns a `collect_limits` failure into the account to report: either the
/// previous cached value marked stale, or a fresh unavailable placeholder.
///
/// A conclusive Amp signed-out/authentication result must never fall back to
/// a previously cached successful value: per issue #8, that case is
/// unavailable, not stale. Every other error (transient Amp failures, and
/// all other providers) keeps the normal stale-cache fallback.
fn account_for_collection_error(
    spec: &AccountSpec,
    cached: Option<CapacityAccount>,
    error: anyhow::Error,
) -> CapacityAccount {
    let usable_cache = if is_amp_signed_out_error(spec, &error) {
        None
    } else {
        cached.filter(|account| stale_cache_is_usable(account.fetched_at, Utc::now()))
    };
    if let Some(mut cached) = usable_cache {
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
            credential_label: String::new(),
            capacity_scope: String::new(),
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
            warning: String::new(),
            collector_fingerprint: collector_fingerprint(spec),
        }
    }
}

fn collect_with_bounded_parallelism<T, R>(
    items: &[T],
    maximum_workers: usize,
    collect: impl Fn(&T) -> R + Send + Sync,
) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    if items.is_empty() {
        return Vec::new();
    }

    let workers = maximum_workers.max(1).min(items.len());
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let next = &next;
            let collect = &collect;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if index >= items.len() {
                    break;
                }
                // The receiver is drained below before the scoped workers are joined, so
                // a slow account cannot prevent a completed account from being recorded.
                sender
                    .send((index, collect(&items[index])))
                    .expect("collection receiver remains available");
            });
        }
        drop(sender);

        let mut results = (0..items.len()).map(|_| None).collect::<Vec<Option<R>>>();
        for (index, result) in receiver {
            results[index] = Some(result);
        }
        results
            .into_iter()
            .map(|result| result.expect("every collection worker returns one result"))
            .collect()
    })
}

fn collect_all(config: &Config, force: bool) -> Result<Vec<CapacityAccount>> {
    let refresh = config
        .refresh_seconds
        .unwrap_or(DEFAULT_REFRESH_SECONDS)
        .max(60);
    let (specs, mut accounts) = configured_accounts(config);
    accounts.extend(collect_with_bounded_parallelism(
        &specs,
        MAX_CONCURRENT_COLLECTORS,
        |spec| collect_account(spec, refresh, force),
    ));
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
    if limit.kind == "unlimited" {
        return if limit.status == LimitStatus::Stale {
            "unlimited ~".into()
        } else {
            "unlimited".into()
        };
    }
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
    if limit.kind == "unlimited" {
        let summary = limit_summary(limit);
        let name_width = width.saturating_sub(summary.chars().count() + 1).max(1);
        return format!("{} {summary}", pad_text(&limit.name, name_width));
    }
    if matches!(
        limit.status,
        LimitStatus::Unknown | LimitStatus::Unavailable
    ) {
        let summary = limit_summary(limit);
        let name_width = width.saturating_sub(summary.chars().count() + 1).max(1);
        return format!("{} {summary}", pad_text(&limit.name, name_width));
    }
    if limit.unit == "usd" && limit.remaining.is_some() {
        return render_money_limit(limit, config, compact, true, width);
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

fn render_money_limit(
    limit: &CapacityLimit,
    config: &Config,
    compact: bool,
    include_remaining: bool,
    width: usize,
) -> String {
    let remaining = limit.remaining.unwrap_or(0.0);
    let stale = if limit.status == LimitStatus::Stale {
        " ~"
    } else {
        ""
    };
    let total = limit.total.map(|total| format!("/${total:.2}"));
    let total = total.as_deref().unwrap_or_default();
    let suffix = if include_remaining && !compact {
        " remaining"
    } else {
        ""
    };
    let summary = format!("${remaining:.2}{total}{suffix}{stale}");
    if UnicodeWidthStr::width(summary.as_str()) + 2 > width {
        return truncate_text(&summary, width);
    }
    let name_width = width.saturating_sub(summary.chars().count() + 1).max(1);
    if compact {
        return format!("{} {summary}", pad_text(&limit.name, name_width));
    }
    let color_value = limit.remaining_percent.unwrap_or(remaining);
    let (warning, critical) = if limit.total.is_some() {
        (
            config.warning_percent.unwrap_or(20.0),
            config.critical_percent.unwrap_or(10.0),
        )
    } else {
        (
            config.warning_usd.unwrap_or(10.0),
            config.critical_usd.unwrap_or(5.0),
        )
    };
    let color = if color_value < critical {
        "\x1b[31m"
    } else if color_value < warning {
        "\x1b[33m"
    } else {
        "\x1b[32m"
    };
    format!(
        "{} {color}${remaining:.2}\x1b[0m{total}{suffix}{stale}",
        pad_text(&limit.name, name_width)
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
        if let Some(plan) = subscription {
            let reset = format_reset(limit.resets_at);
            let heading = if reset.is_empty() {
                plan.to_string()
            } else if compact {
                format!("{plan} · {reset}")
            } else {
                format!("{plan} [renewal in {reset}]")
            };
            lines.push(format!(
                "  \x1b[1m{}\x1b[0m",
                truncate_text(&heading, width.saturating_sub(2))
            ));
            let indent = if compact { "  " } else { "    " };
            for (lane, name) in [(&limits[index], "Other"), (&limits[index + 1], "Orbs")] {
                let mut lane = lane.clone();
                lane.name = name.into();
                lane.resets_at = None;
                lane.detail.clear();
                lines.push(format!(
                    "{indent}{}",
                    render_limit(&lane, config, compact, width.saturating_sub(indent.len()))
                ));
            }
            index += 2;
            continue;
        }

        let mut rendered = limit.clone();
        if rendered.name == "Individual credits" {
            rendered.name = "Available Credits".into();
        }
        let rendered_line = if rendered.name == "Available Credits" {
            render_money_limit(&rendered, config, compact, false, width.saturating_sub(2))
        } else {
            render_limit(&rendered, config, compact, width.saturating_sub(2))
        };
        lines.push(format!("  {rendered_line}"));
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
            let stale_age = if account
                .limits
                .iter()
                .any(|limit| limit.status == LimitStatus::Stale)
            {
                Some(stale_age_label(account.fetched_at, Utc::now()))
            } else {
                None
            };
            let stale = stale_age
                .as_ref()
                .map_or_else(String::new, |age| format!(" \x1b[33m(stale {age})\x1b[0m"));
            let stale_width = stale_age.map_or(0, |age| age.len() + 9);
            let label_width = width.saturating_sub(stale_width);
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
            if !compact && !account.warning.is_empty() {
                lines.push(format!(
                    "  \x1b[33m{}\x1b[0m",
                    truncate_text(&account.warning, width.saturating_sub(2))
                ));
            }
            let error_is_already_shown = account
                .limits
                .iter()
                .any(|limit| limit.detail == account.error);
            if !compact && !account.error.is_empty() && !error_is_already_shown {
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
    if let Some(width) = env::var("MODEL_CAPACITY_WIDTH")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|width: &usize| *width > 0)
    {
        return width;
    }
    #[cfg(unix)]
    {
        let width = |fd| {
            let mut size = libc::winsize {
                ws_row: 0,
                ws_col: 0,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            (unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0)
                .then(|| usize::from(size.ws_col))
        };
        if let Some(width) = width(io::stdout().as_raw_fd()) {
            return width;
        }
        if let Ok(tty) = fs::File::open("/dev/tty") {
            if let Some(width) = width(tty.as_raw_fd()) {
                return width;
            }
        }
    }
    env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|width: &usize| *width > 0)
        .unwrap_or(20)
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
        .unwrap()
        .limits;
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
        .unwrap()
        .limits;
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
                .unwrap()
                .limits;
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
    fn parses_singular_amp_subscription_renewal_day() {
        let now = DateTime::parse_from_rfc3339("2026-08-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let limits = parse_amp_usage_at(
            "Subscription Megawatt: 97% other usage and 100% orb usage remaining - resets upon renewal in 1 day",
            now,
        )
        .unwrap()
        .limits;
        assert_eq!(limits.len(), 2);
        assert!(limits
            .iter()
            .all(|limit| limit.resets_at == Some(now + Duration::days(1))));
        assert_eq!(limits[0].detail, "renewal reported in 1 day");
    }

    #[test]
    fn parses_amp_individual_and_multiple_workspace_balances() {
        let credits = parse_amp_usage_at(
            include_str!("../tests/fixtures/amp/credits.txt"),
            Utc::now(),
        )
        .unwrap()
        .limits;
        assert_eq!(credits[0].remaining, Some(25.64));

        let workspaces = parse_amp_usage_at(
            include_str!("../tests/fixtures/amp/workspaces.txt"),
            Utc::now(),
        )
        .unwrap()
        .limits;
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].name, "Workspace Example One");
        assert_eq!(workspaces[0].remaining, Some(1234.56));
        assert_eq!(workspaces[1].remaining, Some(78.90));
    }

    #[test]
    fn parses_combined_amp_contract_and_ignores_identity_and_advice() {
        let usage = parse_amp_usage_at(
            include_str!("../tests/fixtures/amp/combined.txt"),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(usage.limits.len(), 5);
        assert!(usage.limits.iter().all(|limit| !limit.name.contains('@')));
        assert!(usage.warning.is_none());
    }

    #[test]
    fn parses_current_amp_cli_bold_labels_and_details_advice() {
        let now = DateTime::parse_from_rfc3339("2026-08-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let usage = parse_amp_usage_at(
            include_str!("../tests/fixtures/amp/current_bold_labels.txt"),
            now,
        )
        .unwrap();
        assert!(usage.warning.is_none());
        assert_eq!(
            usage
                .limits
                .iter()
                .map(|limit| (
                    limit.name.as_str(),
                    limit.remaining_percent,
                    limit.remaining
                ))
                .collect::<Vec<_>>(),
            vec![
                ("Amp Megawatt · other", Some(0.0), None),
                ("Amp Megawatt · orb", Some(100.0), None),
                ("Individual credits", None, Some(2.95)),
            ]
        );
        assert!(usage
            .limits
            .iter()
            .take(2)
            .all(|limit| limit.resets_at == Some(now + Duration::days(18))));
    }

    #[test]
    fn rejects_malformed_bold_amp_subscription_and_credits_labels() {
        for output in [
            "**Amp Megawatt Subscription** 0% other usage and 100% orb usage remaining - resets upon renewal in 18 days",
            "**Individual credits** $2.95 remaining",
        ] {
            assert!(parse_amp_usage_at(output, Utc::now()).is_err());
        }
    }

    #[test]
    fn rejects_amp_signed_out_and_parse_drift_instead_of_returning_zero() {
        for output in [
            "You are not signed in. Run amp login.",
            "Amp Free now has plenty remaining",
            "Amp Free: $4.71/$oops remaining (replenishes +$0.42/hour)",
            "Amp Free: -1% remaining today (resets daily)",
            "Amp Free: 101% remaining today (resets daily)",
            "Individual credits: $NaN remaining",
            "Individual credits: $5.00 remaining unexpected text",
            "Workspace Demo\u{1b}[2J: $5.00 remaining",
            "Subscription Demo\u{7}: 97% other usage and 100% orb usage remaining - resets upon renewal in 29 days",
            "Subscription Megawatt: 97% other usage and 100% orb usage remaining - resets upon renewal in 999999999999999999 days",
        ] {
            assert!(parse_amp_usage_at(output, Utc::now()).is_err());
        }
    }

    #[test]
    fn amp_signed_out_output_is_unavailable_not_partial() {
        let error =
            parse_amp_usage_at("You are not signed in. Run amp login.", Utc::now()).unwrap_err();
        assert!(error.to_string().contains("signed out"));
    }

    #[test]
    fn amp_signed_out_marker_wins_even_with_a_parsed_looking_limit_line() {
        let error = parse_amp_usage_at(
            "Individual credits: $5.00 remaining\nYou are not signed in. Run amp login.",
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("signed out"));
    }

    #[test]
    fn amp_authentication_failure_marker_wins_even_with_a_parsed_looking_limit_line() {
        let error = parse_amp_usage_at(
            "Individual credits: $5.00 remaining\nAuthentication required. Run amp login.",
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("signed out"));
    }

    #[test]
    fn amp_metadata_shaped_authentication_failure_still_wins_over_a_parsed_looking_limit_line() {
        let error = parse_amp_usage_at(
            "Logged in as stale@example.invalid — authentication failed\nIndividual credits: $5.00 remaining",
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("signed out"));
    }

    #[test]
    fn amp_usage_keeps_parsed_limits_visible_when_a_line_is_an_unrecognized_run_advice() {
        let usage = parse_amp_usage_at(
            "Individual credits: $5.00 remaining\n# Run something-new for more info.",
            Utc::now(),
        )
        .unwrap();
        assert_eq!(usage.limits.len(), 1);
        assert_eq!(usage.limits[0].remaining, Some(5.00));
        assert_eq!(usage.warning.as_deref(), Some(AMP_PARTIAL_WARNING));
    }

    #[test]
    fn amp_usage_keeps_parsed_limits_visible_when_a_line_is_an_unrecognized_notice() {
        let usage = parse_amp_usage_at(
            "Individual credits: $5.00 remaining\nNote: usage percentages are approximate.",
            Utc::now(),
        )
        .unwrap();
        assert_eq!(usage.limits.len(), 1);
        assert_eq!(usage.limits[0].remaining, Some(5.00));
        assert_eq!(usage.warning.as_deref(), Some(AMP_PARTIAL_WARNING));
    }

    #[test]
    fn amp_usage_keeps_parsed_limits_visible_for_an_unrecognized_new_balance_type() {
        let usage = parse_amp_usage_at(
            "Individual credits: $5.00 remaining\nTeam Demo: $50.00 remaining",
            Utc::now(),
        )
        .unwrap();
        assert_eq!(usage.limits.len(), 1);
        assert_eq!(usage.limits[0].name, "Individual credits");
        assert!(usage.warning.is_some());
    }

    #[test]
    fn amp_usage_does_not_retain_a_misleading_value_for_a_malformed_known_line() {
        let usage = parse_amp_usage_at(
            "Individual credits: $5.00 remaining\nWorkspace changed format",
            Utc::now(),
        )
        .unwrap();
        assert_eq!(usage.limits.len(), 1);
        assert!(!usage
            .limits
            .iter()
            .any(|limit| limit.name.contains("Workspace")));
        assert!(usage.warning.is_some());
    }

    #[test]
    fn amp_partial_warning_is_rendered_and_serializes_without_raw_output() {
        let mut account = test_account("billing", "AMP billing", 0.0);
        account.provider = "amp".into();
        account.limits = parse_amp_usage_at("Individual credits: $5.00 remaining", Utc::now())
            .unwrap()
            .limits;
        account.warning = AMP_PARTIAL_WARNING.into();
        let output = strip_ansi(&render(&Config::default(), &[account.clone()], false, 80));
        assert!(output.contains("not understood"));

        let probe = serde_json::to_value(&account).unwrap();
        assert_eq!(probe["warning"], AMP_PARTIAL_WARNING);
        assert!(!probe.to_string().contains("Workspace changed format"));
        assert!(!probe.to_string().contains("Team Demo"));
    }

    #[test]
    fn signed_in_amp_usage_can_include_login_advice() {
        let limits = parse_amp_usage_at(
            "Individual credits: $25.64 remaining\nRun amp login to switch accounts",
            Utc::now(),
        )
        .unwrap()
        .limits;
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].remaining, Some(25.64));
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

    #[cfg(unix)]
    #[test]
    fn successful_command_does_not_wait_for_descendant_pipe_holders() {
        let started = Instant::now();
        let output = command_stdout_with_timeout(
            Path::new("sh"),
            &["-c".into(), "sleep 2 & printf ok".into()],
            StdDuration::from_secs(1),
        )
        .unwrap();
        assert_eq!(output, b"ok");
        assert!(started.elapsed() < StdDuration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn command_failure_includes_sanitized_stderr() {
        let error = command_stdout_with_timeout(
            Path::new("sh"),
            &[
                "-c".into(),
                "printf 'not signed in; run amp login\\n' >&2; exit 1".into(),
            ],
            StdDuration::from_secs(1),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not signed in; run amp login"));
        assert!(!error.contains('\n'));
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
    fn bounded_collection_runs_mixed_collectors_concurrently_and_keeps_input_order() {
        let tasks = [
            ("slow-first", 120_u64, true),
            ("fast", 20, true),
            ("failed", 30, false),
            ("slow-last", 120, true),
            ("fast-last", 20, true),
        ];
        let active = Arc::new(AtomicUsize::new(0));
        let maximum_active = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let results = collect_with_bounded_parallelism(&tasks, 2, {
            let active = Arc::clone(&active);
            let maximum_active = Arc::clone(&maximum_active);
            move |(name, delay_ms, succeeds)| {
                let current = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                maximum_active.fetch_max(current, std::sync::atomic::Ordering::SeqCst);
                thread::sleep(StdDuration::from_millis(*delay_ms));
                active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                succeeds.then_some(*name).ok_or(*name)
            }
        });

        assert!(started.elapsed() < StdDuration::from_millis(230));
        assert_eq!(maximum_active.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            results,
            vec![
                Ok("slow-first"),
                Ok("fast"),
                Err("failed"),
                Ok("slow-last"),
                Ok("fast-last"),
            ]
        );
    }

    #[test]
    fn rejects_ambiguous_amp_account_configuration() {
        let config: Config = serde_json::from_value(json!({
            "accounts": [
                {"provider": "amp", "accountId": "default", "label": "AMP"},
                {"provider": "amp", "accountId": "work", "label": "Work AMP"},
                {
                    "provider": "amp",
                    "accountId": "settings",
                    "label": "Settings AMP",
                    "ampSettingsPath": "/tmp/amp-settings.json"
                }
            ]
        }))
        .unwrap();
        let (specs, errors) = configured_accounts(&config);
        assert_eq!(specs.len(), 1);
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|account| account
            .error
            .contains("only one Amp account can be configured")));
        assert!(errors
            .iter()
            .any(|account| account.error.contains("ampSettingsPath")));
    }

    #[test]
    fn parses_limited_and_unlimited_openrouter_keys_with_safe_identity() {
        let limited = parse_openrouter_key(
            &json!({"data": {
                "label": "sk-or-v1-…cafe",
                "is_management_key": false,
                "limit": 100.0,
                "limit_remaining": 25.0,
                "limit_reset": "monthly"
            }}),
            OpenRouterScope::Key,
        )
        .unwrap();
        assert_eq!(limited.credential_label, "sk-or-v1-…cafe");
        assert_eq!(limited.capacity_scope, "key_spending_limit");
        assert_eq!(limited.limits[0].name, "key spending limit");
        assert_eq!(limited.limits[0].remaining, Some(25.0));
        assert_eq!(limited.limits[0].remaining_percent, Some(25.0));
        assert_eq!(limited.limits[0].detail, "resets monthly");

        let unlimited = parse_openrouter_key(
            &json!({"data": {
                "label": "sk-or-v1-…beef",
                "is_management_key": false,
                "limit": null,
                "limit_remaining": null,
                "limit_reset": null
            }}),
            OpenRouterScope::Key,
        )
        .unwrap();
        assert_eq!(unlimited.limits[0].status, LimitStatus::Unknown);
        assert_eq!(unlimited.limits[0].kind, "unlimited");
        assert_eq!(limit_summary(&unlimited.limits[0]), "unlimited");
        assert!(unlimited.limits[0]
            .detail
            .contains("key is valid and unlimited"));
        assert!(unlimited.limits[0].detail.contains("management key"));

        let optional_metadata_absent = parse_openrouter_key(
            &json!({"data": {"limit": 5.0, "limit_remaining": 4.0}}),
            OpenRouterScope::Key,
        )
        .unwrap();
        assert!(optional_metadata_absent.credential_label.is_empty());
        assert_eq!(optional_metadata_absent.limits[0].remaining, Some(4.0));
    }

    #[test]
    fn management_key_reports_account_wide_credits() {
        let mut requests = 0;
        let collected = collect_openrouter_with(
            OpenRouterScope::Management,
            "management-secret",
            |url, headers| {
                requests += 1;
                assert!(headers.get(AUTHORIZATION).is_some());
                if url.ends_with("/key") {
                    Ok(json!({"data": {
                        "label": "sk-or-mgmt-…1234",
                        "is_management_key": true
                    }}))
                } else {
                    Ok(json!({"data": {"total_credits": 50.0, "total_usage": 12.5}}))
                }
            },
        )
        .unwrap();
        assert_eq!(requests, 2);
        assert_eq!(collected.credential_label, "sk-or-mgmt-…1234");
        assert_eq!(collected.capacity_scope, "account_credits");
        assert_eq!(collected.limits[0].name, "account credits");
        assert_eq!(collected.limits[0].remaining, Some(37.5));

        let optional_metadata_absent =
            parse_openrouter_key(&json!({"data": {}}), OpenRouterScope::Management).unwrap();
        assert!(optional_metadata_absent.credential_label.is_empty());
        assert_eq!(optional_metadata_absent.capacity_scope, "account_credits");
    }

    #[test]
    fn openrouter_rejects_invalid_keys_and_scope_mismatches_without_echoing_secrets() {
        let secret = "never-print-this-key";
        let error = collect_openrouter_with(OpenRouterScope::Key, secret, |_url, _headers| {
            Err(anyhow!("HTTP 401: invalid key {secret}"))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("401"));
        assert!(error.contains("invalid key"));
        assert!(error.contains("[redacted]"));
        assert!(!error.contains(secret));

        let management_as_key = parse_openrouter_key(
            &json!({"data": {"label": "masked", "is_management_key": true}}),
            OpenRouterScope::Key,
        )
        .unwrap_err()
        .to_string();
        assert!(management_as_key.contains("is a management key"));
        let key_as_management = parse_openrouter_key(
            &json!({"data": {"label": "masked", "is_management_key": false}}),
            OpenRouterScope::Management,
        )
        .unwrap_err()
        .to_string();
        assert!(key_as_management.contains("ordinary API key"));
        let deprecated_flag = parse_openrouter_key(
            &json!({"data": {"is_provisioning_key": true}}),
            OpenRouterScope::Key,
        )
        .unwrap_err()
        .to_string();
        assert!(deprecated_flag.contains("is a management key"));
    }

    #[test]
    fn resolves_keychain_secrets_and_reports_lookup_and_platform_errors() {
        let reference = SecretRef {
            kind: "macos-keychain".into(),
            service: "capacity-openrouter".into(),
            account: "management".into(),
        };
        let secret =
            resolve_secret_ref_with(&reference, true, |_| Ok(" secret-value\n".into())).unwrap();
        assert_eq!(secret, "secret-value");

        let spec: AccountSpec = serde_json::from_value(json!({
            "provider": "openrouter",
            "accountId": "default",
            "label": "OpenRouter",
            "managementSecretRef": {
                "kind": "macos-keychain",
                "service": "capacity-openrouter",
                "account": "management"
            }
        }))
        .unwrap();
        let (scope, secret) =
            openrouter_credential_with(&spec, |_| None, |_| Ok("management-secret".into()))
                .unwrap();
        assert_eq!(scope, OpenRouterScope::Management);
        assert_eq!(secret, "management-secret");

        let missing = resolve_secret_ref_with(&reference, true, |_| {
            Err(anyhow!(
                "no macOS Keychain item for the configured service and account"
            ))
        })
        .unwrap_err()
        .to_string();
        assert!(missing.contains("no macOS Keychain item"));
        assert!(
            resolve_secret_ref_with(&reference, false, |_| unreachable!())
                .unwrap_err()
                .to_string()
                .contains("require macOS")
        );

        let unsupported = SecretRef {
            kind: "vault".into(),
            ..reference
        };
        assert!(
            resolve_secret_ref_with(&unsupported, true, |_| unreachable!())
                .unwrap_err()
                .to_string()
                .contains("unsupported secret reference kind")
        );
    }

    #[test]
    fn parses_claude_usage_limits_array_and_five_hour_seven_day_fallback() {
        let limits = parse_claude_usage(&json!({
            "limits": [
                {"kind": "session", "percent": 28},
                {"kind": "weekly_all", "percent": 52, "resets_in_seconds": 3600}
            ]
        }));
        assert_eq!(
            limits
                .iter()
                .map(|limit| (limit.name.as_str(), limit.remaining_percent))
                .collect::<Vec<_>>(),
            vec![("5h", Some(72.0)), ("7d", Some(48.0))]
        );

        let fallback = parse_claude_usage(&json!({
            "five_hour": {"utilization": 10.0},
            "seven_day": {"utilization": 90.0}
        }));
        assert_eq!(
            fallback
                .iter()
                .map(|limit| (limit.name.as_str(), limit.remaining_percent))
                .collect::<Vec<_>>(),
            vec![("5h", Some(90.0)), ("7d", Some(10.0))]
        );
    }

    #[test]
    fn claude_oauth_usage_distinguishes_success_expired_and_endpoint_rejection() {
        let oauth = json!({"accessToken": "token", "expiresAt": (Utc::now() + Duration::hours(1)).to_rfc3339()});
        let success = claude_oauth_usage_with(&oauth, |_url, headers| {
            assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer token");
            Ok(json!({"five_hour": {"utilization": 20.0}, "seven_day": {"utilization": 40.0}}))
        })
        .unwrap();
        assert_eq!(success[0].remaining_percent, Some(80.0));

        let expired_oauth = json!({"accessToken": "token", "expiresAt": (Utc::now() - Duration::hours(1)).to_rfc3339()});
        let expired = claude_oauth_usage_with(&expired_oauth, |_url, _headers| unreachable!())
            .unwrap_err()
            .to_string();
        assert!(expired.contains("expired"));

        let rejected = claude_oauth_usage_with(&oauth, |_url, _headers| {
            Err(anyhow!("HTTP 403: OAuth token unauthorized for token"))
        })
        .unwrap_err()
        .to_string();
        assert!(rejected.contains("403"));
        assert!(rejected.contains("[redacted]"));
        assert!(!rejected.contains("token"));
    }

    #[test]
    fn standard_and_named_claude_credentials_collect_independently_with_distinct_reasons() {
        // Housemed: the standard `Claude Code-credentials` Keychain item, modeled
        // as the OAuth payload `claude_credentials` would hand to the collector.
        let housemed_oauth = json!({
            "accessToken": "housemed-token",
            "expiresAt": (Utc::now() + Duration::hours(1)).to_rfc3339()
        });
        let housemed_limits = claude_oauth_usage_with(&housemed_oauth, |_url, _headers| {
            Ok(json!({"five_hour": {"utilization": 30.0}, "seven_day": {"utilization": 10.0}}))
        })
        .unwrap();
        assert_eq!(housemed_limits[0].remaining_percent, Some(70.0));

        // shrivatsa-dev: a named Keychain item under
        // herdr-model-capacity-claude/shrivatsa-dev holding an OAuth-shaped
        // credential, collected independently from Housemed.
        let named = SecretRef {
            kind: "macos-keychain".into(),
            service: "herdr-model-capacity-claude".into(),
            account: "shrivatsa-dev".into(),
        };
        let shrivatsa_dev_limits = collect_anthropic_secret_ref_with(
            &named,
            |_| {
                Ok(json!({"claudeAiOauth": {
                    "accessToken": "shrivatsa-dev-token",
                    "expiresAt": (Utc::now() + Duration::hours(1)).to_rfc3339()
                }})
                .to_string())
            },
            |_url, _headers| {
                Ok(json!({"five_hour": {"utilization": 5.0}, "seven_day": {"utilization": 15.0}}))
            },
        )
        .unwrap();
        assert_eq!(shrivatsa_dev_limits[0].remaining_percent, Some(95.0));

        // A genuine setup-token-shaped secret is reported as unsupported, never
        // as zero, and never re-attempts the network.
        let setup_token = collect_anthropic_secret_ref_with(
            &named,
            |_| Ok("cc-setup-token-not-oauth-shaped".into()),
            |_url, _headers| unreachable!("setup-token credentials must not call the endpoint"),
        )
        .unwrap();
        assert_eq!(setup_token.len(), 1);
        assert_eq!(setup_token[0].status, LimitStatus::Unknown);
        assert!(setup_token[0].remaining_percent.is_none());
        assert!(setup_token[0]
            .detail
            .contains("not an OAuth-shaped payload"));
        assert!(!setup_token[0]
            .detail
            .contains("cc-setup-token-not-oauth-shaped"));
    }

    #[test]
    fn distinct_anthropic_accounts_get_unique_cache_keys_and_independent_fingerprints() {
        let housemed: AccountSpec = serde_json::from_value(json!({
            "provider": "anthropic",
            "accountId": "housemed",
            "label": "Housemed",
            "authType": "oauth",
            "allowKeychain": true
        }))
        .unwrap();
        let mut shrivatsa_dev: AccountSpec = serde_json::from_value(json!({
            "provider": "anthropic",
            "accountId": "shrivatsa-dev",
            "label": "shrivatsa-dev",
            "authType": "oauth",
            "secretRef": {
                "kind": "macos-keychain",
                "service": "herdr-model-capacity-claude",
                "account": "shrivatsa-dev"
            }
        }))
        .unwrap();

        assert_ne!(cache_path(&housemed), cache_path(&shrivatsa_dev));
        let housemed_fingerprint = collector_fingerprint(&housemed);
        let original_shrivatsa_dev_fingerprint = collector_fingerprint(&shrivatsa_dev);
        assert_ne!(housemed_fingerprint, original_shrivatsa_dev_fingerprint);

        // Changing shrivatsa-dev's credential reference invalidates only its own
        // cache entry; Housemed refreshes independently.
        shrivatsa_dev.secret_ref.as_mut().unwrap().account = "personal".into();
        assert_ne!(
            collector_fingerprint(&shrivatsa_dev),
            original_shrivatsa_dev_fingerprint
        );
        assert_eq!(collector_fingerprint(&housemed), housemed_fingerprint);
    }

    #[test]
    fn duplicate_keychain_reference_does_not_become_a_third_subscription() {
        let config: Config = serde_json::from_value(json!({
            "accounts": [
                {
                    "provider": "anthropic",
                    "accountId": "shrivatsa-dev",
                    "label": "shrivatsa-dev",
                    "secretRef": {
                        "kind": "macos-keychain",
                        "service": "herdr-model-capacity-claude",
                        "account": "shrivatsa-dev"
                    }
                },
                {
                    "provider": "anthropic",
                    "accountId": "personal",
                    "label": "Personal",
                    "secretRef": {
                        "kind": "macos-keychain",
                        "service": "herdr-model-capacity-claude",
                        "account": "shrivatsa-dev"
                    }
                }
            ]
        }))
        .unwrap();
        let (specs, errors) = configured_accounts(&config);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].account_id, "shrivatsa-dev");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("duplicate Keychain reference"));
    }

    #[test]
    fn rejects_ambiguous_openrouter_sources_and_fingerprints_secret_selectors() {
        let config: Config = serde_json::from_value(json!({
            "accounts": [{
                "provider": "openrouter",
                "accountId": "default",
                "label": "OpenRouter",
                "tokenEnv": "OPENROUTER_API_KEY",
                "tokenSecretRef": {
                    "kind": "macos-keychain",
                    "service": "capacity-openrouter",
                    "account": "ordinary"
                }
            }]
        }))
        .unwrap();
        let (specs, errors) = configured_accounts(&config);
        assert!(specs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("exactly one OpenRouter"));

        let generic_ref: Config = serde_json::from_value(json!({
            "accounts": [{
                "provider": "openrouter",
                "accountId": "wrong-ref",
                "label": "OpenRouter",
                "secretRef": {
                    "kind": "macos-keychain",
                    "service": "capacity-openrouter",
                    "account": "ordinary"
                }
            }]
        }))
        .unwrap();
        let (specs, errors) = configured_accounts(&generic_ref);
        assert!(specs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0]
            .error
            .contains("secretRef is not valid for OpenRouter"));

        let mut first: AccountSpec = serde_json::from_value(json!({
            "provider": "openrouter",
            "accountId": "default",
            "label": "OpenRouter",
            "tokenSecretRef": {
                "kind": "macos-keychain",
                "service": "capacity-openrouter",
                "account": "ordinary"
            }
        }))
        .unwrap();
        let original = collector_fingerprint(&first);
        first.token_secret_ref.as_mut().unwrap().account = "other".into();
        assert_ne!(original, collector_fingerprint(&first));
    }

    #[test]
    fn openrouter_card_omits_identity_and_scope_but_preserves_capacity_data() {
        let collected = parse_openrouter_key(
            &json!({"data": {
                "label": "sk-or-v1-…cafe",
                "is_management_key": false,
                "limit": 10.0,
                "limit_remaining": 8.0
            }}),
            OpenRouterScope::Key,
        )
        .unwrap();
        let key_account = CapacityAccount {
            provider: "openrouter".into(),
            account_id: "default".into(),
            label: "Personal OpenRouter".into(),
            auth_type: "api".into(),
            credential_label: collected.credential_label,
            capacity_scope: collected.capacity_scope,
            limits: collected.limits,
            fetched_at: Utc::now(),
            error: String::new(),
            warning: String::new(),
            collector_fingerprint: String::new(),
        };
        let credits = parse_openrouter_credits(
            &json!({"data": {"total_credits": 50.0, "total_usage": 12.5}}),
            "sk-or-mgmt-…1234".into(),
        )
        .unwrap();
        let credits_account = CapacityAccount {
            provider: "openrouter".into(),
            account_id: "management".into(),
            label: "Work OpenRouter".into(),
            auth_type: "api".into(),
            credential_label: credits.credential_label,
            capacity_scope: credits.capacity_scope,
            limits: credits.limits,
            fetched_at: Utc::now(),
            error: String::new(),
            warning: String::new(),
            collector_fingerprint: String::new(),
        };
        let accounts = [key_account, credits_account];
        let pane = strip_ansi(&render(&Config::default(), &accounts, false, 80));
        assert!(pane.contains("Personal OpenRouter"));
        assert!(pane.contains("$8.00/$10.00 remaining"));
        assert!(pane.contains("Work OpenRouter"));
        assert!(pane.contains("$37.50/$50.00 remaining"));
        assert!(!pane.contains("OpenRouter key spending limit"));
        assert!(!pane.contains("account-wide OpenRouter credits"));
        assert!(!pane.contains("key ID sk-or-v1-…cafe"));
        assert!(!pane.contains("key ID sk-or-mgmt-…1234"));
        let compact = strip_ansi(&render(&Config::default(), &accounts, true, 30));
        assert!(compact.contains("Personal OpenRouter"));
        assert!(compact.contains("$8.00/$10.00"));
        assert!(compact.contains("Work OpenRouter"));
        assert!(compact.contains("$37.50/$50.00"));
        let probe = serde_json::to_value(&accounts[0]).unwrap();
        assert_eq!(probe["credentialLabel"], "sk-or-v1-…cafe");
        assert_eq!(probe["capacityScope"], "key_spending_limit");
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
                .unwrap()
                .limits;
        limits.push(money_limit("Individual credits", 25.64, None));
        let account = CapacityAccount {
            provider: "amp".into(),
            account_id: "billing".into(),
            label: "AMP billing".into(),
            auth_type: "cli".into(),
            credential_label: String::new(),
            capacity_scope: String::new(),
            limits,
            fetched_at: Utc::now(),
            error: String::new(),
            warning: String::new(),
            collector_fingerprint: String::new(),
        };
        for width in [30, 80] {
            let output = strip_ansi(&render(
                &Config::default(),
                std::slice::from_ref(&account),
                false,
                width,
            ));
            assert!(output.contains("AMP"));
            assert!(output.contains("AMP billing"));
            assert!(output.contains("$25.64"));
            assert!(output.contains("Other"));
            assert!(output.contains("Orbs"));
        }
        let output = strip_ansi(&render(
            &Config::default(),
            std::slice::from_ref(&account),
            false,
            80,
        ));
        assert!(output.contains("Megawatt [renewal in "));
        assert!(output.contains("Other"));
        assert!(output.contains("Orbs"));
        assert!(output.contains("Available Credits"));
        assert!(!output.contains("Individual credits"));
        assert!(!output.contains("renewal reported"));
        assert!(!output.contains("$25.64 remaining"));
        assert_eq!(output.matches("renewal in").count(), 1);

        let mut narrow_account = account.clone();
        let narrow_reset = Utc::now() + Duration::days(29);
        narrow_account.limits[0].resets_at = Some(narrow_reset);
        narrow_account.limits[1].resets_at = Some(narrow_reset);
        let narrow_output = strip_ansi(&render(
            &Config::default(),
            std::slice::from_ref(&narrow_account),
            false,
            24,
        ));
        assert!(narrow_output.contains("Megawatt · "));

        let warning_output = render(
            &Config {
                critical_usd: Some(30.0),
                ..Config::default()
            },
            std::slice::from_ref(&account),
            false,
            80,
        );
        assert!(warning_output.contains("\x1b[31m$25.64\x1b[0m"));

        let mut expired = account.clone();
        expired.limits[0].resets_at = Some(Utc::now() - Duration::days(1));
        expired.limits[1].resets_at = expired.limits[0].resets_at;
        let expired_output = strip_ansi(&render(
            &Config::default(),
            std::slice::from_ref(&expired),
            false,
            80,
        ));
        assert!(!expired_output.contains("renewal in"));
        assert!(!expired_output.contains("renewal reported"));
    }

    #[test]
    fn compact_money_keeps_its_name_and_amp_free_shows_its_cap() {
        let credits = money_limit("API credits", 1234.56, None);
        let compact = render_money_limit(&credits, &Config::default(), true, true, 18);
        assert!(compact.contains("API"));
        assert!(!compact.contains("remaining"));

        let amp_free = amp_money_limit("Amp Free", "4.71", Some("10")).unwrap();
        let wide = render_money_limit(&amp_free, &Config::default(), false, true, 80);
        assert!(strip_ansi(&wide).contains("$4.71/$10.00 remaining"));
        assert!(wide.contains("\x1b[32m$4.71\x1b[0m"));
    }

    #[test]
    fn stale_unknown_money_is_not_rendered_as_zero() {
        let limit = CapacityLimit {
            name: "key limit".into(),
            kind: "credits".into(),
            unit: "usd".into(),
            remaining: None,
            total: None,
            remaining_percent: None,
            resets_at: None,
            status: LimitStatus::Stale,
            detail: "this key has no spending limit".into(),
        };
        let output = render_limit(&limit, &Config::default(), false, 60);
        assert!(output.contains("unknown"));
        assert!(!output.contains("$0.00"));
    }

    #[test]
    fn stale_cache_expires_after_a_day_and_reports_its_age() {
        let now = DateTime::parse_from_rfc3339("2026-08-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(stale_cache_is_usable(now - Duration::hours(24), now));
        assert!(!stale_cache_is_usable(
            now - Duration::hours(24) - Duration::seconds(1),
            now
        ));
        assert!(!stale_cache_is_usable(now + Duration::seconds(1), now));
        assert_eq!(stale_age_label(now - Duration::seconds(30), now), "now");
        assert_eq!(stale_age_label(now - Duration::minutes(5), now), "5m");
        assert_eq!(stale_age_label(now - Duration::hours(3), now), "3h");
    }

    fn amp_spec(account_id: &str, label: &str) -> AccountSpec {
        serde_json::from_value(json!({
            "provider": "amp",
            "accountId": account_id,
            "label": label,
            "authType": "cli"
        }))
        .unwrap()
    }

    fn cached_amp_account_with_credits(remaining: f64) -> CapacityAccount {
        CapacityAccount {
            provider: "amp".into(),
            account_id: "billing".into(),
            label: "AMP billing".into(),
            auth_type: "cli".into(),
            credential_label: String::new(),
            capacity_scope: String::new(),
            limits: vec![money_limit("Individual credits", remaining, None)],
            fetched_at: Utc::now(),
            error: String::new(),
            warning: String::new(),
            collector_fingerprint: String::new(),
        }
    }

    #[test]
    fn amp_signed_out_error_clears_cached_limits_and_becomes_unavailable() {
        let spec = amp_spec("billing", "AMP billing");
        let cached = cached_amp_account_with_credits(25.64);
        let account =
            account_for_collection_error(&spec, Some(cached), anyhow!(AMP_SIGNED_OUT_ERROR));
        assert_eq!(account.limits.len(), 1);
        assert_eq!(account.limits[0].status, LimitStatus::Unavailable);
        assert!(account.limits[0].remaining.is_none());
        assert!(!account
            .limits
            .iter()
            .any(|limit| limit.status == LimitStatus::Stale));
        assert!(account.error.contains("signed out"));
    }

    #[test]
    fn transient_amp_error_still_reuses_stale_cached_limits() {
        let spec = amp_spec("billing", "AMP billing");
        let cached = cached_amp_account_with_credits(25.64);
        let account = account_for_collection_error(
            &spec,
            Some(cached),
            anyhow!("command timed out after 10s"),
        );
        assert_eq!(account.limits.len(), 1);
        assert_eq!(account.limits[0].status, LimitStatus::Stale);
        assert_eq!(account.limits[0].remaining, Some(25.64));
    }

    #[test]
    fn fresh_failure_error_is_not_rendered_twice() {
        let error = "no Claude OAuth credential in /missing";
        let mut account = test_account("personal", "Personal", 28.0);
        account.limits[0].status = LimitStatus::Unavailable;
        account.limits[0].detail = error.into();
        account.error = error.into();
        let output = strip_ansi(&render(&Config::default(), &[account], false, 80));
        assert_eq!(output.matches(error).count(), 1);
        assert!(!output.contains("last refresh failed"));
    }

    #[test]
    fn keychain_is_opt_in_even_without_a_claude_config_directory() {
        let spec: AccountSpec = serde_json::from_value(json!({
            "provider": "anthropic",
            "accountId": "personal",
            "label": "Personal",
            "authType": "oauth",
            "source": "claude-code"
        }))
        .unwrap();
        assert!(spec.config_dir.is_none());
        assert!(!spec.allow_keychain);
    }

    #[test]
    fn claude_config_dir_fingerprint_matches_claude_code_namespacing() {
        // The service suffix Claude Code writes when CLAUDE_CONFIG_DIR points at
        // a non-default dir is the first 8 hex chars of sha256(abs_config_dir).
        // A stale Keychain item observed in the wild for
        // /Users/shrivatsa/.claude-accounts/shrivatsa-dev had service
        // "Claude Code-credentials-c7eb4308"; reproducing that hash here pins
        // the contract so the prefix workflow keeps resolving the right item.
        let dir = Path::new("/Users/shrivatsa/.claude-accounts/shrivatsa-dev");
        assert_eq!(claude_config_dir_fingerprint(dir), "c7eb4308");
    }

    #[test]
    fn claude_config_dir_prefers_explicit_then_env_then_default() {
        // Explicit configDir wins over the env var.
        let explicit: AccountSpec = serde_json::from_value(json!({
            "provider": "anthropic",
            "accountId": "a",
            "label": "A",
            "configDir": "/explicit/claude"
        }))
        .unwrap();
        env::set_var("CLAUDE_CONFIG_DIR", "/env/claude");
        assert_eq!(claude_config_dir(&explicit), PathBuf::from("/explicit/claude"));

        // No explicit configDir: fall back to CLAUDE_CONFIG_DIR, ~-expanded.
        let none: AccountSpec = serde_json::from_value(json!({
            "provider": "anthropic",
            "accountId": "a",
            "label": "A"
        }))
        .unwrap();
        assert_eq!(claude_config_dir(&none), expand_home("/env/claude"));

        // No configDir, no env: default ~/.claude.
        env::remove_var("CLAUDE_CONFIG_DIR");
        assert_eq!(claude_config_dir(&none), home_dir().join(".claude"));
    }

    #[test]
    fn long_ascii_and_cjk_amp_names_stay_within_the_pane() {
        let account = CapacityAccount {
            provider: "amp".into(),
            account_id: "billing".into(),
            label: "Very long Amp billing account label".into(),
            auth_type: "cli".into(),
            credential_label: String::new(),
            capacity_scope: String::new(),
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
            warning: String::new(),
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
            for line in output.lines() {
                let line = strip_ansi(line);
                assert!(
                    UnicodeWidthStr::width(line.as_str()) <= width,
                    "line exceeds width {width}: {line:?}"
                );
            }
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
            credential_label: String::new(),
            capacity_scope: String::new(),
            limits: vec![quota_limit("5h", Some(used), None)],
            fetched_at: Utc::now(),
            error: String::new(),
            warning: String::new(),
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
            token_secret_ref: None,
            management_secret_ref: None,
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
