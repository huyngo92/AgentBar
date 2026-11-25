//! Portable Backup / Export & Import for all application data:
//! - Claude Gateway profiles (`profiles.json`)
//! - Claude Failover proxy settings (`failover.json`)
//! - Gemini Antigravity pre-declared account list (`antigravity.json`)
//! - Optional Antigravity login snapshots (`antigravity-sessions.json`) with 0600 permissions
//! - App preferences (`preferences.json`)
//! - 90-day Claude & Antigravity token usage history (`usage.json`, `usage_antigravity.json`)
//! - Automatic local safety snapshots in `<app_dir>/backups/`

use crate::antigravity::{now_unix_secs, AccountList, AntigravityManager, Vault};
use crate::config::{AppPreferences, ConfigManager, ProfilesConfig};
use crate::failover::{self, FailoverSettings};
use crate::usage::{self, Store as UsageStore};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const BACKUP_FORMAT: &str = "claude-gemini-gateway-backup";
pub const BACKUP_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BackupBundle {
    pub format: String,
    pub version: u32,
    pub exported_at: u64,
    pub include_secrets: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<ProfilesConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover: Option<FailoverSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antigravity: Option<AccountList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antigravity_sessions: Option<Vault>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferences: Option<AppPreferences>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_claude: Option<UsageStore>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_antigravity: Option<UsageStore>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub detected_format: String,
    pub mode: String,
    pub claude_profiles: usize,
    pub failover_chain: usize,
    pub antigravity_accounts: usize,
    pub antigravity_sessions: usize,
    pub usage_days_merged: usize,
    pub safety_snapshot: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEntry {
    pub filename: String,
    pub path: String,
    pub size_bytes: u64,
    pub created_at: u64,
    pub include_secrets: bool,
    pub claude_profiles: usize,
    pub antigravity_accounts: usize,
}

pub fn backups_dir() -> PathBuf {
    ConfigManager::get_app_dir().join("backups")
}

/// Build an in-memory backup bundle of all current data.
pub fn create_bundle(include_secrets: bool) -> Result<BackupBundle, String> {
    let app_dir = ConfigManager::get_app_dir();
    let mut profiles = ConfigManager::load_profiles().ok();
    if !include_secrets {
        if let Some(ref mut cfg) = profiles {
            for p in cfg.profiles.values_mut() {
                p.env.remove("ANTHROPIC_AUTH_TOKEN");
                p.env.remove("ANTHROPIC_API_KEY");
                p.env.remove("ANTHROPIC_CUSTOM_HEADERS");
            }
        }
    }

    let mut failover_cfg = failover::load();
    if !include_secrets {
        failover_cfg.secret.clear();
    }
    let failover = Some(failover_cfg);

    let antigravity = AntigravityManager::load_list().ok();
    let antigravity_sessions = if include_secrets {
        Some(AntigravityManager::load_vault())
    } else {
        None
    };
    let preferences = Some(ConfigManager::load_preferences());

    let usage_claude: Option<UsageStore> = fs::read_to_string(usage::store_path(&app_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let usage_antigravity: Option<UsageStore> =
        fs::read_to_string(usage::antigravity_store_path(&app_dir))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());

    Ok(BackupBundle {
        format: BACKUP_FORMAT.to_string(),
        version: BACKUP_VERSION,
        exported_at: now_unix_secs(),
        include_secrets,
        profiles,
        failover,
        antigravity,
        antigravity_sessions,
        preferences,
        usage_claude,
        usage_antigravity,
    })
}

/// Export backup bundle to a specific destination file (`0600` when secrets are included).
pub fn export_to_file(dest: &Path, include_secrets: bool) -> Result<serde_json::Value, String> {
    let bundle = create_bundle(include_secrets)?;
    let json = serde_json::to_string_pretty(&bundle).map_err(|e| e.to_string())? + "\n";
    if include_secrets {
        ConfigManager::write_private(dest, &json)?;
    } else {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(dest, &json).map_err(|e| e.to_string())?;
    }
    Ok(serde_json::json!({
        "path": dest.to_string_lossy(),
        "sizeBytes": json.len(),
        "includeSecrets": include_secrets,
        "claudeProfiles": bundle.profiles.as_ref().map(|p| p.profiles.len()).unwrap_or(0),
        "failoverChain": bundle.failover.as_ref().map(|f| f.chain.len()).unwrap_or(0),
        "antigravityAccounts": bundle.antigravity.as_ref().map(|a| a.accounts.len()).unwrap_or(0),
        "antigravitySessions": bundle.antigravity_sessions.as_ref().map(|v| v.accounts.len()).unwrap_or(0),
    }))
}

/// Create a timestamped local snapshot inside `<app_dir>/backups/`.
pub fn create_local_snapshot(prefix: &str, include_secrets: bool) -> Result<SnapshotEntry, String> {
    let dir = backups_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ts = now_unix_secs();
    let mut filename = format!("{prefix}-{ts}.json");
    let mut dest = dir.join(&filename);
    let mut seq = 1;
    while dest.exists() {
        filename = format!("{prefix}-{ts}-{seq}.json");
        dest = dir.join(&filename);
        seq += 1;
    }
    let res = export_to_file(&dest, include_secrets)?;
    // Keep at most 20 newest snapshots
    prune_old_snapshots(&dir, 20);
    Ok(SnapshotEntry {
        filename,
        path: dest.to_string_lossy().to_string(),
        size_bytes: res["sizeBytes"].as_u64().unwrap_or(0),
        created_at: ts,
        include_secrets,
        claude_profiles: res["claudeProfiles"].as_u64().unwrap_or(0) as usize,
        antigravity_accounts: res["antigravityAccounts"].as_u64().unwrap_or(0) as usize,
    })
}

pub fn list_local_snapshots() -> Vec<SnapshotEntry> {
    let dir = backups_dir();
    let Ok(rd) = fs::read_dir(&dir) else { return Vec::new() };
    let mut items = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let size_bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
        let (created_at, include_secrets, claude_profiles, antigravity_accounts) =
            fs::read_to_string(&p)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .map(|v| {
                    let ts = v.get("exportedAt").and_then(|x| x.as_u64()).unwrap_or(0);
                    let sec = v.get("includeSecrets").and_then(|x| x.as_bool()).unwrap_or(false);
                    let cp = v
                        .pointer("/profiles/profiles")
                        .and_then(|x| x.as_object())
                        .map(|m| m.len())
                        .unwrap_or(0);
                    let ag = v
                        .pointer("/antigravity/accounts")
                        .and_then(|x| x.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    (ts, sec, cp, ag)
                })
                .unwrap_or((0, false, 0, 0));
        items.push(SnapshotEntry {
            filename: p.file_name().unwrap_or_default().to_string_lossy().to_string(),
            path: p.to_string_lossy().to_string(),
            size_bytes,
            created_at,
            include_secrets,
            claude_profiles,
            antigravity_accounts,
        });
    }
    items.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.filename.cmp(&a.filename)));
    items
}

pub fn delete_local_snapshot(filename: &str) -> Result<(), String> {
    let clean = Path::new(filename)
        .file_name()
        .ok_or("Tên file snapshot không hợp lệ")?;
    let path = backups_dir().join(clean);
    if !path.is_file() {
        return Err("Không tìm thấy snapshot".to_string());
    }
    fs::remove_file(path).map_err(|e| e.to_string())
}

fn prune_old_snapshots(dir: &Path, max_keep: usize) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .collect();
    if entries.len() <= max_keep {
        return;
    }
    entries.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    let remove_count = entries.len().saturating_sub(max_keep);
    for e in entries.into_iter().take(remove_count) {
        let _ = fs::remove_file(e.path());
    }
}

/// Smart import: auto-detects full `BackupBundle`, plain `profiles.json`,
/// or plain `antigravity.json`. Supports `"merge"` (default) and `"replace"`.
pub fn import_from_file(src: &Path, mode: &str) -> Result<ImportSummary, String> {
    let raw = fs::read_to_string(src).map_err(|e| format!("Không thể đọc file backup: {e}"))?;
    let val: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("File JSON không hợp lệ: {e}"))?;

    // Always create a safety snapshot before mutating state
    let safety = create_local_snapshot("pre-import", true)
        .map(|s| s.filename)
        .unwrap_or_default();

    let replace = mode.eq_ignore_ascii_case("replace");
    let effective_mode = if replace { "replace" } else { "merge" }.to_string();

    // Case 1: Full BackupBundle
    if val.get("format").and_then(|x| x.as_str()) == Some(BACKUP_FORMAT) {
        let bundle: BackupBundle = serde_json::from_value(val).map_err(|e| e.to_string())?;
        let mut summary = ImportSummary {
            detected_format: "full_backup".to_string(),
            mode: effective_mode,
            claude_profiles: 0,
            failover_chain: 0,
            antigravity_accounts: 0,
            antigravity_sessions: 0,
            usage_days_merged: 0,
            safety_snapshot: safety,
        };

        if let Some(imp_profiles) = bundle.profiles {
            summary.claude_profiles = imp_profiles.profiles.len();
            if replace {
                ConfigManager::save_profiles(&imp_profiles)?;
            } else {
                let mut cur = ConfigManager::load_profiles()?;
                for (k, mut p) in imp_profiles.profiles {
                    // Preserve existing secret tokens if sanitized backup omitted them
                    if let Some(existing) = cur.profiles.get(&k) {
                        for secret_k in [
                            "ANTHROPIC_AUTH_TOKEN",
                            "ANTHROPIC_API_KEY",
                            "ANTHROPIC_CUSTOM_HEADERS",
                        ] {
                            if !p.env.contains_key(secret_k) {
                                if let Some(ev) = existing.env.get(secret_k) {
                                    p.env.insert(secret_k.to_string(), ev.clone());
                                }
                            }
                        }
                    }
                    cur.profiles.insert(k, p);
                }
                if cur.active.is_empty() && !imp_profiles.active.is_empty() {
                    cur.active = imp_profiles.active;
                }
                ConfigManager::save_profiles(&cur)?;
            }
        }

        if let Some(mut imp_fo) = bundle.failover {
            summary.failover_chain = imp_fo.chain.len();
            let cur = failover::load();
            if imp_fo.secret.trim().is_empty() {
                imp_fo.secret = cur.secret;
            }
            if replace || cur.chain.is_empty() || !imp_fo.chain.is_empty() {
                let _ = failover::save(&imp_fo);
            }
        }

        if let Some(imp_ag) = bundle.antigravity {
            summary.antigravity_accounts = imp_ag.accounts.len();
            if replace {
                AntigravityManager::save_list(&imp_ag)?;
            } else {
                let mut cur = AntigravityManager::load_list()?;
                for acc in imp_ag.accounts {
                    if let Some(pos) = cur.accounts.iter().position(|x| x.key == acc.key) {
                        let existing_saved_at = cur.accounts[pos].saved_at;
                        cur.accounts[pos] = acc;
                        if cur.accounts[pos].saved_at == 0 {
                            cur.accounts[pos].saved_at = existing_saved_at;
                        }
                    } else {
                        cur.accounts.push(acc);
                    }
                }
                if cur.active.is_empty() && !imp_ag.active.is_empty() {
                    cur.active = imp_ag.active;
                }
                AntigravityManager::save_list(&cur)?;
            }
        }

        if let Some(imp_vault) = bundle.antigravity_sessions {
            summary.antigravity_sessions = imp_vault.accounts.len();
            if replace {
                AntigravityManager::save_vault(&imp_vault)?;
            } else {
                let mut cur = AntigravityManager::load_vault();
                for (k, snap) in imp_vault.accounts {
                    if snap.is_logged_in() {
                        cur.accounts.insert(k, snap);
                    }
                }
                if cur.original.is_none() && imp_vault.original.is_some() {
                    cur.original = imp_vault.original;
                }
                AntigravityManager::save_vault(&cur)?;
            }
        }

        if let Some(prefs) = bundle.preferences {
            if replace {
                let _ = ConfigManager::save_preferences(&prefs);
            } else {
                let mut cur = ConfigManager::load_preferences();
                cur.theme = prefs.theme;
                cur.percent_style = prefs.percent_style;
                cur.privacy_mode = prefs.privacy_mode;
                if cur.profiles_sync_path.is_empty() {
                    cur.profiles_sync_path = prefs.profiles_sync_path;
                }
                if cur.antigravity_sync_path.is_empty() {
                    cur.antigravity_sync_path = prefs.antigravity_sync_path;
                }
                let _ = ConfigManager::save_preferences(&cur);
            }
        }

        let app_dir = ConfigManager::get_app_dir();
        if let Some(u) = bundle.usage_claude {
            summary.usage_days_merged += merge_usage_file(&usage::store_path(&app_dir), u, replace)?;
        }
        if let Some(u) = bundle.usage_antigravity {
            summary.usage_days_merged +=
                merge_usage_file(&usage::antigravity_store_path(&app_dir), u, replace)?;
        }

        return Ok(summary);
    }

    // Case 2: Plain Antigravity pre-declared accounts file (`antigravity.json`)
    if val.get("accounts").and_then(|x| x.as_array()).is_some() {
        let imp_ag: AccountList = serde_json::from_value(val).map_err(|e| e.to_string())?;
        let count = imp_ag.accounts.len();
        if replace {
            AntigravityManager::save_list(&imp_ag)?;
        } else {
            let mut cur = AntigravityManager::load_list()?;
            for acc in imp_ag.accounts {
                if let Some(pos) = cur.accounts.iter().position(|x| x.key == acc.key) {
                    let prev_saved = cur.accounts[pos].saved_at;
                    cur.accounts[pos] = acc;
                    if cur.accounts[pos].saved_at == 0 {
                        cur.accounts[pos].saved_at = prev_saved;
                    }
                } else {
                    cur.accounts.push(acc);
                }
            }
            AntigravityManager::save_list(&cur)?;
        }
        return Ok(ImportSummary {
            detected_format: "antigravity_json".to_string(),
            mode: effective_mode,
            claude_profiles: 0,
            failover_chain: 0,
            antigravity_accounts: count,
            antigravity_sessions: 0,
            usage_days_merged: 0,
            safety_snapshot: safety,
        });
    }

    // Case 3: Plain Claude `profiles.json`
    if val.get("profiles").and_then(|x| x.as_object()).is_some() {
        let imp_p: ProfilesConfig = serde_json::from_value(val).map_err(|e| e.to_string())?;
        let count = imp_p.profiles.len();
        if replace {
            ConfigManager::save_profiles(&imp_p)?;
        } else {
            let mut cur = ConfigManager::load_profiles()?;
            for (k, v) in imp_p.profiles {
                cur.profiles.insert(k, v);
            }
            ConfigManager::save_profiles(&cur)?;
        }
        return Ok(ImportSummary {
            detected_format: "claude_profiles_json".to_string(),
            mode: effective_mode,
            claude_profiles: count,
            failover_chain: 0,
            antigravity_accounts: 0,
            antigravity_sessions: 0,
            usage_days_merged: 0,
            safety_snapshot: safety,
        });
    }

    Err("Không nhận diện được định dạng file backup (hỗ trợ Full Backup, profiles.json, hoặc antigravity.json)".to_string())
}

fn merge_usage_file(path: &Path, incoming: UsageStore, replace: bool) -> Result<usize, String> {
    let incoming_days = incoming.days.len();
    let final_store = if replace || !path.exists() {
        incoming
    } else {
        let mut cur: UsageStore = fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        for (day, models) in incoming.days {
            let slot = cur.days.entry(day).or_default();
            for (m, t) in models {
                let e = slot.entry(m).or_default();
                e.input = e.input.max(t.input);
                e.output = e.output.max(t.output);
                e.cache_read = e.cache_read.max(t.cache_read);
                e.cache_write = e.cache_write.max(t.cache_write);
                e.messages = e.messages.max(t.messages);
            }
        }
        cur
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(path, serde_json::to_string(&final_store).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(incoming_days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::antigravity::Snapshot;
    use crate::config::{Profile, TEST_HOME};
    use std::collections::{BTreeMap, HashMap};

    fn with_tmp_home<F: FnOnce(&Path)>(name: &str, f: F) {
        let dir = std::env::temp_dir().join(format!("cgs-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TEST_HOME.with(|h| *h.borrow_mut() = Some(dir.clone()));
        f(&dir);
        TEST_HOME.with(|h| *h.borrow_mut() = None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_sanitized_and_import_merge_preserves_tokens() {
        with_tmp_home("sanitized", |dir| {
            let mut env = HashMap::new();
            env.insert("ANTHROPIC_BASE_URL".into(), "https://gw.example.com".into());
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), "sk-live-secret".into());
            let mut profiles = HashMap::new();
            profiles.insert("gw1".into(), Profile { name: "GW 1".into(), env, model_prefix: "cc/".into() });
            ConfigManager::save_profiles(&ProfilesConfig { active: "gw1".into(), profiles }).unwrap();

            let mut fo = failover::load();
            fo.chain = vec!["gw1".into()];
            let orig_secret = fo.secret.clone();
            failover::save(&fo).unwrap();

            AntigravityManager::save_account(None, "pro-1", "Pro 01", "a@gmail.com", "primary").unwrap();
            let mut vault = Vault::default();
            let mut snap = Snapshot::default();
            snap.emails.insert("Antigravity".into(), "a@gmail.com".into());
            snap.keychain = Some("dG9rZW4=".into());
            vault.accounts.insert("pro-1".into(), snap);
            AntigravityManager::save_vault(&vault).unwrap();

            // 1. Export without secrets
            let out_no_sec = dir.join("backup-nosec.json");
            let meta = export_to_file(&out_no_sec, false).unwrap();
            assert_eq!(meta["claudeProfiles"], 1);
            assert_eq!(meta["antigravityAccounts"], 1);
            assert_eq!(meta["antigravitySessions"], 0);
            let raw = fs::read_to_string(&out_no_sec).unwrap();
            assert!(!raw.contains("sk-live-secret"));
            assert!(!raw.contains("dG9rZW4="));
            assert!(!raw.contains(&orig_secret));

            // 2. Import merge back: must preserve existing API token and failover secret!
            let summary = import_from_file(&out_no_sec, "merge").unwrap();
            assert_eq!(summary.detected_format, "full_backup");
            assert!(!summary.safety_snapshot.is_empty());
            let loaded_p = ConfigManager::load_profiles().unwrap();
            assert_eq!(
                loaded_p.profiles["gw1"].env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
                Some("sk-live-secret")
            );
            assert_eq!(failover::load().secret, orig_secret);

            // 3. Export with secrets and restore in replace mode
            let out_sec = dir.join("backup-sec.json");
            export_to_file(&out_sec, true).unwrap();
            AntigravityManager::remove_account("pro-1").unwrap();
            assert!(AntigravityManager::load_list().unwrap().accounts.is_empty());

            let sum2 = import_from_file(&out_sec, "replace").unwrap();
            assert_eq!(sum2.antigravity_accounts, 1);
            assert_eq!(sum2.antigravity_sessions, 1);
            assert!(AntigravityManager::load_vault().accounts.contains_key("pro-1"));

            // 4. Snapshots listed and deletable
            let snaps = list_local_snapshots();
            assert!(!snaps.is_empty());
            delete_local_snapshot(&snaps[0].filename).unwrap();
        });
    }

    #[test]
    fn auto_detects_plain_antigravity_and_profiles_json() {
        with_tmp_home("autodetect", |dir| {
            let ag_file = dir.join("ag.json");
            fs::write(
                &ag_file,
                r#"{"active":"","accounts":[{"key":"p1","label":"P1","email":"p1@x.com"}]}"#,
            )
            .unwrap();
            let s1 = import_from_file(&ag_file, "merge").unwrap();
            assert_eq!(s1.detected_format, "antigravity_json");
            assert_eq!(s1.antigravity_accounts, 1);

            let p_file = dir.join("p.json");
            fs::write(
                &p_file,
                r#"{"active":"","profiles":{"k1":{"name":"K1","env":{"ANTHROPIC_BASE_URL":"https://x.io"}}}}"#,
            )
            .unwrap();
            let s2 = import_from_file(&p_file, "merge").unwrap();
            assert_eq!(s2.detected_format, "claude_profiles_json");
            assert_eq!(s2.claude_profiles, 1);

            let mut days = BTreeMap::new();
            let mut m = BTreeMap::new();
            m.insert("claude-sonnet-4-6".to_string(), crate::usage::Tokens { input: 100, output: 50, ..Default::default() });
            days.insert("2026-11-24".to_string(), m);
            let merged = merge_usage_file(&dir.join("u.json"), UsageStore { days }, false).unwrap();
            assert_eq!(merged, 1);
        });
    }
}
