//! Live quota telemetry & CodexBar-style Pace calculation for:
//! 1. Gemini Antigravity (local running `language_server` RPC `RetrieveUserQuotaSummary` & `GetUserStatus`)
//! 2. Claude Code (`https://api.anthropic.com/api/oauth/usage` via `~/.claude/.credentials.json` or Keychain)

use crate::antigravity::{now_unix_secs, AntigravityManager};
use crate::config::ConfigManager;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QuotaPace {
    pub status: String,     // "on_pace" | "reserve" | "deficit"
    pub delta_percent: f64, // positive = deficit, negative = reserve
    pub label: String,
    pub eta_label: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindowView {
    pub id: String,
    pub group: String, // "gemini" | "claude_gpt" | "claude" | "other"
    pub display_name: String,
    pub window: String, // "5h" | "weekly" | "other"
    pub window_minutes: u64,
    pub remaining_percent: f64,
    pub used_percent: f64,
    pub reset_time: String,
    pub reset_unix: Option<i64>,
    pub pace: Option<QuotaPace>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuotaSnapshot {
    pub account_key: String,
    pub email: String,
    pub plan: String,
    pub source: String, // "language_server" | "idle"
    pub updated_at: u64,
    pub error: Option<String>,
    pub buckets: Vec<QuotaWindowView>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeQuotaSnapshot {
    pub available: bool,
    pub source: String,
    pub email: String,
    pub plan: String,
    pub updated_at: u64,
    pub error: Option<String>,
    pub windows: Vec<QuotaWindowView>,
    pub extra_usage: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct QuotaCacheStore {
    pub antigravity: HashMap<String, AccountQuotaSnapshot>,
    pub claude: Option<ClaudeQuotaSnapshot>,
}

pub fn quota_cache_path() -> PathBuf {
    ConfigManager::get_app_dir().join("quota-cache.json")
}

pub fn load_quota_cache() -> QuotaCacheStore {
    fs::read_to_string(quota_cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_quota_cache(store: &QuotaCacheStore) {
    let path = quota_cache_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(store) {
        let _ = fs::write(path, json);
    }
}

/// CodexBar Pace calculation: compares actual % used against elapsed % of the window.
pub fn compute_pace(used_percent: f64, window_minutes: u64, reset_unix: Option<i64>) -> Option<QuotaPace> {
    let reset_ts = reset_unix?;
    if window_minutes == 0 {
        return None;
    }
    let now = now_unix_secs() as i64;
    let remaining_secs = reset_ts - now;
    let total_secs = (window_minutes * 60) as i64;
    if remaining_secs <= 0 || remaining_secs > total_secs {
        return None;
    }
    let elapsed_secs = total_secs - remaining_secs;
    let elapsed_frac = elapsed_secs as f64 / total_secs as f64;
    let min_elapsed = if window_minutes >= 7 * 24 * 60 { 0.01 } else { 0.03 };
    if elapsed_frac < min_elapsed {
        return None;
    }

    let expected_used = elapsed_frac * 100.0;
    let delta = used_percent - expected_used;

    if delta.abs() < 2.0 {
        return Some(QuotaPace {
            status: "on_pace".to_string(),
            delta_percent: 0.0,
            label: "Đúng nhịp (On pace)".to_string(),
            eta_label: "Đủ dùng đến khi reset".to_string(),
        });
    }
    if delta < 0.0 {
        return Some(QuotaPace {
            status: "reserve".to_string(),
            delta_percent: delta.round(),
            label: format!("Dư {:.0}% (in reserve)", -delta),
            eta_label: "Đủ dùng đến khi reset".to_string(),
        });
    }

    let burn_rate = used_percent / (elapsed_secs as f64);
    let left_pct = (100.0 - used_percent).max(0.0);
    let eta_label = if burn_rate > 0.0 && left_pct > 0.0 {
        let run_out_secs = (left_pct / burn_rate) as i64;
        if run_out_secs < remaining_secs {
            format!("Dự kiến hết sau {}", format_short_duration(run_out_secs))
        } else {
            "Đủ dùng đến khi reset".to_string()
        }
    } else if left_pct <= 0.0 {
        "Đã cạn hạn mức".to_string()
    } else {
        "Đủ dùng đến khi reset".to_string()
    };

    Some(QuotaPace {
        status: "deficit".to_string(),
        delta_percent: delta.round(),
        label: format!("Vượt {:.0}% (in deficit)", delta),
        eta_label,
    })
}

pub fn format_short_duration(total_secs: i64) -> String {
    if total_secs <= 0 {
        return "0m".to_string();
    }
    let d = total_secs / 86400;
    let h = (total_secs % 86400) / 3600;
    let m = (total_secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{}m", m.max(1))
    }
}

/// Parse ISO-8601 timestamp ("2026-11-24T12:34:56Z" or with offset) into unix seconds.
pub fn parse_rfc3339_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return s.parse::<i64>().ok();
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let hh: i64 = s.get(11..13)?.parse().ok()?;
    let mm: i64 = s.get(14..16)?.parse().ok()?;
    let ss: i64 = s.get(17..19)?.parse().ok()?;

    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut unix = days * 86400 + hh * 3600 + mm * 60 + ss;

    let tz_part = &s[19..];
    if let Some(sign_pos) = tz_part.find(['+', '-']) {
        let sign = if &tz_part[sign_pos..sign_pos + 1] == "+" { 1 } else { -1 };
        let off = &tz_part[sign_pos + 1..];
        let oh: i64 = off.get(0..2).and_then(|x| x.parse().ok()).unwrap_or(0);
        let om: i64 = off
            .get(3..5)
            .or_else(|| off.get(2..4))
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        unix -= sign * (oh * 3600 + om * 60);
    }
    Some(unix)
}

fn make_window(
    id: String,
    group: &str,
    display_name: String,
    window: &str,
    remaining_fraction: f64,
    reset_time: String,
) -> QuotaWindowView {
    let clamped = remaining_fraction.clamp(0.0, 1.0);
    let remaining_percent = (clamped * 1000.0).round() / 10.0;
    let used_percent = ((1.0 - clamped) * 1000.0).round() / 10.0;
    let window_minutes = if window.eq_ignore_ascii_case("weekly") {
        7 * 24 * 60
    } else if window.eq_ignore_ascii_case("5h") {
        300
    } else {
        0
    };
    let reset_unix = if reset_time.is_empty() {
        None
    } else {
        parse_rfc3339_unix(&reset_time).filter(|&ts| ts > 86400)
    };
    let pace = compute_pace(used_percent, window_minutes, reset_unix);
    QuotaWindowView {
        id,
        group: group.to_string(),
        display_name,
        window: window.to_lowercase(),
        window_minutes,
        remaining_percent,
        used_percent,
        reset_time: if reset_unix.is_some() { reset_time } else { String::new() },
        reset_unix,
        pace,
    }
}

/// Probe local Antigravity `language_server` or `agy` CLI and update the quota cache.
/// Single-flight guarded so concurrent callers never spawn duplicate `agy` CLI processes.
pub async fn refresh_antigravity_quota() -> Result<HashMap<String, AccountQuotaSnapshot>, String> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;

    let st = AntigravityManager::status().unwrap_or_default();
    let active_key = st["active"].as_str().unwrap_or("").to_string();
    let current_email = st["email"].as_str().unwrap_or("").to_string();
    let target_key = if !active_key.is_empty() {
        active_key
    } else if !current_email.is_empty() {
        "__current__".to_string()
    } else {
        "__current__".to_string()
    };

    let mut cache = load_quota_cache();
    let now = now_unix_secs();

    let probe_res = match probe_local_language_server(&target_key).await {
        Some((buckets, email, plan)) => Some((buckets, email, plan, "language_server".to_string())),
        None => probe_agy_cli_usage(&target_key)
            .await
            .map(|buckets| (buckets, String::new(), String::new(), "agy_cli".to_string())),
    };

    if let Some((buckets, ls_email, ls_plan, source)) = probe_res {
        let email = if !ls_email.is_empty() { ls_email } else { current_email.clone() };
        let resolved_key = if target_key != "__current__" {
            target_key
        } else if let Ok(list) = AntigravityManager::load_list() {
            list.accounts
                .iter()
                .find(|a| !email.is_empty() && a.email.eq_ignore_ascii_case(&email))
                .map(|a| a.key.clone())
                .unwrap_or_else(|| "__current__".to_string())
        } else {
            "__current__".to_string()
        };

        let snap = AccountQuotaSnapshot {
            account_key: resolved_key.clone(),
            email,
            plan: ls_plan,
            source,
            updated_at: now,
            error: None,
            buckets,
        };
        cache.antigravity.insert(resolved_key, snap);
        save_quota_cache(&cache);
    }

    Ok(cache.antigravity)
}

fn parse_quota_summary_json(account_key: &str, root: &serde_json::Value) -> Vec<QuotaWindowView> {
    let payload = root
        .pointer("/command/data")
        .or_else(|| root.get("response"))
        .or_else(|| root.get("summary"))
        .unwrap_or(root);
    let mut out = Vec::new();

    if let Some(groups) = payload.get("groups").and_then(|g| g.as_array()) {
        for g in groups {
            let gname = g
                .get("displayName")
                .or_else(|| g.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let glower = gname.to_ascii_lowercase();
            let (group_id, clean_group) = if glower.contains("claude") || glower.contains("gpt") {
                ("claude_gpt", "Claude & GPT")
            } else if glower.contains("gemini") {
                ("gemini", "Gemini Models")
            } else {
                ("other", gname)
            };

            if let Some(buckets) = g.get("buckets").and_then(|b| b.as_array()) {
                for b in buckets {
                    if b.get("disabled").and_then(|x| x.as_bool()).unwrap_or(false) {
                        continue;
                    }
                    let bid = b
                        .get("bucketId")
                        .or_else(|| b.get("bucket_id"))
                        .or_else(|| b.get("id"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("bucket");
                    let win_raw = b.get("window").and_then(|x| x.as_str()).unwrap_or("");
                    let wlower = format!("{win_raw} {bid}").to_ascii_lowercase();
                    let window = if wlower.contains("week") { "weekly" } else { "5h" };
                    let frac = b
                        .get("remainingFraction")
                        .or_else(|| b.get("remaining_fraction"))
                        .or_else(|| b.pointer("/remaining/remainingFraction"))
                        .or_else(|| b.pointer("/remaining/remaining_fraction"))
                        .and_then(|x| x.as_f64());
                    let Some(frac) = frac else { continue };
                    let reset = b
                        .get("resetTime")
                        .or_else(|| b.get("reset_time"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let label = format!(
                        "{clean_group} ({})",
                        if window == "weekly" { "Weekly" } else { "5h" }
                    );
                    out.push(make_window(
                        format!("{account_key}-{bid}"),
                        group_id,
                        label,
                        window,
                        frac,
                        reset,
                    ));
                }
            }
        }
    }

    // Sort cleanly: Gemini before Claude/GPT, and 5h before Weekly within each group.
    out.sort_by_key(|w| {
        let gr = match w.group.as_str() {
            "gemini" => 0,
            "claude_gpt" => 1,
            _ => 2,
        };
        let wr = match w.window.as_str() {
            "5h" => 0,
            "weekly" => 1,
            _ => 2,
        };
        (gr, wr, w.display_name.clone())
    });
    out
}

fn resolve_agy_binary() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(home) = ConfigManager::home() {
        candidates.push(home.join(".gemini/bin/agy"));
        candidates.push(home.join(".local/bin/agy"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/agy"));
    candidates.push(PathBuf::from("/usr/local/bin/agy"));
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    if let Ok(out) = std::process::Command::new("which").arg("agy").output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    None
}

/// Fallback for Antigravity 2.35+: `agy -p /usage --output-format json --print-timeout 20s`
async fn probe_agy_cli_usage(account_key: &str) -> Option<Vec<QuotaWindowView>> {
    let bin = resolve_agy_binary()?;
    let key_owned = account_key.to_string();
    let task = tokio::task::spawn_blocking(move || {
        use std::process::Stdio;
        let out = std::process::Command::new(&bin)
            .args(["-p", "/usage", "--output-format", "json", "--print-timeout", "18s"])
            .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let txt = String::from_utf8_lossy(&out.stdout);
        let v: serde_json::Value = serde_json::from_str(txt.trim()).ok()?;
        if v.get("status").and_then(|s| s.as_str()) != Some("SUCCESS") {
            return None;
        }
        let buckets = parse_quota_summary_json(&key_owned, &v);
        if buckets.is_empty() { None } else { Some(buckets) }
    });
    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .ok()?
        .ok()?
}

/// Probe locally running Antigravity `language_server` (macOS/Linux) for real-time quota summary.
async fn probe_local_language_server(
    account_key: &str,
) -> Option<(Vec<QuotaWindowView>, String, String)> {
    #[cfg(not(unix))]
    {
        let _ = account_key;
        return None;
    }
    #[cfg(unix)]
    {
        let ps_out = std::process::Command::new("ps")
            .args(["-ax", "-o", "pid=,command="])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&ps_out.stdout);
        let mut target_pid = String::new();
        let mut csrf = String::new();

        for line in stdout.lines() {
            if !line.contains("language_server") || !line.contains("--csrf_token") {
                continue;
            }
            if !line.to_ascii_lowercase().contains("antigravity") {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            for w in parts.windows(2) {
                if w[0] == "--csrf_token" {
                    target_pid = parts[0].to_string();
                    csrf = w[1].to_string();
                    break;
                }
            }
            if !csrf.is_empty() {
                break;
            }
        }
        if csrf.is_empty() || target_pid.is_empty() {
            return None;
        }

        let lsof_out = std::process::Command::new("lsof")
            .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", &target_pid])
            .output()
            .ok()?;
        let mut ports = Vec::new();
        for line in String::from_utf8_lossy(&lsof_out.stdout).lines() {
            if let Some(listen_idx) = line.find("(LISTEN)") {
                let before = line[..listen_idx].trim();
                if let Some(colon) = before.rfind(':') {
                    if let Ok(p) = before[colon + 1..].parse::<u16>() {
                        if !ports.contains(&p) {
                            ports.push(p);
                        }
                    }
                }
            }
        }
        if ports.is_empty() {
            return None;
        }

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_millis(1800))
            .no_proxy()
            .build()
            .ok()?;

        for port in ports {
            for scheme in ["https", "http"] {
                let url = format!("{scheme}://127.0.0.1:{port}/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary");
                if let Ok(resp) = client
                    .post(&url)
                    .header("x-codeium-csrf-token", &csrf)
                    .header("Connect-Protocol-Version", "1")
                    .header("content-type", "application/json")
                    .body(r#"{"forceRefresh":true}"#)
                    .send()
                    .await
                {
                    if resp.status().is_success() {
                        if let Ok(txt) = resp.text().await {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                                let buckets = parse_quota_summary_json(account_key, &v);
                                if !buckets.is_empty() {
                                    let mut email = String::new();
                                    let mut plan = String::new();
                                    let status_url = format!("{scheme}://127.0.0.1:{port}/exa.language_server_pb.LanguageServerService/GetUserStatus");
                                    if let Ok(sr) = client
                                        .post(&status_url)
                                        .header("x-codeium-csrf-token", &csrf)
                                        .header("Connect-Protocol-Version", "1")
                                        .header("content-type", "application/json")
                                        .body("{}")
                                        .send()
                                        .await
                                    {
                                        if let Ok(stxt) = sr.text().await {
                                            if let Ok(sv) = serde_json::from_str::<serde_json::Value>(&stxt) {
                                                email = sv
                                                    .pointer("/userStatus/email")
                                                    .and_then(|x| x.as_str())
                                                    .unwrap_or("")
                                                    .to_string();
                                                plan = sv
                                                    .pointer("/userStatus/planStatus/planInfo/planName")
                                                    .and_then(|x| x.as_str())
                                                    .unwrap_or("")
                                                    .to_string();
                                            }
                                        }
                                    }
                                    return Some((buckets, email, plan));
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

/// Fetch Claude Code OAuth Quota (`five_hour` session, `seven_day` weekly, `seven_day_sonnet`, `extra_usage`).
pub async fn refresh_claude_quota(allow_keychain: bool) -> ClaudeQuotaSnapshot {
    let now = now_unix_secs();
    let native = ConfigManager::native_info().unwrap_or_default();
    let native_email = native.get("email").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let native_plan = native.get("billingType").and_then(|x| x.as_str()).unwrap_or("").to_string();

    let Some((access_token, source, plan_hint)) = read_claude_oauth_token(allow_keychain) else {
        let snap = ClaudeQuotaSnapshot {
            available: false,
            source: "none".to_string(),
            email: native_email,
            plan: native_plan,
            updated_at: now,
            error: Some("Chưa tìm thấy token Claude OAuth (~/.claude/.credentials.json)".to_string()),
            windows: Vec::new(),
            extra_usage: None,
        };
        let mut cache = load_quota_cache();
        cache.claude = Some(snap.clone());
        save_quota_cache(&cache);
        return snap;
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .bearer_auth(&access_token)
        .header("Accept", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "claude-cli/2.1.0 (external, cli)")
        .send()
        .await;

    let snap = match resp {
        Ok(r) if r.status().is_success() => {
            if let Ok(txt) = r.text().await {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                    let mut windows = Vec::new();
                    let mut push_win =
                        |id: &str, title: &str, win_name: &str, obj: Option<&serde_json::Value>| {
                            if let Some(o) = obj.filter(|x| x.is_object()) {
                                if let Some(util) = o.get("utilization").and_then(|x| x.as_f64()) {
                                    let used_frac = if util > 1.0 { util / 100.0 } else { util };
                                    let resets = o
                                        .get("resets_at")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    windows.push(make_window(
                                        id.to_string(),
                                        "claude",
                                        title.to_string(),
                                        win_name,
                                        (1.0 - used_frac).clamp(0.0, 1.0),
                                        resets,
                                    ));
                                }
                            }
                        };
                    push_win("claude-session", "Session (5h)", "5h", v.get("five_hour"));
                    push_win(
                        "claude-weekly",
                        "Weekly (7 ngày)",
                        "weekly",
                        v.get("seven_day").or_else(|| v.get("seven_day_oauth_apps")),
                    );
                    push_win("claude-sonnet", "Sonnet Weekly", "weekly", v.get("seven_day_sonnet"));
                    push_win("claude-opus", "Opus Weekly", "weekly", v.get("seven_day_opus"));

                    let extra_usage = v.get("extra_usage").filter(|x| x.is_object()).cloned();
                    ClaudeQuotaSnapshot {
                        available: !windows.is_empty() || extra_usage.is_some(),
                        source,
                        email: native_email,
                        plan: if !plan_hint.is_empty() { plan_hint } else { native_plan },
                        updated_at: now,
                        error: None,
                        windows,
                        extra_usage,
                    }
                } else {
                    ClaudeQuotaSnapshot {
                        available: false,
                        source,
                        email: native_email,
                        plan: plan_hint,
                        updated_at: now,
                        error: Some("Không thể đọc phản hồi Claude OAuth usage".to_string()),
                        windows: Vec::new(),
                        extra_usage: None,
                    }
                }
            } else {
                ClaudeQuotaSnapshot {
                    available: false,
                    source,
                    email: native_email,
                    plan: plan_hint,
                    updated_at: now,
                    error: Some("Không đọc được nội dung Claude OAuth".to_string()),
                    windows: Vec::new(),
                    extra_usage: None,
                }
            }
        }
        Ok(r) => ClaudeQuotaSnapshot {
            available: false,
            source,
            email: native_email,
            plan: plan_hint,
            updated_at: now,
            error: Some(format!("Claude OAuth trả về HTTP {}", r.status().as_u16())),
            windows: Vec::new(),
            extra_usage: None,
        },
        Err(e) => ClaudeQuotaSnapshot {
            available: false,
            source,
            email: native_email,
            plan: plan_hint,
            updated_at: now,
            error: Some(format!("Lỗi kết nối Claude OAuth: {e}")),
            windows: Vec::new(),
            extra_usage: None,
        },
    };

    let mut cache = load_quota_cache();
    cache.claude = Some(snap.clone());
    save_quota_cache(&cache);
    snap
}

fn read_claude_oauth_token(allow_keychain: bool) -> Option<(String, String, String)> {
    if let Ok(home) = ConfigManager::home() {
        if let Ok(raw) = fs::read_to_string(home.join(".claude/.credentials.json")) {
            if let Some((tok, plan)) = parse_claude_cred_json(&raw) {
                return Some((tok, "credentials_file".to_string(), plan));
            }
        }
    }
    #[cfg(target_os = "macos")]
    if allow_keychain {
        if let Ok(out) = std::process::Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .output()
        {
            if out.status.success() {
                let raw = String::from_utf8_lossy(&out.stdout);
                if let Some((tok, plan)) = parse_claude_cred_json(raw.trim()) {
                    return Some((tok, "keychain".to_string(), plan));
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = allow_keychain;

    None
}

fn parse_claude_cred_json(raw: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let oauth = v.get("claudeAiOauth").unwrap_or(&v);
    let token = oauth
        .get("accessToken")
        .or_else(|| oauth.get("access_token"))
        .and_then(|x| x.as_str())?
        .trim()
        .to_string();
    if token.is_empty() {
        return None;
    }
    let tier = oauth
        .get("rateLimitTier")
        .or_else(|| oauth.get("subscriptionType"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let plan = if tier.contains("max_20x") {
        "Max 20x"
    } else if tier.contains("max_5x") {
        "Max 5x"
    } else if tier.to_ascii_lowercase().contains("max") {
        "Max"
    } else if tier.to_ascii_lowercase().contains("pro") {
        "Pro"
    } else if tier.to_ascii_lowercase().contains("team") {
        "Team"
    } else {
        tier
    }
    .to_string();
    Some((token, plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pace_reserve_and_deficit() {
        let now = now_unix_secs() as i64;
        let p_res = compute_pace(20.0, 300, Some(now + 9000)).unwrap();
        assert_eq!(p_res.status, "reserve");
        assert!(p_res.delta_percent < 0.0);

        let p_def = compute_pace(80.0, 300, Some(now + 9000)).unwrap();
        assert_eq!(p_def.status, "deficit");
        assert!(p_def.delta_percent > 0.0);
        assert!(p_def.eta_label.contains("Dự kiến hết"));
    }

    #[test]
    fn parses_quota_summary_groups() {
        let json = serde_json::json!({
            "response": {
                "groups": [
                    {
                        "displayName": "Gemini Models",
                        "buckets": [
                            { "bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.76, "resetTime": "2026-11-24T15:00:00Z" },
                            { "bucketId": "gemini-weekly", "window": "weekly", "remainingFraction": 0.91, "resetTime": "2026-11-29T00:00:00Z" }
                        ]
                    },
                    {
                        "displayName": "Claude and GPT models",
                        "buckets": [
                            { "bucketId": "3p-5h", "window": "5h", "remainingFraction": 0.42, "resetTime": "2026-11-24T14:00:00Z" }
                        ]
                    }
                ]
            }
        });
        let buckets = parse_quota_summary_json("acc1", &json);
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].group, "gemini");
        assert_eq!(buckets[0].remaining_percent, 76.0);
        assert_eq!(buckets[2].group, "claude_gpt");
        assert_eq!(buckets[2].remaining_percent, 42.0);
    }

    #[test]
    fn parses_agy_cli_usage_output() {
        let json = serde_json::json!({
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "buckets": [
                                { "id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.703, "reset_time": "2026-11-30T22:16:47Z" },
                                { "id": "gemini-5h", "window": "5h", "remaining_fraction": 0.905, "reset_time": "2026-11-24T09:41:47Z" }
                            ]
                        },
                        {
                            "name": "Claude and GPT models",
                            "buckets": [
                                { "id": "3p-5h", "window": "5h", "remaining_fraction": 1.0, "reset_time": "1970-01-01T00:00:00Z" }
                            ]
                        }
                    ]
                }
            }
        });
        let buckets = parse_quota_summary_json("P-PRO-01", &json);
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].id, "P-PRO-01-gemini-5h");
        assert_eq!(buckets[0].remaining_percent, 90.5);
        assert_eq!(buckets[1].id, "P-PRO-01-gemini-weekly");
        assert_eq!(buckets[2].id, "P-PRO-01-3p-5h");
        assert_eq!(buckets[2].reset_unix, None);
    }
}
