//! Local failover proxy for Claude Code, across Claude profiles only.
//!
//! When the "Failover" entry is active, ~/.claude/settings.json points Claude
//! Code at 127.0.0.1:<port>. Each request goes to the first profile in the
//! chain that isn't cooling down; on a network error or a 401/402/403/408/429/5xx
//! it is retried on the next profile, with that profile's own URL, token and
//! model prefix. Antigravity/Gemini is never involved.

use crate::config::{ConfigManager, Profile};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const FAILOVER_KEY: &str = "__failover__";

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FailoverSettings {
    /// Profile keys in priority order.
    #[serde(default)]
    pub chain: Vec<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    /// How long a failing profile is skipped (unless Retry-After says otherwise).
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    /// Token Claude Code must send, so other local programs can't use the proxy.
    #[serde(default)]
    pub secret: String,
}

fn default_port() -> u16 { 18431 }
fn default_cooldown() -> u64 { 60 }

impl Default for FailoverSettings {
    fn default() -> Self {
        Self { chain: Vec::new(), port: default_port(), cooldown_secs: default_cooldown(), secret: String::new() }
    }
}

impl FailoverSettings {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn settings_path() -> PathBuf {
    ConfigManager::get_app_dir().join("failover.json")
}

pub fn load() -> FailoverSettings {
    let mut s: FailoverSettings = fs::read_to_string(settings_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if s.secret.is_empty() {
        s.secret = random_hex(24);
        let _ = save(&s);
    }
    s
}

pub fn save(s: &FailoverSettings) -> Result<(), String> {
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    ConfigManager::write_private(&settings_path(), &json)
}

fn random_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    if fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (t >> ((i % 16) * 8)) as u8 ^ (i as u8).wrapping_mul(97);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------- model names ----------

/// Prefix this profile puts before bare `claude-*` IDs ("cc/", "anthropic." …).
/// Older profiles have no modelPrefix: use the prefix all their Claude models share.
pub fn effective_prefix(p: &Profile) -> String {
    if !p.model_prefix.is_empty() {
        return p.model_prefix.clone();
    }
    let found: std::collections::HashSet<&str> = p
        .env
        .iter()
        .filter(|(k, _)| k.contains("MODEL"))
        .filter_map(|(_, v)| v.find("claude-").map(|i| &v[..i]))
        .collect();
    if found.len() == 1 { found.into_iter().next().unwrap().to_string() } else { String::new() }
}

/// Model name as `p` expects it: bare Claude IDs get its prefix, anything else passes through.
pub fn map_model(model: &str, p: &Profile) -> String {
    if model.starts_with("claude-") { format!("{}{model}", effective_prefix(p)) } else { model.to_string() }
}

/// settings.json env for failover mode: the first profile's model choices with its
/// prefix removed (the proxy re-adds each profile's own), URL/token pointing at the proxy.
pub fn claude_env(s: &FailoverSettings) -> Result<HashMap<String, String>, String> {
    let cfg = ConfigManager::load_profiles()?;
    let first = s
        .chain
        .iter()
        .find_map(|k| cfg.profiles.get(k))
        .ok_or("Chưa chọn profile nào cho failover")?;
    let prefix = effective_prefix(first);
    let mut env: HashMap<String, String> = first
        .env
        .iter()
        .map(|(k, v)| {
            let v = match v.strip_prefix(prefix.as_str()) {
                Some(rest) if !prefix.is_empty() && k.contains("MODEL") && rest.starts_with("claude-") => rest.to_string(),
                _ => v.clone(),
            };
            (k.clone(), v)
        })
        .collect();
    env.remove("ANTHROPIC_API_KEY");
    env.remove("ANTHROPIC_CUSTOM_HEADERS");
    env.insert("ANTHROPIC_BASE_URL".into(), s.url());
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), s.secret.clone());
    Ok(env)
}

// ---------- shared state ----------

#[derive(Default)]
struct State {
    port: Option<u16>,
    bind_error: String,
    last_profile: String,
    last_at: u64,
    events: Vec<(u64, String)>,
    cooldown: HashMap<String, Instant>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn server() -> &'static Mutex<Option<tauri::async_runtime::JoinHandle<()>>> {
    static H: OnceLock<Mutex<Option<tauri::async_runtime::JoinHandle<()>>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn event(msg: String) {
    let mut st = state().lock().unwrap();
    st.events.insert(0, (now_secs(), msg));
    st.events.truncate(8);
}

pub fn status() -> serde_json::Value {
    let s = load();
    let st = state().lock().unwrap();
    let now = Instant::now();
    let cooling: HashMap<&String, u64> = st
        .cooldown
        .iter()
        .filter(|(_, until)| **until > now)
        .map(|(k, until)| (k, (*until - now).as_secs() + 1))
        .collect();
    serde_json::json!({
        "chain": s.chain,
        "port": s.port,
        "cooldownSecs": s.cooldown_secs,
        "running": st.port == Some(s.port),
        "bindError": st.bind_error,
        "lastProfile": st.last_profile,
        "lastAt": st.last_at,
        "events": st.events.iter().map(|(at, msg)| serde_json::json!({ "at": at, "msg": msg })).collect::<Vec<_>>(),
        "cooling": cooling,
    })
}

/// Start (or move) the listener when a chain is configured; stop it otherwise.
pub fn ensure_running() {
    let s = load();
    let mut handle = server().lock().unwrap();
    {
        let st = state().lock().unwrap();
        if !s.chain.is_empty() && st.port == Some(s.port) && handle.is_some() {
            return;
        }
    }
    if let Some(h) = handle.take() {
        h.abort();
    }
    let mut st = state().lock().unwrap();
    st.port = None;
    st.bind_error.clear();
    if s.chain.is_empty() {
        return;
    }
    match std::net::TcpListener::bind(("127.0.0.1", s.port)).and_then(|l| l.set_nonblocking(true).map(|_| l)) {
        Ok(std_listener) => {
            st.port = Some(s.port);
            *handle = Some(tauri::async_runtime::spawn(async move {
                let Ok(listener) = tokio::net::TcpListener::from_std(std_listener) else { return };
                loop {
                    if let Ok((sock, _)) = listener.accept().await {
                        tokio::spawn(async move {
                            let _ = handle_conn(sock).await;
                        });
                    }
                }
            }));
        }
        Err(e) => st.bind_error = format!("Không mở được cổng {}: {e}", s.port),
    }
}

// ---------- HTTP ----------

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(600))
            .build()
            .expect("http client")
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Decode a complete chunked body; None while more bytes are needed.
fn decode_chunked(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = find(b, b"\r\n")?;
        let size_str = std::str::from_utf8(&b[..line_end]).ok()?.split(';').next()?.trim().to_string();
        let size = usize::from_str_radix(&size_str, 16).ok()?;
        b = &b[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if b.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

async fn write_error(sock: &mut TcpStream, code: u16, kind: &str, msg: &str) -> std::io::Result<()> {
    let body = serde_json::json!({ "type": "error", "error": { "type": kind, "message": msg } }).to_string();
    let reason = reqwest::StatusCode::from_u16(code).ok().and_then(|s| s.canonical_reason()).unwrap_or("");
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(resp.as_bytes()).await
}

const SKIP_REQ: &[&str] = &[
    "host", "content-length", "transfer-encoding", "connection", "keep-alive", "authorization",
    "x-api-key", "accept-encoding", "proxy-connection", "proxy-authorization", "te", "upgrade",
];
const SKIP_RESP: &[&str] = &["content-length", "transfer-encoding", "connection", "keep-alive"];

fn retryable(code: u16) -> bool {
    matches!(code, 401 | 402 | 403 | 408 | 429) || (500..=599).contains(&code)
}

async fn handle_conn(mut sock: TcpStream) -> Result<(), String> {
    let mut buf = Vec::with_capacity(16 * 1024);
    let mut tmp = vec![0u8; 64 * 1024];
    let head_len = loop {
        let n = sock.read(&mut tmp).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 256 * 1024 {
            return Err("header too large".into());
        }
    };
    let (method, path, headers) = {
        let mut hs = [httparse::EMPTY_HEADER; 128];
        let mut req = httparse::Request::new(&mut hs);
        req.parse(&buf[..head_len]).map_err(|e| e.to_string())?;
        let headers: Vec<(String, String)> = req
            .headers
            .iter()
            .map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).into_owned()))
            .collect();
        (req.method.unwrap_or("GET").to_string(), req.path.unwrap_or("/").to_string(), headers)
    };
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());

    let mut body = buf[head_len..].to_vec();
    if let Some(len) = header("content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
        while body.len() < len {
            let n = sock.read(&mut tmp).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(len);
    } else if header("transfer-encoding").map_or(false, |v| v.to_ascii_lowercase().contains("chunked")) {
        body = loop {
            if let Some(decoded) = decode_chunked(&body) {
                break decoded;
            }
            let n = sock.read(&mut tmp).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("truncated chunked body".into());
            }
            body.extend_from_slice(&tmp[..n]);
        };
    }

    let settings = load();
    let bearer = format!("Bearer {}", settings.secret);
    if header("authorization") != Some(bearer.as_str()) && header("x-api-key") != Some(settings.secret.as_str()) {
        let _ = write_error(&mut sock, 401, "authentication_error", "Failover proxy: sai token").await;
        return Ok(());
    }

    let cfg = ConfigManager::load_profiles()?;
    let chain: Vec<(String, Profile)> = settings
        .chain
        .iter()
        .filter_map(|k| cfg.profiles.get(k).map(|p| (k.clone(), p.clone())))
        .collect();
    if chain.is_empty() {
        let _ = write_error(&mut sock, 503, "api_error", "Failover: chưa có profile nào trong chuỗi").await;
        return Ok(());
    }
    // Profiles in cooldown go last (still tried if nothing else works).
    let order: Vec<&(String, Profile)> = {
        let st = state().lock().unwrap();
        let now = Instant::now();
        let cooling = |k: &String| st.cooldown.get(k).map_or(false, |u| *u > now);
        let (ready, waiting): (Vec<_>, Vec<_>) = chain.iter().partition(|(k, _)| !cooling(k));
        ready.into_iter().chain(waiting).collect()
    };

    let json: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
    let model = json.as_ref().and_then(|v| v.get("model")?.as_str().map(str::to_string));
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let mut failures: Vec<String> = Vec::new();

    for (i, (key, p)) in order.iter().enumerate() {
        let is_last = i + 1 == order.len();
        let base = p.env.get("ANTHROPIC_BASE_URL").map(String::as_str).unwrap_or("https://api.anthropic.com");
        let url = format!("{}{}", base.trim_end_matches('/'), path);

        let out_body = match (&json, &model) {
            (Some(v), Some(m)) if map_model(m, p) != *m => {
                let mut v = v.clone();
                v["model"] = map_model(m, p).into();
                serde_json::to_vec(&v).map_err(|e| e.to_string())?
            }
            _ => body.clone(),
        };
        let mut rb = client().request(method.clone(), &url);
        for (k, v) in &headers {
            if !SKIP_REQ.contains(&k.as_str()) {
                rb = rb.header(k.as_str(), v.as_str());
            }
        }
        if let Some(t) = p.env.get("ANTHROPIC_AUTH_TOKEN").filter(|t| !t.is_empty()) {
            rb = rb.bearer_auth(t);
        }
        if let Some(t) = p.env.get("ANTHROPIC_API_KEY").filter(|t| !t.is_empty()) {
            rb = rb.header("x-api-key", t.as_str());
        }
        for line in p.env.get("ANTHROPIC_CUSTOM_HEADERS").map(String::as_str).unwrap_or("").lines() {
            if let Some((k, v)) = line.split_once(':') {
                rb = rb.header(k.trim(), v.trim());
            }
        }

        match rb.body(out_body).send().await {
            Ok(resp) if retryable(resp.status().as_u16()) && !is_last => {
                let code = resp.status().as_u16();
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok())
                    .unwrap_or(settings.cooldown_secs)
                    .clamp(5, 3600);
                state().lock().unwrap().cooldown.insert(key.clone(), Instant::now() + Duration::from_secs(wait));
                failures.push(format!("{} HTTP {code}", p.name));
            }
            Ok(resp) => {
                let code = resp.status().as_u16();
                {
                    let mut st = state().lock().unwrap();
                    st.last_profile = key.clone();
                    st.last_at = now_secs();
                    if !retryable(code) {
                        st.cooldown.remove(key);
                    }
                }
                if !failures.is_empty() {
                    event(format!("{} → chuyển sang {}", failures.join(", "), p.name));
                }
                return stream_back(&mut sock, resp, method == reqwest::Method::HEAD).await;
            }
            Err(e) => {
                state().lock().unwrap().cooldown.insert(key.clone(), Instant::now() + Duration::from_secs(settings.cooldown_secs));
                let reason = if e.is_connect() { "không kết nối được" } else if e.is_timeout() { "timeout" } else { "lỗi mạng" };
                failures.push(format!("{} {reason}", p.name));
                if is_last {
                    event(format!("Tất cả profile lỗi: {}", failures.join(", ")));
                    let msg = format!("Failover: tất cả profile đều lỗi ({})", failures.join(", "));
                    let _ = write_error(&mut sock, 502, "api_error", &msg).await;
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

async fn stream_back(sock: &mut TcpStream, resp: reqwest::Response, head_only: bool) -> Result<(), String> {
    let status = resp.status();
    let mut head = format!("HTTP/1.1 {} {}\r\n", status.as_u16(), status.canonical_reason().unwrap_or(""));
    for (k, v) in resp.headers() {
        if SKIP_RESP.contains(&k.as_str()) {
            continue;
        }
        if let Ok(v) = v.to_str() {
            head.push_str(&format!("{}: {v}\r\n", k.as_str()));
        }
    }
    head.push_str(if head_only { "connection: close\r\n\r\n" } else { "transfer-encoding: chunked\r\nconnection: close\r\n\r\n" });
    sock.write_all(head.as_bytes()).await.map_err(|e| e.to_string())?;
    if head_only {
        return Ok(());
    }
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            continue;
        }
        sock.write_all(format!("{:x}\r\n", chunk.len()).as_bytes()).await.map_err(|e| e.to_string())?;
        sock.write_all(&chunk).await.map_err(|e| e.to_string())?;
        sock.write_all(b"\r\n").await.map_err(|e| e.to_string())?;
        sock.flush().await.map_err(|e| e.to_string())?;
    }
    sock.write_all(b"0\r\n\r\n").await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(prefix: &str, env: &[(&str, &str)]) -> Profile {
        Profile {
            name: "p".into(),
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            model_prefix: prefix.into(),
        }
    }

    #[test]
    fn model_mapping_and_chunked() {
        let cc = profile("cc/", &[]);
        let old = profile("", &[("ANTHROPIC_DEFAULT_OPUS_MODEL", "anthropic.claude-opus-5-5")]);
        let plain = profile("", &[]);
        assert_eq!(map_model("claude-opus-5-5", &cc), "cc/claude-opus-5-5");
        assert_eq!(map_model("claude-opus-5-5", &old), "anthropic.claude-opus-5-5");
        assert_eq!(map_model("claude-opus-5-5", &plain), "claude-opus-5-5");
        assert_eq!(map_model("gpt-5", &cc), "gpt-5");
        assert_eq!(decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(), b"Wikipedia");
        assert!(decode_chunked(b"4\r\nWi").is_none());
        assert!(retryable(429) && retryable(529) && !retryable(400) && !retryable(200));
    }

    /// Mock upstream: answers every connection with `status`, echoing what it received.
    async fn upstream(status: u16, hits: std::sync::Arc<Mutex<Vec<String>>>) -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let mut got = Vec::new();
                loop {
                    let n = s.read(&mut buf).await.unwrap();
                    got.extend_from_slice(&buf[..n]);
                    if let Some(h) = find(&got, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&got[..h]).to_ascii_lowercase();
                        let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
                        if got.len() >= h + 4 + len { break; }
                    }
                }
                let req = String::from_utf8_lossy(&got).to_string();
                hits.lock().unwrap().push(req.clone());
                let auth = req.lines().find(|l| l.to_ascii_lowercase().starts_with("authorization:")).unwrap_or("").to_string();
                let body = format!("event: message_start\ndata: {{\"auth\":\"{}\"}}\n\n", auth.trim());
                let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                s.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        port
    }

    async fn call(proxy: &tokio::net::TcpListener, auth: &str, body: &str) -> String {
        let addr = proxy.local_addr().unwrap();
        let req = format!("POST /v1/messages?beta=true HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {auth}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
        let client = tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(req.as_bytes()).await.unwrap();
            let mut out = Vec::new();
            c.read_to_end(&mut out).await.unwrap();
            String::from_utf8_lossy(&out).to_string()
        });
        let (sock, _) = proxy.accept().await.unwrap();
        handle_conn(sock).await.unwrap();
        client.await.unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn proxy_fails_over_between_claude_profiles() {
        let home = std::env::temp_dir().join(format!("cgs-fo-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".claude")).unwrap();
        crate::config::TEST_HOME.with(|h| *h.borrow_mut() = Some(home.clone()));

        let (hits_a, hits_b) = (Default::default(), Default::default());
        let pa = upstream(429, std::sync::Arc::clone(&hits_a)).await;
        let pb = upstream(200, std::sync::Arc::clone(&hits_b)).await;
        let prof = |port: u16, tok: &str, prefix: &str, opus: &str| Profile {
            name: tok.to_uppercase(),
            env: [("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}")), ("ANTHROPIC_AUTH_TOKEN", tok.into()),
                  ("ANTHROPIC_DEFAULT_OPUS_MODEL", opus.into())].into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            model_prefix: prefix.into(),
        };
        ConfigManager::save_profiles(&crate::config::ProfilesConfig {
            active: String::new(),
            profiles: HashMap::from([
                ("a".to_string(), prof(pa, "ta", "cc/", "cc/claude-opus-5-5")),
                ("b".to_string(), prof(pb, "tb", "", "claude-opus-5-5")),
            ]),
        }).unwrap();
        save(&FailoverSettings { chain: vec!["a".into(), "b".into()], secret: "s".into(), ..Default::default() }).unwrap();

        // settings.json in failover mode: proxy URL/token, bare model names.
        ConfigManager::apply_profile(FAILOVER_KEY).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap()).unwrap();
        assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:18431");
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "s");
        assert_eq!(settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus-5-5");
        assert_eq!(ConfigManager::load_effective_profiles().unwrap().active, FAILOVER_KEY);

        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert!(call(&proxy, "wrong", "{}").await.starts_with("HTTP/1.1 401"));

        let body = r#"{"model":"claude-opus-5-5","max_tokens":5,"stream":true}"#;
        let out = call(&proxy, "s", body).await;
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("Bearer tb") && out.contains("message_start"), "{out}");
        let a = hits_a.lock().unwrap()[0].clone();
        assert!(a.contains("\"model\":\"cc/claude-opus-5-5\"") && a.to_lowercase().contains("authorization: bearer ta"), "{a}");
        assert!(hits_b.lock().unwrap()[0].contains(body), "unchanged body for profile without prefix");
        assert!(status()["events"][0]["msg"].as_str().unwrap().contains("HTTP 429"));

        // A is cooling down now: the next request goes straight to B.
        call(&proxy, "s", body).await;
        assert_eq!(hits_a.lock().unwrap().len(), 1);
        assert_eq!(hits_b.lock().unwrap().len(), 2);

        ConfigManager::restore_native().unwrap();
        let _ = fs::remove_dir_all(&home);
    }
}
