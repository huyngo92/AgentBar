//! Antigravity account switcher, fully separate from the Claude side.
//!
//! A saved account is a snapshot of the login state Antigravity itself keeps:
//! the auth rows in each desktop app's `state.vscdb`, Antigravity 2.x's keychain
//! item and `app_storage.json` login keys, plus the Gemini CLI files in `~/.gemini`. Switching closes Antigravity, writes the chosen snapshot
//! back and reopens it. No network calls, no OAuth client of our own, and
//! nothing under `~/.claude` is read or written.

use crate::config::ConfigManager;
use base64::Engine as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Desktop apps sharing the login format (bundle name == Application Support dir).
pub const APPS: &[&str] = &["Antigravity", "Antigravity IDE"];
/// Rows in `User/globalStorage/state.vscdb` that make up the signed-in account.
const AUTH_KEYS: &[&str] = &[
    "antigravityAuthStatus",
    "antigravityUnifiedStateSync.oauthToken",
    "antigravityUnifiedStateSync.userStatus",
    "antigravityUnifiedStateSync.modelCredits",
    "antigravity.profileUrl",
];
/// Gemini CLI login files in `~/.gemini`.
const GEMINI_FILES: &[&str] = &["oauth_creds.json", "google_accounts.json"];
/// Antigravity 2.x keeps its sign-in in the login keychain (service, account).
#[cfg_attr(test, allow(dead_code))]
const KEYCHAIN: (&str, &str) = ("gemini", "antigravity");
/// Antigravity 2.x `app_storage.json` keys naming the signed-in user.
const APP_STORAGE_KEYS: &[&str] = &["agy.onboarding.lastLoginUsername", "agy.onboarding.lastLoginIsGcpTos"];
/// Where each signed-in email was read from, in display/matching order.
pub const SOURCES: &[&str] = &["Antigravity", "Antigravity IDE", "Gemini CLI"];

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// Unix seconds when the login snapshot was last captured (0 = never).
    #[serde(default)]
    pub saved_at: u64,
}

/// `antigravity.json`: the account list (no secrets), mirrored to the workspace copy.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AccountList {
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

/// app -> (row key -> value, None = row absent); ~/.gemini file -> content.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    #[serde(default)]
    pub apps: BTreeMap<String, BTreeMap<String, Option<String>>>,
    #[serde(default)]
    pub files: BTreeMap<String, Option<String>>,
    /// app -> its `app_storage.json` login keys (None = key absent).
    #[serde(default)]
    pub app_storage: BTreeMap<String, BTreeMap<String, Option<serde_json::Value>>>,
    /// Keychain item, base64 (None = absent). Only meaningful when `keychain_captured`.
    #[serde(default)]
    pub keychain: Option<String>,
    #[serde(default)]
    pub keychain_captured: bool,
    /// Source (see `SOURCES`) -> signed-in email.
    #[serde(default)]
    pub emails: BTreeMap<String, String>,
}

impl Snapshot {
    pub fn is_logged_in(&self) -> bool {
        !self.emails.is_empty()
            || self.keychain.is_some()
            || self.apps.values().any(|rows| rows.get("antigravityUnifiedStateSync.oauthToken").map_or(false, |v| v.is_some()))
            || self.files.get("oauth_creds.json").map_or(false, |v| v.is_some())
    }

    /// The account to match saved entries by: Antigravity first, then the IDE, then the Gemini CLI.
    pub fn email(&self) -> String {
        SOURCES.iter().find_map(|s| self.emails.get(*s)).cloned().unwrap_or_default()
    }

    /// Work out each source's email from the captured state.
    fn detect_emails(&mut self) {
        let mut emails = BTreeMap::new();
        for app in APPS {
            let from_storage = self
                .app_storage
                .get(*app)
                .and_then(|m| m.get(APP_STORAGE_KEYS[0])?.as_ref()?.as_str().map(str::to_string));
            let rows = self.apps.get(*app);
            let row = |k: &str| rows.and_then(|r| r.get(k)?.clone());
            let from_status = || row("antigravityUnifiedStateSync.userStatus").and_then(|v| email_in_proto(&v));
            let from_auth = || {
                let v: serde_json::Value = serde_json::from_str(&row("antigravityAuthStatus")?).ok()?;
                v.get("email")?.as_str().map(str::to_string)
            };
            if let Some(e) = from_storage.or_else(from_status).or_else(from_auth).filter(|e| !e.trim().is_empty()) {
                emails.insert(app.to_string(), e.trim().to_string());
            }
        }
        let cli = self
            .files
            .get("google_accounts.json")
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw.as_ref()?).ok())
            .and_then(|v| v.get("active")?.as_str().map(str::to_string))
            .filter(|e| !e.trim().is_empty());
        if let Some(e) = cli {
            emails.insert("Gemini CLI".to_string(), e.trim().to_string());
        }
        self.emails = emails;
    }
}

// ---------- email inside Antigravity's protobuf records ----------

fn varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, byte) in b.iter().enumerate().take(10) {
        v |= ((byte & 0x7f) as u64) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

fn decode_b64(b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 16 || !b.iter().all(|c| c.is_ascii_alphanumeric() || b"+/-_=".contains(c)) {
        return None;
    }
    let t: Vec<u8> = b.iter().copied().filter(|c| *c != b'=').collect();
    let e = &base64::engine::general_purpose::STANDARD_NO_PAD;
    e.decode(&t).or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&t)).ok()
}

fn find_email(b: &[u8]) -> Option<String> {
    let local = |c: u8| c.is_ascii_alphanumeric() || b"._+-".contains(&c);
    let domain = |c: u8| c.is_ascii_alphanumeric() || b".-".contains(&c);
    for (at, _) in b.iter().enumerate().filter(|(_, c)| **c == b'@') {
        let start = (0..at).rev().take_while(|i| local(b[*i])).last()?;
        let end = (at + 1..b.len()).take_while(|i| domain(b[*i])).last()? + 1;
        let d = std::str::from_utf8(&b[at + 1..end]).ok()?.trim_end_matches('.');
        if d.contains('.') {
            return Some(format!("{}@{d}", std::str::from_utf8(&b[start..at]).ok()?));
        }
    }
    None
}

/// Walk protobuf fields (and base64 layers inside them) for the first email.
fn walk_proto(b: &[u8], depth: usize) -> Option<String> {
    if depth > 6 {
        return None;
    }
    let mut i = 0;
    while i < b.len() {
        let (key, n) = varint(&b[i..])?;
        i += n;
        match key & 7 {
            0 => i += varint(&b[i..])?.1,
            1 => i += 8,
            5 => i += 4,
            2 => {
                let (len, n) = varint(&b[i..])?;
                i += n;
                let v = b.get(i..i + len as usize)?;
                i += len as usize;
                let nested = decode_b64(v);
                if let Some(e) = nested.as_deref().and_then(find_email).or_else(|| find_email(v)) {
                    return Some(e);
                }
                if let Some(e) = walk_proto(v, depth + 1).or_else(|| walk_proto(nested.as_deref()?, depth + 1)) {
                    return Some(e);
                }
            }
            _ => return None,
        }
    }
    None
}

pub fn email_in_proto(value: &str) -> Option<String> {
    let outer = decode_b64(value.trim().as_bytes())?;
    walk_proto(&outer, 0)
}

// ---------- keychain (Antigravity 2.x) ----------

#[cfg(test)]
thread_local! {
    static FAKE_KEYCHAIN: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
}

fn keychain_read() -> Result<Option<Vec<u8>>, String> {
    #[cfg(test)]
    return Ok(FAKE_KEYCHAIN.with(|k| k.borrow().clone()));
    #[cfg(not(test))]
    match security_framework::passwords::get_generic_password(KEYCHAIN.0, KEYCHAIN.1) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == -25300 => Ok(None), // errSecItemNotFound
        Err(e) => Err(format!(
            "Không đọc được đăng nhập Antigravity trong Keychain ({e}). Khi macOS hỏi, hãy bấm \"Luôn cho phép\"."
        )),
    }
}

fn keychain_write(v: Option<&[u8]>) -> Result<(), String> {
    #[cfg(test)]
    {
        FAKE_KEYCHAIN.with(|k| *k.borrow_mut() = v.map(<[u8]>::to_vec));
        return Ok(());
    }
    #[cfg(not(test))]
    {
        use security_framework::passwords::{delete_generic_password, set_generic_password};
        match v {
            Some(v) => set_generic_password(KEYCHAIN.0, KEYCHAIN.1, v).map_err(|e| format!("Keychain: {e}")),
            None => match delete_generic_password(KEYCHAIN.0, KEYCHAIN.1) {
                Err(e) if e.code() != -25300 => Err(format!("Keychain: {e}")),
                _ => Ok(()),
            },
        }
    }
}

/// 0600 file holding the snapshots (tokens live only here).
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Vault {
    #[serde(default)]
    pub accounts: BTreeMap<String, Snapshot>,
    /// Login state before the first switch, for "Khôi phục ban đầu".
    #[serde(default)]
    pub original: Option<Snapshot>,
}

pub struct AntigravityManager;

impl AntigravityManager {
    fn list_path() -> PathBuf {
        ConfigManager::get_app_dir().join("antigravity.json")
    }

    fn vault_path() -> PathBuf {
        ConfigManager::get_app_dir().join("antigravity-sessions.json")
    }

    fn sync_path() -> Option<PathBuf> {
        #[cfg(test)]
        if crate::config::TEST_HOME.with(|h| h.borrow().is_some()) {
            return None;
        }
        let prefs = ConfigManager::load_preferences();
        Some(prefs.antigravity_sync_path.trim())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| ConfigManager::discover_workspace_file("antigravity.json"))
    }

    fn db_path(home: &Path, app: &str) -> PathBuf {
        home.join("Library/Application Support")
            .join(app)
            .join("User/globalStorage/state.vscdb")
    }

    fn app_storage_path(home: &Path, app: &str) -> PathBuf {
        home.join("Library/Application Support").join(app).join("app_storage.json")
    }

    fn installed(home: &Path, app: &str) -> bool {
        Self::db_path(home, app).is_file() || Self::app_storage_path(home, app).is_file()
    }

    // ---------- account list & vault ----------

    /// Newest of the app copy and the workspace copy wins (same rule as profiles.json).
    pub fn load_list() -> Result<AccountList, String> {
        let app_path = Self::list_path();
        let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
        let chosen = match Self::sync_path().filter(|p| p.is_file() && *p != app_path) {
            Some(sp) if !app_path.exists() || modified(&sp) > modified(&app_path) => sp,
            _ if app_path.exists() => app_path,
            _ => return Ok(AccountList::default()),
        };
        let raw = fs::read_to_string(&chosen).map_err(|e| e.to_string())?;
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", chosen.display()))
    }

    pub fn save_list(list: &AccountList) -> Result<(), String> {
        let json = serde_json::to_string_pretty(list).map_err(|e| e.to_string())? + "\n";
        let app_path = Self::list_path();
        if let Some(parent) = app_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(&app_path, &json).map_err(|e| e.to_string())?;
        if let Some(sp) = Self::sync_path().filter(|p| *p != app_path) {
            let _ = fs::write(sp, &json);
        }
        Ok(())
    }

    pub fn load_vault() -> Vault {
        fs::read_to_string(Self::vault_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save_vault(vault: &Vault) -> Result<(), String> {
        let json = serde_json::to_string(vault).map_err(|e| e.to_string())?;
        ConfigManager::write_private(&Self::vault_path(), &json)
    }

    // ---------- machine state ----------

    /// Read the login currently on this machine (works while the apps are open).
    /// `with_keychain` also reads the keychain item (macOS may ask for permission),
    /// so plain status refreshes leave it out.
    pub fn read_current(with_keychain: bool) -> Result<Snapshot, String> {
        let home = ConfigManager::home()?;
        let mut snap = Snapshot::default();
        for app in APPS {
            let db = Self::db_path(&home, app);
            if !db.is_file() {
                continue;
            }
            // Read-only open can fail while the app holds the file; immutable mode still reads it.
            let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|c| c.query_row("SELECT count(*) FROM ItemTable", [], |_| Ok(())).map(|_| c))
                .or_else(|_| {
                    Connection::open_with_flags(
                        format!("file:{}?immutable=1", db.display()),
                        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
                    )
                })
                .map_err(|e| format!("Không mở được dữ liệu {app}: {e}"))?;
            let mut rows = BTreeMap::new();
            for key in AUTH_KEYS {
                let v: Option<String> = conn
                    .query_row("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?1", [key], |r| r.get(0))
                    .optional()
                    .map_err(|e| format!("{app}: {e}"))?;
                rows.insert(key.to_string(), v);
            }
            snap.apps.insert(app.to_string(), rows);
        }
        for app in APPS {
            let Ok(raw) = fs::read_to_string(Self::app_storage_path(&home, app)) else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
            let keys = APP_STORAGE_KEYS.iter().map(|k| (k.to_string(), v.get(*k).cloned())).collect();
            snap.app_storage.insert(app.to_string(), keys);
        }
        let gemini = home.join(".gemini");
        for f in GEMINI_FILES {
            snap.files.insert(f.to_string(), fs::read_to_string(gemini.join(f)).ok());
        }
        if with_keychain {
            snap.keychain = keychain_read()?.map(|b| base64::engine::general_purpose::STANDARD.encode(b));
            snap.keychain_captured = true;
        }
        snap.detect_emails();
        Ok(snap)
    }

    /// Write a snapshot back. Apps it did not capture are left untouched.
    fn write_snapshot(snap: &Snapshot) -> Result<(), String> {
        let home = ConfigManager::home()?;
        for (app, rows) in &snap.apps {
            let db = Self::db_path(&home, app);
            if !db.is_file() {
                continue;
            }
            let mut conn = Connection::open(&db).map_err(|e| format!("Không mở được dữ liệu {app}: {e}"))?;
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            for (key, value) in rows {
                match value {
                    Some(v) => tx.execute("INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)", [key, v]),
                    None => tx.execute("DELETE FROM ItemTable WHERE key = ?1", [key]),
                }
                .map_err(|e| format!("{app}: {e}"))?;
            }
            tx.commit().map_err(|e| e.to_string())?;
        }
        for (app, keys) in &snap.app_storage {
            let path = Self::app_storage_path(&home, app);
            let Ok(raw) = fs::read_to_string(&path) else { continue };
            let mut v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| format!("{app}: {e}"))?;
            let Some(obj) = v.as_object_mut() else { continue };
            for (k, val) in keys {
                match val {
                    Some(val) => obj.insert(k.clone(), val.clone()),
                    None => obj.remove(k),
                };
            }
            fs::write(&path, serde_json::to_string(&v).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        }
        if snap.keychain_captured {
            let bytes = match &snap.keychain {
                Some(b) => Some(base64::engine::general_purpose::STANDARD.decode(b).map_err(|e| e.to_string())?),
                None => None,
            };
            keychain_write(bytes.as_deref())?;
        }
        let gemini = home.join(".gemini");
        for (name, content) in &snap.files {
            let path = gemini.join(name);
            match content {
                Some(c) => ConfigManager::write_private(&path, c)?,
                None => {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        Ok(())
    }

    /// Apps currently running (main process only).
    fn running_apps() -> Vec<String> {
        if cfg!(test) {
            return Vec::new();
        }
        let Ok(out) = std::process::Command::new("pgrep")
            .args(["-fl", "Antigravity"])
            .output()
        else {
            return Vec::new();
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        APPS.iter()
            .filter(|app| {
                let needle = format!("/{app}.app/Contents/MacOS/");
                stdout.lines().any(|line| line.contains(&needle))
            })
            .map(|a| a.to_string())
            .collect()
    }

    /// Ask the apps to quit normally and wait until they're gone, so they don't
    /// overwrite the login rows on exit.
    fn quit_apps(apps: &[String]) -> Result<(), String> {
        for app in apps {
            let _ = std::process::Command::new("osascript")
                .args(["-e", &format!("tell application \"{app}\" to quit")])
                .output();
        }
        for _ in 0..40 {
            if Self::running_apps().is_empty() {
                // Let the last writes to state.vscdb settle.
                std::thread::sleep(std::time::Duration::from_millis(500));
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        Err("Antigravity chưa đóng (có thể đang hỏi lưu file). Hãy đóng hẳn Antigravity rồi thử lại.".into())
    }

    pub fn open_app(app: &str) -> Result<(), String> {
        if cfg!(test) {
            return Ok(());
        }
        std::process::Command::new("open")
            .env_remove("ELECTRON_RUN_AS_NODE")
            .args(["-a", app])
            .status()
            .map_err(|e| e.to_string())
            .and_then(|s| if s.success() { Ok(()) } else { Err(format!("Không mở được {app}")) })
    }

    // ---------- operations ----------

    /// Account list, which saved account (if any) is signed in now, and app state.
    pub fn status() -> Result<serde_json::Value, String> {
        Self::status_opts(true)
    }

    /// Lightweight status without spawning `pgrep` when process state isn't needed (tray / popover).
    pub fn status_opts(include_running: bool) -> Result<serde_json::Value, String> {
        let list = Self::load_list()?;
        let vault = Self::load_vault();
        let current = Self::read_current(false)?;
        let email = current.email();
        let logged_in = current.is_logged_in();
        let active = Self::effective_active(&list, &email, logged_in);
        let running = if include_running {
            Self::running_apps()
        } else {
            Vec::new()
        };
        let home = ConfigManager::home()?;
        let apps: Vec<_> = SOURCES
            .iter()
            .map(|a| {
                let is_app = APPS.contains(a);
                serde_json::json!({
                    "name": a,
                    "installed": if is_app { Self::installed(&home, a) } else { current.emails.contains_key(*a) },
                    "running": running.iter().any(|r| r == a),
                    "email": current.emails.get(*a).cloned().unwrap_or_default(),
                    "isApp": is_app,
                })
            })
            .collect();
        let accounts: Vec<_> = list
            .accounts
            .iter()
            .map(|a| {
                let mut v = serde_json::to_value(a).unwrap_or_default();
                v["hasLogin"] = vault.accounts.contains_key(&a.key).into();
                v
            })
            .collect();
        Ok(serde_json::json!({
            "loggedIn": logged_in,
            "email": email,
            "active": active,
            "accounts": accounts,
            "apps": apps,
            "hasOriginal": vault.original.is_some(),
        }))
    }

    /// The saved account matching the machine's login; empty if it isn't one of them.
    fn effective_active(list: &AccountList, email: &str, logged_in: bool) -> String {
        if !logged_in || email.is_empty() {
            return String::new();
        }
        let matches = |a: &&Account| a.email.eq_ignore_ascii_case(email);
        list.accounts
            .iter()
            .filter(matches)
            .find(|a| a.key == list.active)
            .or_else(|| list.accounts.iter().find(matches))
            .map(|a| a.key.clone())
            .unwrap_or_default()
    }

    /// Save the machine's current login into `key`, or into the account with the
    /// same email, or into a new account named after the email.
    pub fn capture_current(key: Option<String>) -> Result<Account, String> {
        let snap = Self::read_current(true)?;
        if !snap.is_logged_in() {
            return Err("Antigravity chưa đăng nhập tài khoản nào. Hãy đăng nhập trong Antigravity trước.".into());
        }
        let email = snap.email();
        let mut list = Self::load_list()?;
        let pos = match key.as_deref().filter(|k| !k.is_empty()) {
            Some(k) => Some(
                list.accounts.iter().position(|a| a.key == k).ok_or_else(|| format!("Không tìm thấy \"{k}\""))?,
            ),
            None => list.accounts.iter().position(|a| !email.is_empty() && a.email.eq_ignore_ascii_case(&email)),
        };
        let pos = match pos {
            Some(p) => {
                let acc = &list.accounts[p];
                if !acc.email.is_empty() && !email.is_empty() && !acc.email.eq_ignore_ascii_case(&email) {
                    return Err(format!(
                        "Antigravity đang đăng nhập {email}, không phải {} của \"{}\".",
                        acc.email, acc.label
                    ));
                }
                p
            }
            None => {
                let base: String = email
                    .split('@')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("account")
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
                    .collect();
                let mut k = base.clone();
                let mut n = 2;
                while list.accounts.iter().any(|a| a.key == k) {
                    k = format!("{base}-{n}");
                    n += 1;
                }
                let label = if email.is_empty() { k.clone() } else { email.clone() };
                list.accounts.push(Account { key: k, label, ..Default::default() });
                list.accounts.len() - 1
            }
        };
        let acc = &mut list.accounts[pos];
        if !email.is_empty() {
            acc.email = email;
        }
        acc.saved_at = now_secs();
        let saved = acc.clone();
        list.active = saved.key.clone();

        let mut vault = Self::load_vault();
        vault.accounts.insert(saved.key.clone(), snap);
        Self::save_vault(&vault)?;
        Self::save_list(&list)?;
        Ok(saved)
    }

    /// Add or edit an account's name/notes (`old_key` = None adds an empty slot).
    pub fn save_account(old_key: Option<String>, key: &str, label: &str, email: &str, notes: &str) -> Result<(), String> {
        let key = key.trim();
        if key.is_empty() {
            return Err("Key không được để trống".into());
        }
        let mut list = Self::load_list()?;
        if old_key.as_deref() != Some(key) && list.accounts.iter().any(|a| a.key == key) {
            return Err(format!("Key \"{key}\" đã tồn tại"));
        }
        let label = if label.trim().is_empty() { key } else { label.trim() };
        match old_key.as_deref().and_then(|o| list.accounts.iter().position(|a| a.key == o)) {
            Some(p) => {
                let old = list.accounts[p].key.clone();
                if old != key {
                    let mut vault = Self::load_vault();
                    if let Some(s) = vault.accounts.remove(&old) {
                        vault.accounts.insert(key.to_string(), s);
                        Self::save_vault(&vault)?;
                    }
                    if list.active == old {
                        list.active = key.to_string();
                    }
                }
                let a = &mut list.accounts[p];
                a.key = key.to_string();
                a.label = label.to_string();
                a.notes = notes.trim().to_string();
                // Email comes from the saved login once there is one.
                if a.saved_at == 0 {
                    a.email = email.trim().to_string();
                }
            }
            None => list.accounts.push(Account {
                key: key.to_string(),
                label: label.to_string(),
                email: email.trim().to_string(),
                notes: notes.trim().to_string(),
                saved_at: 0,
            }),
        }
        Self::save_list(&list)
    }

    pub fn remove_account(key: &str) -> Result<(), String> {
        let mut list = Self::load_list()?;
        list.accounts.retain(|a| a.key != key);
        if list.active == key {
            list.active.clear();
        }
        let mut vault = Self::load_vault();
        vault.accounts.remove(key);
        Self::save_vault(&vault)?;
        Self::save_list(&list)
    }

    /// Switch the machine's Antigravity login to a saved account.
    pub fn switch(key: &str) -> Result<(), String> {
        let mut list = Self::load_list()?;
        let target = list
            .accounts
            .iter()
            .find(|a| a.key == key)
            .cloned()
            .ok_or_else(|| format!("Không tìm thấy \"{key}\""))?;
        let mut vault = Self::load_vault();
        let snap = vault
            .accounts
            .get(key)
            .cloned()
            .ok_or_else(|| format!("\"{}\" chưa lưu đăng nhập. Đăng nhập tài khoản này trong Antigravity rồi bấm \"Lưu đăng nhập hiện tại\".", target.label))?;

        let running = Self::running_apps();
        Self::quit_apps(&running)?;
        Self::keep_outgoing(&mut list, &mut vault)?;
        Self::write_snapshot(&snap)?;
        list.active = key.to_string();
        Self::save_list(&list)?;

        for app in &running {
            Self::open_app(app)?;
        }
        Ok(())
    }

    /// Before replacing the machine's login (apps already closed): store the outgoing
    /// account's possibly-refreshed tokens, and remember the very first state.
    fn keep_outgoing(list: &mut AccountList, vault: &mut Vault) -> Result<Snapshot, String> {
        let current = Self::read_current(true)?;
        let email = current.email();
        if current.is_logged_in() && !email.is_empty() {
            if let Some(acc) = list.accounts.iter_mut().find(|a| a.email.eq_ignore_ascii_case(&email)) {
                acc.saved_at = now_secs();
                vault.accounts.insert(acc.key.clone(), current.clone());
            }
        }
        if vault.original.is_none() {
            vault.original = Some(current.clone());
        }
        Self::save_vault(vault)?;
        Ok(current)
    }

    /// The same keys as `current`, all absent: writing it signs every app out.
    fn signed_out(current: &Snapshot) -> Snapshot {
        let blank = |m: &BTreeMap<String, Option<String>>| m.keys().map(|k| (k.clone(), None)).collect();
        Snapshot {
            apps: current.apps.iter().map(|(a, rows)| (a.clone(), blank(rows))).collect(),
            files: blank(&current.files),
            app_storage: current
                .app_storage
                .iter()
                .map(|(a, m)| (a.clone(), m.keys().map(|k| (k.clone(), None)).collect()))
                .collect(),
            keychain: None,
            keychain_captured: true,
            emails: BTreeMap::new(),
        }
    }

    /// Sign this machine out (Antigravity, IDE, Gemini CLI) without revoking anything
    /// on Google's side, so saved accounts keep working. Returns the apps it closed.
    fn clear_local_closed() -> Result<Vec<String>, String> {
        let running = Self::running_apps();
        Self::quit_apps(&running)?;
        let mut list = Self::load_list()?;
        let mut vault = Self::load_vault();
        let current = Self::keep_outgoing(&mut list, &mut vault)?;
        Self::write_snapshot(&Self::signed_out(&current))?;
        list.active.clear();
        Self::save_list(&list)?;
        Ok(running)
    }

    /// "Clear local login": sign out locally, reopen whatever was open.
    pub fn clear_local() -> Result<(), String> {
        for app in Self::clear_local_closed()? {
            Self::open_app(&app)?;
        }
        Ok(())
    }

    /// "Thêm tài khoản khác": save the current login, sign out locally, open
    /// Antigravity on its sign-in screen. Returns the account just saved, if any.
    pub fn add_another() -> Result<Option<Account>, String> {
        let saved = if Self::read_current(false)?.is_logged_in() { Some(Self::capture_current(None)?) } else { None };
        Self::clear_local_closed()?;
        Self::open_app(APPS[0])?;
        Ok(saved)
    }

    /// "Chuyển đổi": switch to the saved account after the current one (wrapping).
    pub fn switch_next() -> Result<Account, String> {
        let list = Self::load_list()?;
        let vault = Self::load_vault();
        let current = Self::read_current(false)?;
        let active = Self::effective_active(&list, &current.email(), current.is_logged_in());
        let saved: Vec<&Account> = list.accounts.iter().filter(|a| vault.accounts.contains_key(&a.key)).collect();
        let start = saved.iter().position(|a| a.key == active).map_or(0, |i| i + 1);
        let next = saved
            .iter()
            .cycle()
            .skip(start)
            .take(saved.len())
            .find(|a| a.key != active)
            .ok_or("Cần ít nhất 2 tài khoản đã lưu đăng nhập để chuyển đổi.")?;
        let next = (*next).clone();
        Self::switch(&next.key)?;
        Ok(next)
    }

    /// Put back the login that was on the machine before the first switch.
    pub fn restore_original() -> Result<(), String> {
        let mut vault = Self::load_vault();
        let original = vault.original.clone().ok_or("Chưa có bản lưu ban đầu (chưa chuyển tài khoản lần nào).")?;
        let running = Self::running_apps();
        Self::quit_apps(&running)?;
        Self::write_snapshot(&original)?;
        vault.original = None;
        Self::save_vault(&vault)?;
        let mut list = Self::load_list()?;
        list.active.clear();
        Self::save_list(&list)?;
        for app in &running {
            Self::open_app(app)?;
        }
        Ok(())
    }
}

pub fn now_unix_secs() -> u64 {
    now_secs()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Antigravity 2.x login: app_storage.json + keychain (its state.vscdb rows go stale).
    fn set_login(home: &Path, email: &str, token: &str) {
        let path = AntigravityManager::app_storage_path(home, "Antigravity");
        let mut v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        v["agy.onboarding.lastLoginUsername"] = email.into();
        fs::write(&path, v.to_string()).unwrap();
        keychain_write(Some(token.as_bytes())).unwrap();
        fs::write(home.join(".gemini/google_accounts.json"), r#"{"active":"cli@corp.com","old":[]}"#).unwrap();
    }

    fn token(_home: &Path) -> Option<String> {
        keychain_read().unwrap().map(|b| String::from_utf8(b).unwrap())
    }

    /// Protobuf length-delimited field.
    fn field(n: u8, v: &[u8]) -> Vec<u8> {
        let mut out = vec![(n << 3) | 2, v.len() as u8];
        out.extend_from_slice(v);
        out
    }

    #[test]
    fn email_from_ide_user_status() {
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        // Same shape as the IDE record: base64(pb{1: {2: {1: base64(pb{7: email})}}}).
        let inner = [field(3, b"Demo User"), field(7, b"dev.user@example.com")].concat();
        let rec = field(1, &[field(1, b"userStatusSentinelKey"), field(2, &field(1, b64(&inner).as_bytes()))].concat());
        assert_eq!(email_in_proto(&b64(&rec)).as_deref(), Some("dev.user@example.com"));
        assert_eq!(email_in_proto("not base64!"), None);
    }

    #[test]
    fn capture_switch_restore() {
        let home = std::env::temp_dir().join(format!("cgs-ag-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let db = AntigravityManager::db_path(&home, "Antigravity");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::create_dir_all(home.join(".gemini")).unwrap();
        crate::config::TEST_HOME.with(|h| *h.borrow_mut() = Some(home.clone()));
        let conn = Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)", []).unwrap();
        conn.execute("INSERT INTO ItemTable VALUES ('theme', 'dark')", []).unwrap();
        fs::write(AntigravityManager::app_storage_path(&home, "Antigravity"), r#"{"projectsOrder":"p"}"#).unwrap();

        assert!(AntigravityManager::capture_current(None).is_err(), "nothing logged in yet");
        // Leftover login from an old Antigravity version must not win over the current one.
        conn.execute("INSERT INTO ItemTable VALUES ('antigravityAuthStatus', '{\"email\":\"stale@old.com\"}')", []).unwrap();
        drop(conn);

        set_login(&home, "a@x.com", "tok-a");
        let a = AntigravityManager::capture_current(None).unwrap();
        assert_eq!((a.key.as_str(), a.email.as_str()), ("a", "a@x.com"));
        // Pre-declared slot gets filled by email match.
        AntigravityManager::save_account(None, "b", "Backup", "b@x.com", "").unwrap();
        set_login(&home, "b@x.com", "tok-b");
        assert_eq!(AntigravityManager::capture_current(None).unwrap().key, "b");
        assert!(AntigravityManager::capture_current(Some("a".into())).is_err(), "email mismatch");
        assert_eq!(AntigravityManager::status().unwrap()["active"], "b");

        // Token refreshed while on b, then switch to a: b's newer token is kept.
        set_login(&home, "b@x.com", "tok-b2");
        AntigravityManager::switch("a").unwrap();
        assert_eq!(token(&home).as_deref(), Some("tok-a"));
        assert_eq!(AntigravityManager::status().unwrap()["email"], "a@x.com");
        AntigravityManager::switch("b").unwrap();
        assert_eq!(token(&home).as_deref(), Some("tok-b2"));

        // Unrelated rows untouched; original state (b, tok-b2) restorable.
        let theme: String = Connection::open(&db).unwrap()
            .query_row("SELECT value FROM ItemTable WHERE key='theme'", [], |r| r.get(0)).unwrap();
        assert_eq!(theme, "dark");
        let storage = fs::read_to_string(AntigravityManager::app_storage_path(&home, "Antigravity")).unwrap();
        assert!(storage.contains("projectsOrder") && storage.contains("b@x.com"), "{storage}");
        let st = AntigravityManager::status().unwrap();
        assert_eq!(st["apps"][0]["email"], "b@x.com");
        assert_eq!(st["apps"][2]["email"], "cli@corp.com");
        AntigravityManager::switch("a").unwrap();
        AntigravityManager::restore_original().unwrap();
        assert_eq!(token(&home).as_deref(), Some("tok-b2"));
        assert_eq!(AntigravityManager::status().unwrap()["hasOriginal"], false);

        // Chuyển đổi cycles a <-> b.
        assert_eq!(AntigravityManager::switch_next().unwrap().key, "a");
        assert_eq!(AntigravityManager::switch_next().unwrap().key, "b");
        assert_eq!(token(&home).as_deref(), Some("tok-b2"));

        // Clear local login: signed out everywhere, saved accounts intact and usable.
        AntigravityManager::clear_local().unwrap();
        let st = AntigravityManager::status().unwrap();
        assert_eq!((st["loggedIn"].as_bool(), st["active"].as_str()), (Some(false), Some("")));
        assert_eq!(token(&home), None);
        assert!(!fs::read_to_string(AntigravityManager::app_storage_path(&home, "Antigravity")).unwrap().contains("lastLoginUsername"));
        assert!(!home.join(".gemini/google_accounts.json").exists());
        AntigravityManager::switch("a").unwrap();
        assert_eq!(token(&home).as_deref(), Some("tok-a"));

        // Thêm tài khoản khác: a's refreshed token is saved first, then signed out for a new login.
        set_login(&home, "a@x.com", "tok-a2");
        assert_eq!(AntigravityManager::add_another().unwrap().unwrap().key, "a");
        assert_eq!(token(&home), None);
        set_login(&home, "c@x.com", "tok-c");
        assert_eq!(AntigravityManager::capture_current(None).unwrap().key, "c");
        AntigravityManager::switch("a").unwrap();
        assert_eq!(token(&home).as_deref(), Some("tok-a2"));

        AntigravityManager::remove_account("a").unwrap();
        assert!(AntigravityManager::switch("a").is_err());
        let _ = fs::remove_dir_all(&home);
    }

    /// Manual check against this machine (read-only): `cargo test real_machine -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_machine_status() {
        let st = AntigravityManager::status().unwrap();
        let snap = AntigravityManager::read_current(false).unwrap();
        let rows: Vec<_> = snap.apps.iter().map(|(a, r)| (a.clone(), r.values().filter(|v| v.is_some()).count())).collect();
        println!("loggedIn={} email={} active={:?} rows={rows:?}\napps={}", st["loggedIn"], st["email"], st["active"], st["apps"]);
    }
}
