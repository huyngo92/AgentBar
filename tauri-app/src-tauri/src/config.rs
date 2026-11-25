use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use directories::ProjectDirs;
use crate::failover::{self, FAILOVER_KEY};

/// Reserved key for the machine's own Claude Code login (no gateway).
pub const NATIVE_KEY: &str = "__native__";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub env: HashMap<String, String>,
    /// Vendor/gateway prefix applied to bare `claude-*` model names (e.g. "cc/",
    /// "global.anthropic."). Only remembered so the editor can show it again.
    #[serde(default, rename = "modelPrefix", skip_serializing_if = "String::is_empty")]
    pub model_prefix: String,
}

/// Env vars a profile may set besides ANTHROPIC_* (Claude Code model routing).
const EXTRA_MANAGED: &[&str] = &["CLAUDE_CODE_SUBAGENT_MODEL"];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProfilesConfig {
    pub active: String,
    pub profiles: HashMap<String, Profile>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClaudeSettings {
    #[serde(flatten)]
    pub other: HashMap<String, serde_json::Value>,
    pub env: Option<HashMap<String, String>>,
}

/// Snapshot of the ANTHROPIC_* env that existed while on the native login,
/// taken right before the first switch to a provider. Shared with the CLI.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NativeBackup {
    pub env: HashMap<String, String>,
    #[serde(rename = "savedAt")]
    pub saved_at: u64,
}

/// UI & sync preferences inspired by CodexBar.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AppPreferences {
    #[serde(default = "default_language")]
    pub language: String, // "en" | "vi"
    #[serde(default = "default_theme")]
    pub theme: String, // "system" | "dark" | "light"
    #[serde(default = "default_percent_style")]
    pub percent_style: String, // "left" | "used"
    #[serde(default = "default_privacy_mode")]
    pub privacy_mode: bool,
    #[serde(default)]
    pub profiles_sync_path: String,
    #[serde(default)]
    pub antigravity_sync_path: String,
}

fn default_language() -> String { "en".to_string() }
fn default_theme() -> String { "system".to_string() }
fn default_percent_style() -> String { "left".to_string() }
fn default_privacy_mode() -> bool { true }

impl Default for AppPreferences {
    fn default() -> Self {
        Self {
            language: default_language(),
            theme: default_theme(),
            percent_style: default_percent_style(),
            privacy_mode: default_privacy_mode(),
            profiles_sync_path: String::new(),
            antigravity_sync_path: String::new(),
        }
    }
}

pub struct ConfigManager;

#[cfg(test)]
thread_local! {
    /// Tests point home/app dir here instead of touching the real ~/.claude.
    pub static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

impl ConfigManager {
    pub fn get_app_dir() -> PathBuf {
        #[cfg(test)]
        if let Some(h) = TEST_HOME.with(|h| h.borrow().clone()) {
            return h.join("app");
        }
        ProjectDirs::from("com", "claude-gateway", "switcher")
            .map(|pd| pd.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("./config"))
    }

    pub fn home() -> Result<PathBuf, String> {
        #[cfg(test)]
        if let Some(h) = TEST_HOME.with(|h| h.borrow().clone()) {
            return Ok(h);
        }
        directories::UserDirs::new()
            .map(|ud| ud.home_dir().to_path_buf())
            .ok_or_else(|| "Could not find home directory".to_string())
    }

    /// Discover workspace root when running from repo or installed alongside repo files.
    pub fn discover_workspace_file(filename: &str) -> Option<PathBuf> {
        #[cfg(test)]
        if TEST_HOME.with(|h| h.borrow().is_some()) {
            return None;
        }
        static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<PathBuf>>>> =
            std::sync::OnceLock::new();
        let map = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        if let Ok(guard) = map.lock() {
            if let Some(cached) = guard.get(filename) {
                if cached.as_ref().map_or(true, |p| p.is_file()) {
                    return cached.clone();
                }
            }
        }
        let mut candidates = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(filename));
            if let Some(parent) = cwd.parent() {
                candidates.push(parent.join(filename));
                if let Some(gp) = parent.parent() {
                    candidates.push(gp.join(filename));
                }
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            let mut cur = exe.parent();
            for _ in 0..7 {
                if let Some(dir) = cur {
                    candidates.push(dir.join(filename));
                    cur = dir.parent();
                }
            }
        }
        if let Some(manifest_dir) = option_env!("CARGO_MANIFEST_DIR") {
            let m = Path::new(manifest_dir);
            if let Some(repo_root) = m.parent().and_then(|p| p.parent()) {
                candidates.push(repo_root.join(filename));
            }
        }
        if let Ok(home) = Self::home() {
            candidates.push(home.join("Documents/claude-gateway-switcher").join(filename));
        }
        let found = candidates.into_iter().find(|p| p.is_file());
        if let Ok(mut guard) = map.lock() {
            guard.insert(filename.to_string(), found.clone());
        }
        found
    }

    pub fn get_preferences_path() -> PathBuf {
        Self::get_app_dir().join("preferences.json")
    }

    pub fn load_preferences() -> AppPreferences {
        let path = Self::get_preferences_path();
        let mut prefs: AppPreferences = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if prefs.profiles_sync_path.is_empty() {
            if let Some(p) = Self::discover_workspace_file("profiles.json") {
                prefs.profiles_sync_path = p.to_string_lossy().to_string();
            }
        }
        if prefs.antigravity_sync_path.is_empty() {
            if let Some(p) = Self::discover_workspace_file("antigravity.json") {
                prefs.antigravity_sync_path = p.to_string_lossy().to_string();
            }
        }
        prefs
    }

    pub fn save_preferences(prefs: &AppPreferences) -> Result<(), String> {
        let path = Self::get_preferences_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(prefs).map_err(|e| e.to_string())?;
        fs::write(path, json).map_err(|e| e.to_string())
    }

    pub fn get_profiles_path() -> PathBuf {
        Self::get_app_dir().join("profiles.json")
    }

    pub fn resolved_profiles_sync_path() -> Option<PathBuf> {
        #[cfg(test)]
        if TEST_HOME.with(|h| h.borrow().is_some()) {
            return None;
        }
        let prefs = Self::load_preferences();
        if !prefs.profiles_sync_path.trim().is_empty() {
            return Some(PathBuf::from(prefs.profiles_sync_path.trim()));
        }
        Self::discover_workspace_file("profiles.json")
    }

    pub fn get_claude_settings_path() -> Result<PathBuf, String> {
        Ok(Self::home()?.join(".claude").join("settings.json"))
    }

    pub fn get_native_backup_path() -> Result<PathBuf, String> {
        Ok(Self::home()?.join(".claude").join("gateway-switcher-native-backup.json"))
    }

    /// Two-way load: picks whichever of app_dir/profiles.json and workspace profiles.json
    /// was modified most recently (or falls back to whichever exists).
    pub fn load_profiles() -> Result<ProfilesConfig, String> {
        let app_path = Self::get_profiles_path();
        let sync_path = Self::resolved_profiles_sync_path();

        let chosen = match (&sync_path, app_path.exists()) {
            (Some(sp), true) if sp.exists() && sp != &app_path => {
                let app_m = fs::metadata(&app_path).and_then(|m| m.modified()).ok();
                let sp_m = fs::metadata(sp).and_then(|m| m.modified()).ok();
                if sp_m > app_m { sp.clone() } else { app_path.clone() }
            }
            (Some(sp), false) if sp.exists() => sp.clone(),
            (_, true) => app_path.clone(),
            _ => return Ok(ProfilesConfig { active: String::new(), profiles: HashMap::new() }),
        };

        let content = fs::read_to_string(&chosen).map_err(|e| e.to_string())?;
        let cfg: ProfilesConfig = serde_json::from_str(&content).map_err(|e| e.to_string())?;
        // Mirror to app_path if loaded from external sync file for the first time
        if chosen != app_path && !app_path.exists() {
            let _ = Self::write_profiles_file(&app_path, &cfg);
        }
        Ok(cfg)
    }

    fn write_profiles_file(path: &Path, config: &ProfilesConfig) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let content = serde_json::to_string_pretty(config).map_err(|e| e.to_string())? + "\n";
        fs::write(path, content).map_err(|e| e.to_string())
    }

    pub fn save_profiles(config: &ProfilesConfig) -> Result<(), String> {
        let app_path = Self::get_profiles_path();
        Self::write_profiles_file(&app_path, config)?;
        if let Some(sp) = Self::resolved_profiles_sync_path() {
            if sp != app_path {
                let _ = Self::write_profiles_file(&sp, config);
            }
        }
        Ok(())
    }

    fn load_settings() -> Result<ClaudeSettings, String> {
        let path = Self::get_claude_settings_path()?;
        if !path.exists() {
            return Ok(ClaudeSettings { other: HashMap::new(), env: Some(HashMap::new()) });
        }
        let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str(&content).map_err(|e| e.to_string())
    }

    fn save_settings(settings: &ClaudeSettings) -> Result<(), String> {
        let path = Self::get_claude_settings_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
        fs::write(path, json).map_err(|e| e.to_string())
    }

    pub fn write_private(path: &Path, content: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(path, content).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Keys the switcher owns in settings.env: ANTHROPIC_*, known Claude Code model
    /// vars, and anything any profile sets. Switching replaces exactly these.
    fn managed_keys(cfg: &ProfilesConfig) -> HashSet<String> {
        let mut keys: HashSet<String> = EXTRA_MANAGED.iter().map(|k| k.to_string()).collect();
        for p in cfg.profiles.values() {
            keys.extend(p.env.keys().cloned());
        }
        keys
    }

    fn is_managed(k: &str, extra: &HashSet<String>) -> bool {
        k.starts_with("ANTHROPIC_") || extra.contains(k)
    }

    /// Native login is active unless the `active` profile's gateway/credential
    /// vars are actually what ~/.claude/settings.json contains.
    pub fn is_native_active(cfg: &ProfilesConfig, env: &HashMap<String, String>) -> bool {
        if cfg.active == FAILOVER_KEY {
            let s = failover::load();
            return env.get("ANTHROPIC_BASE_URL") != Some(&s.url())
                || env.get("ANTHROPIC_AUTH_TOKEN") != Some(&s.secret);
        }
        let Some(p) = cfg.profiles.get(&cfg.active) else { return true };
        ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
            .iter()
            .any(|k| p.env.get(*k) != env.get(*k))
    }

    /// Like `load_profiles`, but `active` is cleared when settings.json is on the native login.
    pub fn load_effective_profiles() -> Result<ProfilesConfig, String> {
        let mut cfg = Self::load_profiles()?;
        let env = Self::load_settings()?.env.unwrap_or_default();
        if Self::is_native_active(&cfg, &env) {
            cfg.active = String::new();
        }
        Ok(cfg)
    }

    pub fn apply_profile(profile_key: &str) -> Result<(), String> {
        if profile_key == NATIVE_KEY {
            return Self::restore_native();
        }
        let mut profiles = Self::load_profiles()?;
        let new_env = if profile_key == FAILOVER_KEY {
            failover::claude_env(&failover::load())?
        } else {
            profiles.profiles.get(profile_key)
                .ok_or_else(|| format!("Profile {} not found", profile_key))?
                .env
                .clone()
        };

        let mut settings = Self::load_settings()?;
        let mut current_env = settings.env.take().unwrap_or_default();

        let managed = Self::managed_keys(&profiles);

        // Leaving the native login: snapshot its managed env once so it can be restored.
        let backup_path = Self::get_native_backup_path()?;
        if !backup_path.exists() {
            let env = if Self::is_native_active(&profiles, &current_env) {
                current_env.iter()
                    .filter(|(k, _)| Self::is_managed(k, &managed))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            } else {
                HashMap::new()
            };
            let saved_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let json = serde_json::to_string_pretty(&NativeBackup { env, saved_at }).map_err(|e| e.to_string())?;
            Self::write_private(&backup_path, &json)?;
        }

        // Keep the user's own vars, replace everything the switcher manages
        current_env.retain(|k, _| !Self::is_managed(k, &managed));
        current_env.extend(new_env);
        settings.env = Some(current_env);
        Self::save_settings(&settings)?;

        profiles.active = profile_key.to_string();
        Self::save_profiles(&profiles)?;
        Ok(())
    }

    /// Switch back to the machine's own Claude Code login: drop provider env,
    /// put back whatever ANTHROPIC_* the native setup had, and clear the snapshot.
    pub fn restore_native() -> Result<(), String> {
        let managed = Self::managed_keys(&Self::load_profiles()?);
        let mut settings = Self::load_settings()?;
        let mut env = settings.env.take().unwrap_or_default();
        env.retain(|k, _| !Self::is_managed(k, &managed));

        let backup_path = Self::get_native_backup_path()?;
        if backup_path.exists() {
            let content = fs::read_to_string(&backup_path).map_err(|e| e.to_string())?;
            let backup: NativeBackup = serde_json::from_str(&content).map_err(|e| e.to_string())?;
            env.extend(backup.env);
        }
        settings.env = Some(env);
        Self::save_settings(&settings)?;

        if backup_path.exists() {
            fs::remove_file(&backup_path).map_err(|e| e.to_string())?;
        }

        let mut profiles = Self::load_profiles()?;
        if !profiles.active.is_empty() {
            profiles.active = String::new();
            Self::save_profiles(&profiles)?;
        }
        Ok(())
    }

    /// Add or edit a profile. `old_key` renames; if the profile is the one in use,
    /// settings.json is re-applied so the change takes effect right away.
    pub fn save_profile(old_key: Option<String>, key: &str, profile: Profile) -> Result<(), String> {
        let key = key.trim();
        if key.is_empty() || key == NATIVE_KEY || key == FAILOVER_KEY || key == "native" {
            return Err(format!("Key \"{key}\" không hợp lệ"));
        }
        if profile.env.get("ANTHROPIC_BASE_URL").map_or(true, |u| u.trim().is_empty()) {
            return Err("Thiếu Base URL".into());
        }
        let was_active = {
            let eff = Self::load_effective_profiles()?;
            !eff.active.is_empty() && Some(&eff.active) == old_key.as_ref()
        };
        let mut cfg = Self::load_profiles()?;
        let previous = old_key.as_deref().and_then(|o| cfg.profiles.get(o).cloned());
        if let Some(old) = old_key.as_deref().filter(|o| *o != key) {
            if cfg.profiles.contains_key(key) {
                return Err(format!("Key \"{key}\" đã tồn tại"));
            }
            cfg.profiles.remove(old);
            if cfg.active == old {
                cfg.active = key.to_string();
            }
        }
        let dropped: Vec<String> = previous
            .map(|p| p.env.into_keys().filter(|k| !profile.env.contains_key(k)).collect())
            .unwrap_or_default();
        cfg.profiles.insert(key.to_string(), profile);
        Self::save_profiles(&cfg)?;

        if was_active {
            if !dropped.is_empty() {
                let mut settings = Self::load_settings()?;
                if let Some(env) = settings.env.as_mut() {
                    env.retain(|k, _| !dropped.contains(k));
                }
                Self::save_settings(&settings)?;
            }
            Self::apply_profile(key)?;
        }
        Self::reapply_failover()?;
        Ok(())
    }

    /// In failover mode settings.json mirrors the chain's first profile; rebuild it
    /// after profiles or failover settings change.
    pub fn reapply_failover() -> Result<(), String> {
        if Self::load_effective_profiles()?.active == FAILOVER_KEY {
            Self::apply_profile(FAILOVER_KEY)?;
        }
        Ok(())
    }

    /// Who Claude Code is logged in as (from ~/.claude.json), plus switch state.
    pub fn native_info() -> Result<serde_json::Value, String> {
        let cfg = Self::load_effective_profiles()?;
        let account = fs::read_to_string(Self::home()?.join(".claude.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("oauthAccount").cloned())
            .filter(|v| v.is_object());
        let field = |k: &str| account.as_ref()
            .and_then(|a| a.get(k))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(serde_json::json!({
            "loggedIn": account.is_some(),
            "email": field("emailAddress"),
            "displayName": field("displayName"),
            "organization": field("organizationName"),
            "billingType": field("billingType"),
            "active": cfg.active.is_empty(),
            "hasBackup": Self::get_native_backup_path()?.exists(),
            "syncPath": Self::resolved_profiles_sync_path().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        }))
    }

    /// Short label for the tray/menu, e.g. "Claude Login (me@x.com)".
    #[allow(dead_code)]
    pub fn native_label() -> String {
        let email = Self::native_info().ok()
            .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_default();
        if email.is_empty() { "Claude Login".to_string() } else { format!("Claude Login ({email})") }
    }

    /// Delete a single profile (if currently active, restores native login first).
    pub fn remove_profile(key: &str) -> Result<(), String> {
        let eff = Self::load_effective_profiles()?;
        if eff.active == key {
            Self::restore_native()?;
        }
        let mut cfg = Self::load_profiles()?;
        cfg.profiles
            .remove(key)
            .ok_or_else(|| format!("Profile \"{key}\" không tồn tại"))?;
        if cfg.active == key {
            cfg.active.clear();
        }
        Self::save_profiles(&cfg)?;
        let mut fo = failover::load();
        if fo.chain.iter().any(|k| k == key) {
            fo.chain.retain(|k| k != key);
            let _ = failover::save(&fo);
        }
        Self::reapply_failover()?;
        Ok(())
    }

    /// Restore the native login, then delete profiles.json.
    pub fn remove_all() -> Result<(), String> {
        Self::restore_native()?;
        let profiles_path = Self::get_profiles_path();
        if profiles_path.exists() {
            fs::remove_file(profiles_path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn import_legacy_profiles(legacy_path: &Path) -> Result<(), String> {
        let content = fs::read_to_string(legacy_path).map_err(|e| e.to_string())?;
        let config: ProfilesConfig = serde_json::from_str(&content).map_err(|e| e.to_string())?;
        Self::save_profiles(&config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_round_trip() {
        let home = std::env::temp_dir().join(format!("cgs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".claude")).unwrap();
        TEST_HOME.with(|h| *h.borrow_mut() = Some(home.clone()));

        fs::write(home.join(".claude/settings.json"),
            r#"{"model":"opus","env":{"FOO":"1","ANTHROPIC_MODEL":"native"}}"#).unwrap();
        fs::write(home.join(".claude.json"), r#"{"oauthAccount":{"emailAddress":"a@b.c"}}"#).unwrap();
        let env = |u: &str| HashMap::from([
            ("ANTHROPIC_BASE_URL".to_string(), u.to_string()),
            ("ANTHROPIC_AUTH_TOKEN".to_string(), "t".to_string()),
        ]);
        ConfigManager::save_profiles(&ProfilesConfig {
            active: "a".into(),
            profiles: HashMap::from([
                ("a".to_string(), Profile { name: "A".into(), env: env("https://a"), model_prefix: String::new() }),
                ("b".to_string(), Profile { name: "B".into(), env: env("https://b"), model_prefix: String::new() }),
            ]),
        }).unwrap();

        assert_eq!(ConfigManager::native_info().unwrap()["active"], true);
        ConfigManager::apply_profile("a").unwrap();
        ConfigManager::apply_profile("b").unwrap();
        let s = ConfigManager::load_settings().unwrap();
        let e = s.env.unwrap();
        assert_eq!(e["ANTHROPIC_BASE_URL"], "https://b");
        assert!(!e.contains_key("ANTHROPIC_MODEL"));
        assert_eq!(ConfigManager::native_info().unwrap()["active"], false);
        assert_eq!(ConfigManager::native_label(), "Claude Login (a@b.c)");

        ConfigManager::apply_profile(NATIVE_KEY).unwrap();
        let s = ConfigManager::load_settings().unwrap();
        let e = s.env.unwrap();
        assert_eq!(e.get("ANTHROPIC_MODEL").map(String::as_str), Some("native"));
        assert_eq!(e.get("FOO").map(String::as_str), Some("1"));
        assert!(!e.contains_key("ANTHROPIC_BASE_URL"));
        assert_eq!(s.other["model"], "opus");
        assert!(!ConfigManager::get_native_backup_path().unwrap().exists());
        assert_eq!(ConfigManager::load_effective_profiles().unwrap().active, "");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn edit_and_extra_vars() {
        let home = std::env::temp_dir().join(format!("cgs-test-edit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".claude")).unwrap();
        TEST_HOME.with(|h| *h.borrow_mut() = Some(home.clone()));
        fs::write(home.join(".claude/settings.json"), r#"{"env":{"MINE":"1"}}"#).unwrap();

        let prof = |u: &str, extra: &[(&str, &str)]| {
            let mut env = HashMap::from([
                ("ANTHROPIC_BASE_URL".to_string(), u.to_string()),
                ("ANTHROPIC_AUTH_TOKEN".to_string(), "t".to_string()),
            ]);
            env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
            Profile { name: u.into(), env, model_prefix: "cc/".into() }
        };
        let env_now = || ConfigManager::load_settings().unwrap().env.unwrap();

        ConfigManager::save_profile(None, "gw", prof("https://gw", &[
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", "cc/claude-opus-5-5"),
            ("CLAUDE_CODE_SUBAGENT_MODEL", "cc/claude-sonnet-5-5"),
        ])).unwrap();
        ConfigManager::save_profile(None, "plain", prof("https://plain", &[])).unwrap();
        assert!(ConfigManager::save_profile(None, "native", prof("https://x", &[])).is_err());

        ConfigManager::apply_profile("gw").unwrap();
        assert_eq!(env_now()["CLAUDE_CODE_SUBAGENT_MODEL"], "cc/claude-sonnet-5-5");

        ConfigManager::save_profile(Some("gw".into()), "gw2", prof("https://gw", &[
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", "cc/claude-fable-5-2"),
        ])).unwrap();
        let e = env_now();
        assert_eq!(e["ANTHROPIC_DEFAULT_OPUS_MODEL"], "cc/claude-fable-5-2");
        assert!(!e.contains_key("CLAUDE_CODE_SUBAGENT_MODEL"));
        assert_eq!(ConfigManager::load_effective_profiles().unwrap().active, "gw2");
        assert_eq!(ConfigManager::load_profiles().unwrap().profiles["gw2"].model_prefix, "cc/");

        ConfigManager::save_profile(Some("gw2".into()), "gw2", prof("https://gw", &[
            ("CLAUDE_CODE_SUBAGENT_MODEL", "x"),
        ])).unwrap();
        ConfigManager::apply_profile("plain").unwrap();
        let e = env_now();
        assert!(!e.contains_key("CLAUDE_CODE_SUBAGENT_MODEL"));
        assert!(!e.contains_key("ANTHROPIC_DEFAULT_OPUS_MODEL"));
        assert_eq!(e["MINE"], "1");

        ConfigManager::restore_native().unwrap();
        let e = env_now();
        assert_eq!(e.len(), 1);
        assert_eq!(e["MINE"], "1");
        let _ = fs::remove_dir_all(&home);
    }
}
