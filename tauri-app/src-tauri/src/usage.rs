//! Daily token usage & estimated USD spend per model for BOTH:
//! 1. Claude Code transcripts (`~/.claude/projects/**/*.jsonl`) -> `usage.json`
//! 2. Gemini Antigravity (local SQLite conversations + proxy telemetry) -> `usage_antigravity.json`

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SHOW_DAYS: i64 = 30;
const KEEP_DAYS: i64 = 90;

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    #[serde(rename = "cacheRead")]
    pub cache_read: u64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: u64,
    pub messages: u64,
}

impl Tokens {
    fn max(self, o: Tokens) -> Tokens {
        Tokens {
            input: self.input.max(o.input),
            output: self.output.max(o.output),
            cache_read: self.cache_read.max(o.cache_read),
            cache_write: self.cache_write.max(o.cache_write),
            messages: self.messages.max(o.messages),
        }
    }
    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.messages += o.messages;
    }
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// day ("YYYY-MM-DD", local) -> model -> tokens
pub type Days = BTreeMap<String, BTreeMap<String, Tokens>>;

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Store {
    pub days: Days,
}

#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<Msg>,
}
#[derive(Deserialize)]
struct Msg {
    id: Option<String>,
    model: Option<String>,
    usage: Option<RawUsage>,
}
#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

// --- civil date helpers (no chrono) ---------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> String {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

/// "2026-06-22T10:12:09.862Z" -> unix seconds
fn parse_ts(ts: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let days = days_from_civil(n(0..4)?, n(5..7)?, n(8..10)?);
    Some(days * 86400 + n(11..13)? * 3600 + n(14..16)? * 60 + n(17..19)?)
}

fn local_day_index(unix: i64, tz_offset_min: i64) -> i64 {
    (unix + tz_offset_min * 60).div_euclid(86400)
}

/// "cc/claude-opus-5-5" -> "claude-opus-5-5"
fn normalize_model(m: &str) -> String {
    m.rsplit('/').next().unwrap_or(m).to_string()
}

/// Approximate public API price (USD per 1M tokens: input, output, cache_read, cache_write).
pub fn estimate_cost_usd(model: &str, t: &Tokens) -> f64 {
    let m = model.to_ascii_lowercase();
    let (pin, pout, pcr, pcw) = if m.contains("opus") || m.contains("fable") {
        (15.0, 75.0, 1.5, 18.75)
    } else if m.contains("sonnet") {
        (3.0, 15.0, 0.3, 3.75)
    } else if m.contains("haiku") {
        (0.8, 4.0, 0.08, 1.0)
    } else if m.contains("gemini") && (m.contains("pro") || m.contains("carbon") || m.contains("argon")) {
        (1.25, 10.0, 0.31, 1.25)
    } else if m.contains("gemini") && (m.contains("flash") || m.contains("lite") || m.contains("neon") || m.contains("hydrogen")) {
        (0.30, 2.50, 0.075, 0.30)
    } else if m.contains("gemini") {
        (1.0, 6.0, 0.25, 1.0)
    } else if m.contains("gpt") {
        (2.5, 10.0, 0.5, 2.5)
    } else {
        (1.0, 4.0, 0.1, 1.0)
    };
    (t.input as f64 * pin
        + t.output as f64 * pout
        + t.cache_read as f64 * pcr
        + t.cache_write as f64 * pcw)
        / 1_000_000.0
}

// --- Claude JSONL scanning ------------------------------------------------

fn collect_jsonl(dir: &Path, since: SystemTime, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            collect_jsonl(&p, since, out);
        } else if p.extension().is_some_and(|x| x == "jsonl")
            && meta.modified().map(|m| m >= since).unwrap_or(true)
        {
            out.push(p);
        }
    }
}

fn scan(projects: &Path, tz_offset_min: i64, first_day: i64) -> Days {
    let since = SystemTime::now() - Duration::from_secs((SHOW_DAYS as u64 + 1) * 86400);
    let mut files = Vec::new();
    collect_jsonl(projects, since, &mut files);

    let mut per_msg: HashMap<String, (i64, String, Tokens)> = HashMap::new();
    let mut anon: Vec<(i64, String, Tokens)> = Vec::new();
    for f in files {
        let Ok(file) = fs::File::open(&f) else { continue };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(l) = serde_json::from_str::<Line>(&line) else { continue };
            let (Some(msg), Some(ts)) = (l.message, l.timestamp) else { continue };
            let (Some(u), Some(model)) = (msg.usage, msg.model) else { continue };
            if model.starts_with('<') {
                continue;
            }
            let Some(unix) = parse_ts(&ts) else { continue };
            let day = local_day_index(unix, tz_offset_min);
            if day < first_day {
                continue;
            }
            let t = Tokens {
                input: u.input_tokens.unwrap_or(0),
                output: u.output_tokens.unwrap_or(0),
                cache_read: u.cache_read_input_tokens.unwrap_or(0),
                cache_write: u.cache_creation_input_tokens.unwrap_or(0),
                messages: 1,
            };
            let model = normalize_model(&model);
            match msg.id {
                Some(id) => {
                    let key = format!("{id}:{}", l.request_id.unwrap_or_default());
                    per_msg
                        .entry(key)
                        .and_modify(|e| e.2 = e.2.max(t))
                        .or_insert((day, model, t));
                }
                None => anon.push((day, model, t)),
            }
        }
    }

    let mut days = Days::new();
    for (day, model, t) in per_msg.into_values().chain(anon) {
        days.entry(civil_from_days(day))
            .or_default()
            .entry(model)
            .or_default()
            .add(&t);
    }
    days
}

// --- Antigravity proxy + local SQLite protobuf scanner --------------------

pub fn store_path(app_dir: &Path) -> PathBuf {
    app_dir.join("usage.json")
}

pub fn antigravity_store_path(app_dir: &Path) -> PathBuf {
    app_dir.join("usage_antigravity.json")
}

fn today_index(tz_offset_min: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    local_day_index(now, tz_offset_min)
}

/// Record tokens intercepted by the local Antigravity failover proxy.
#[allow(dead_code)]
pub fn record_antigravity_tokens(
    app_dir: &Path,
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
) {
    let path = antigravity_store_path(app_dir);
    let mut store: Store = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let today = civil_from_days(today_index(0));
    let entry = store
        .days
        .entry(today)
        .or_default()
        .entry(normalize_model(model))
        .or_default();
    entry.add(&Tokens {
        input,
        output,
        cache_read,
        cache_write: 0,
        messages: 1,
    });
    let _ = fs::create_dir_all(app_dir);
    if let Ok(json) = serde_json::to_string(&store) {
        let _ = fs::write(&path, json);
    }
}

struct ParsedAgTurn {
    ts_secs: Option<i64>,
    model: String,
    label: String,
    step_uuid: String,
    bot_id: String,
    response_id: String,
    tokens: Tokens,
}

/// Lightweight protobuf wire reader for Antigravity `gen_metadata` BLOBs
/// (matching CodexBar's `AntigravityProtoReader`: field 1 = chat, #4 = usage, #9.#4 = timestamp, #19 = model, #21 = label, root #4 = stepUUID).
fn parse_ag_proto_turn(bytes: &[u8]) -> Option<ParsedAgTurn> {
    let mut model = String::new();
    let mut label = String::new();
    let mut step_uuid = String::new();
    let mut bot_id = String::new();
    let mut response_id = String::new();
    let mut ts_secs: Option<i64> = None;
    let mut tokens = Tokens::default();
    let mut found_usage = false;

    for (fnum, wire, _, data) in ProtoIter::new(bytes) {
        if fnum == 4 && wire == 2 {
            if let Ok(s) = std::str::from_utf8(data) {
                if !s.trim().is_empty() {
                    step_uuid = s.trim().to_string();
                }
            }
        } else if fnum == 1 && wire == 2 {
            for (cf, cw, _, cd) in ProtoIter::new(data) {
                match (cf, cw) {
                    (4, 2) => {
                        for (uf, uw, uv, ud) in ProtoIter::new(cd) {
                            match (uf, uw) {
                                (2, 0) => { tokens.input = uv; found_usage = true; }
                                (5, 0) => { tokens.cache_read = uv; found_usage = true; }
                                (7, 2) => {
                                    if let Ok(s) = std::str::from_utf8(ud) {
                                        if !s.trim().is_empty() { bot_id = s.trim().to_string(); }
                                    }
                                }
                                (9, 0) => { tokens.output += uv; found_usage = true; }
                                (10, 0) => { tokens.output += uv; found_usage = true; }
                                (11, 2) => {
                                    if let Ok(s) = std::str::from_utf8(ud) {
                                        if !s.trim().is_empty() { response_id = s.trim().to_string(); }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    (9, 2) => {
                        for (gf, gw, _, gd) in ProtoIter::new(cd) {
                            if gf == 4 && gw == 2 {
                                for (tf, tw, tv, _) in ProtoIter::new(gd) {
                                    if tf == 1 && tw == 0 && tv > 1_600_000_000 && tv < 2_500_000_000 {
                                        ts_secs = Some(tv as i64);
                                    }
                                }
                            }
                        }
                    }
                    (19, 2) => {
                        if let Ok(s) = std::str::from_utf8(cd) {
                            if !s.trim().is_empty() {
                                model = s.trim().to_string();
                            }
                        }
                    }
                    (21, 2) => {
                        if let Ok(s) = std::str::from_utf8(cd) {
                            if !s.trim().is_empty() {
                                label = s.trim().to_string();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if !found_usage || tokens.total() == 0 {
        return None;
    }
    tokens.messages = 1;
    Some(ParsedAgTurn {
        ts_secs,
        model: if model.is_empty() { String::new() } else { normalize_model(&model) },
        label,
        step_uuid,
        bot_id,
        response_id,
        tokens,
    })
}

/// Extract `(ts_secs, step_uuid, bot_id)` from `steps.metadata` protobuf BLOBs.
fn parse_ag_step_metadata(bytes: &[u8]) -> Option<(i64, String, String)> {
    let mut ts_secs: Option<i64> = None;
    let mut step_uuid = String::new();
    let mut bot_id = String::new();

    for (fnum, wire, _, data) in ProtoIter::new(bytes) {
        match (fnum, wire) {
            (1, 2) => {
                for (tf, tw, tv, _) in ProtoIter::new(data) {
                    if tf == 1 && tw == 0 && tv > 1_600_000_000 && tv < 2_500_000_000 {
                        ts_secs = Some(tv as i64);
                    }
                }
            }
            (9, 2) => {
                for (sf, sw, _, sd) in ProtoIter::new(data) {
                    if sf == 7 && sw == 2 {
                        if let Ok(s) = std::str::from_utf8(sd) {
                            if !s.trim().is_empty() { bot_id = s.trim().to_string(); }
                        }
                    }
                }
            }
            (12, 2) => {
                if let Ok(s) = std::str::from_utf8(data) {
                    if !s.trim().is_empty() { step_uuid = s.trim().to_string(); }
                }
            }
            _ => {}
        }
    }

    Some((ts_secs?, step_uuid, bot_id))
}

struct ProtoIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ProtoIter<'a> {
    fn new(buf: &'a [u8]) -> Self { Self { buf, pos: 0 } }
    fn read_varint(&mut self) -> Option<u64> {
        let mut res = 0u64;
        for i in 0..10 {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            res |= ((b & 0x7f) as u64) << (i * 7);
            if b & 0x80 == 0 {
                return Some(res);
            }
        }
        None
    }
}

impl<'a> Iterator for ProtoIter<'a> {
    type Item = (u32, u8, u64, &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.read_varint()?;
        let fnum = (tag >> 3) as u32;
        let wire = (tag & 7) as u8;
        match wire {
            0 => {
                let v = self.read_varint()?;
                Some((fnum, wire, v, &[]))
            }
            1 => {
                if self.pos + 8 > self.buf.len() { return None; }
                let sl = &self.buf[self.pos..self.pos + 8];
                self.pos += 8;
                Some((fnum, wire, 0, sl))
            }
            2 => {
                let len = self.read_varint()? as usize;
                if self.pos + len > self.buf.len() { return None; }
                let sl = &self.buf[self.pos..self.pos + len];
                self.pos += len;
                Some((fnum, wire, len as u64, sl))
            }
            5 => {
                if self.pos + 4 > self.buf.len() { return None; }
                let sl = &self.buf[self.pos..self.pos + 4];
                self.pos += 4;
                Some((fnum, wire, 0, sl))
            }
            _ => None,
        }
    }
}

type FileStamp = (u128, u64); // (mtime_nanos, size_bytes)

#[derive(Default)]
struct ScanCache {
    ag_files: HashMap<PathBuf, (FileStamp, Days)>,
    last_key: Option<(PathBuf, i64)>,
    last_at: u64,
    last_value: Option<serde_json::Value>,
}

fn scan_cache() -> &'static Mutex<ScanCache> {
    static C: OnceLock<Mutex<ScanCache>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn file_stamp(meta: &fs::Metadata) -> Option<FileStamp> {
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some((mtime, meta.len()))
}

fn open_ro_sqlite(db: &Path) -> Option<rusqlite::Connection> {
    use rusqlite::{Connection, OpenFlags};
    if let Ok(conn) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        return Some(conn);
    }
    let encoded = db.to_string_lossy().replace('?', "%3F").replace('#', "%23");
    let uri = format!("file:{encoded}?mode=ro&immutable=1");
    Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI).ok()
}

fn parse_ag_db_file(db: &Path, mtime_secs: i64, tz_offset_min: i64, first_day: i64) -> Days {
    let mut days = Days::new();
    let Some(conn) = open_ro_sqlite(db) else { return days };
    let _ = conn.busy_timeout(Duration::from_millis(200));

    let mut ts_by_bot: HashMap<String, i64> = HashMap::new();
    let mut ts_by_step: HashMap<String, i64> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT metadata FROM steps WHERE typeof(metadata)='blob' AND length(metadata) <= 4194304 LIMIT 5000",
    ) {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                if let Ok(blob) = row.get::<_, Vec<u8>>(0) {
                    if let Some((ts, step_uuid, bot_id)) = parse_ag_step_metadata(&blob) {
                        if !bot_id.is_empty() {
                            ts_by_bot.entry(bot_id).or_insert(ts);
                        }
                        if !step_uuid.is_empty() {
                            ts_by_step.entry(step_uuid).or_insert(ts);
                        }
                    }
                }
            }
        }
    }

    let mut turns: Vec<ParsedAgTurn> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT data FROM gen_metadata WHERE typeof(data)='blob' AND length(data) <= 8388608 LIMIT 2000",
    ) {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                if let Ok(blob) = row.get::<_, Vec<u8>>(0) {
                    if let Some(t) = parse_ag_proto_turn(&blob) {
                        turns.push(t);
                    }
                }
            }
        }
    }
    if turns.is_empty() {
        return days;
    }

    let mut label_model: HashMap<String, String> = HashMap::new();
    for t in &turns {
        if !t.label.is_empty() && !t.model.is_empty() {
            label_model.entry(t.label.clone()).or_insert_with(|| t.model.clone());
        }
    }

    let mut seen_resp: std::collections::HashSet<String> = std::collections::HashSet::new();
    for t in turns {
        if !t.response_id.is_empty() && !seen_resp.insert(t.response_id.clone()) {
            continue;
        }
        let ts = t
            .ts_secs
            .or_else(|| (!t.bot_id.is_empty()).then(|| ts_by_bot.get(&t.bot_id).copied()).flatten())
            .or_else(|| (!t.step_uuid.is_empty()).then(|| ts_by_step.get(&t.step_uuid).copied()).flatten())
            .unwrap_or(mtime_secs);
        if ts <= 0 {
            continue;
        }
        let day = local_day_index(ts, tz_offset_min);
        if day < first_day {
            continue;
        }
        let model = if !t.model.is_empty() {
            t.model
        } else if let Some(inherited) = (!t.label.is_empty()).then(|| label_model.get(&t.label)).flatten() {
            inherited.clone()
        } else {
            "gemini-antigravity".to_string()
        };
        days.entry(civil_from_days(day))
            .or_default()
            .entry(model)
            .or_default()
            .add(&t.tokens);
    }
    days
}

/// Scan local Antigravity conversation SQLite DBs via `rusqlite`, caching unchanged files by `(mtime, size)`.
fn scan_antigravity_local(home: &Path, tz_offset_min: i64, first_day: i64) -> Days {
    let mut days = Days::new();
    let since = SystemTime::now() - Duration::from_secs((SHOW_DAYS as u64 + 1) * 86400);
    let roots = [
        home.join(".gemini/antigravity/conversations"),
        home.join(".gemini/antigravity-cli/conversations"),
        home.join(".gemini/antigravity-ide/conversations"),
    ];

    let mut db_files: Vec<(SystemTime, FileStamp, PathBuf)> = Vec::new();
    for r in roots {
        if let Ok(rd) = fs::read_dir(&r) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "db") {
                    if let Ok(meta) = e.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            if mtime >= since {
                                // Factor active -wal size into stamp so live WAL writes invalidate cache immediately.
                                let mut stamp = file_stamp(&meta).unwrap_or((0, meta.len()));
                                let wal = PathBuf::from(format!("{}-wal", p.display()));
                                if let Ok(wm) = fs::metadata(&wal) {
                                    if let Some((wt, wl)) = file_stamp(&wm) {
                                        stamp.0 = stamp.0.max(wt);
                                        stamp.1 = stamp.1.wrapping_add(wl);
                                    }
                                }
                                db_files.push((mtime, stamp, p));
                            }
                        }
                    }
                }
            }
        }
    }
    db_files.sort_by(|a, b| b.0.cmp(&a.0));
    db_files.truncate(150);

    for (mtime, stamp, db) in db_files {
        if let Some(cached) = scan_cache()
            .lock()
            .ok()
            .and_then(|c| c.ag_files.get(&db).filter(|(st, _)| *st == stamp).map(|(_, d)| d.clone()))
        {
            for (day, models) in cached {
                let slot = days.entry(day).or_default();
                for (m, t) in models {
                    slot.entry(m).or_default().add(&t);
                }
            }
            continue;
        }

        let mtime_secs = mtime.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let parsed = parse_ag_db_file(&db, mtime_secs, tz_offset_min, first_day);
        for (day, models) in &parsed {
            let slot = days.entry(day.clone()).or_default();
            for (m, t) in models {
                slot.entry(m.clone()).or_default().add(t);
            }
        }
        if let Ok(mut c) = scan_cache().lock() {
            c.ag_files.insert(db, (stamp, parsed));
        }
    }
    days
}

/// Rescan Claude + Antigravity usage, merge into persisted stores, and return unified 30-day analytics.
/// Uses a 15-second in-memory snapshot TTL so repeated popover/focus opens return in <1ms.
pub fn refresh(home: &Path, app_dir: &Path, tz_offset_min: i64) -> Result<serde_json::Value, String> {
    let now_s = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if !cfg!(test) {
        if let Ok(c) = scan_cache().lock() {
            if c.last_key.as_ref() == Some(&(home.to_path_buf(), tz_offset_min))
                && now_s.saturating_sub(c.last_at) < 15
            {
                if let Some(ref v) = c.last_value {
                    return Ok(v.clone());
                }
            }
        }
    }

    let today = today_index(tz_offset_min);
    let first_show = today - (SHOW_DAYS - 1);
    let keep_from = civil_from_days(today - (KEEP_DAYS - 1));

    // 1. Claude usage
    let c_path = store_path(app_dir);
    let mut c_store: Store = fs::read_to_string(&c_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    for (day, models) in scan(&home.join(".claude").join("projects"), tz_offset_min, first_show) {
        let slot = c_store.days.entry(day).or_default();
        for (model, t) in models {
            let e = slot.entry(model).or_default();
            *e = e.max(t);
        }
    }
    c_store.days.retain(|d, _| *d >= keep_from);
    fs::create_dir_all(app_dir).map_err(|e| e.to_string())?;
    fs::write(&c_path, serde_json::to_string(&c_store).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;

    // 2. Antigravity usage (persisted proxy metrics + local SQLite conversation scan)
    let ag_path = antigravity_store_path(app_dir);
    let mut ag_store: Store = fs::read_to_string(&ag_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    for (day, models) in scan_antigravity_local(home, tz_offset_min, first_show) {
        let slot = ag_store.days.entry(day).or_default();
        for (model, t) in models {
            let e = slot.entry(model).or_default();
            *e = e.max(t);
        }
    }
    ag_store.days.retain(|d, _| *d >= keep_from);
    let _ = fs::write(&ag_path, serde_json::to_string(&ag_store).unwrap_or_default());

    let today_str = civil_from_days(today);
    let mut combined_days = Vec::with_capacity(SHOW_DAYS as usize);
    let mut claude_days = Vec::with_capacity(SHOW_DAYS as usize);
    let mut ag_days = Vec::with_capacity(SHOW_DAYS as usize);

    let mut c_total_30d = Tokens::default();
    let mut c_today = Tokens::default();
    let mut c_cost_30d = 0.0f64;
    let mut c_cost_today = 0.0f64;

    let mut ag_total_30d = Tokens::default();
    let mut ag_today = Tokens::default();
    let mut ag_cost_30d = 0.0f64;
    let mut ag_cost_today = 0.0f64;

    let mut model_totals: HashMap<(String, String), (Tokens, f64)> = HashMap::new();

    for d in first_show..=today {
        let date = civil_from_days(d);
        let cm = c_store.days.get(&date).cloned().unwrap_or_default();
        let am = ag_store.days.get(&date).cloned().unwrap_or_default();
        let mut merged = cm.clone();

        for (m, t) in &cm {
            let cost = estimate_cost_usd(m, t);
            c_total_30d.add(t);
            c_cost_30d += cost;
            if date == today_str {
                c_today.add(t);
                c_cost_today += cost;
            }
            let e = model_totals.entry(("claude".to_string(), m.clone())).or_default();
            e.0.add(t);
            e.1 += cost;
        }
        for (m, t) in &am {
            let cost = estimate_cost_usd(m, t);
            ag_total_30d.add(t);
            ag_cost_30d += cost;
            if date == today_str {
                ag_today.add(t);
                ag_cost_today += cost;
            }
            let e = model_totals.entry(("antigravity".to_string(), m.clone())).or_default();
            e.0.add(t);
            e.1 += cost;
            merged.entry(m.clone()).or_default().add(t);
        }

        combined_days.push(serde_json::json!({ "date": date, "models": merged }));
        claude_days.push(serde_json::json!({ "date": date, "models": cm }));
        ag_days.push(serde_json::json!({ "date": date, "models": am }));
    }

    let mut model_rows: Vec<_> = model_totals
        .into_iter()
        .map(|((provider, model), (t, cost))| {
            serde_json::json!({
                "provider": provider,
                "model": model,
                "input": t.input,
                "output": t.output,
                "cacheRead": t.cache_read,
                "cacheWrite": t.cache_write,
                "messages": t.messages,
                "totalTokens": t.total(),
                "costUsd": (cost * 100.0).round() / 100.0,
            })
        })
        .collect();
    model_rows.sort_by(|a, b| {
        b["totalTokens"].as_u64().cmp(&a["totalTokens"].as_u64())
    });

    let out = serde_json::json!({
        "days": combined_days,
        "claudeDays": claude_days,
        "antigravityDays": ag_days,
        "providers": {
            "claude": {
                "todayTokens": c_today.total(),
                "todayCost": (c_cost_today * 100.0).round() / 100.0,
                "total30dTokens": c_total_30d.total(),
                "total30dCost": (c_cost_30d * 100.0).round() / 100.0,
                "output30d": c_total_30d.output,
                "messages30d": c_total_30d.messages,
            },
            "antigravity": {
                "todayTokens": ag_today.total(),
                "todayCost": (ag_cost_today * 100.0).round() / 100.0,
                "total30dTokens": ag_total_30d.total(),
                "total30dCost": (ag_cost_30d * 100.0).round() / 100.0,
                "output30d": ag_total_30d.output,
                "messages30d": ag_total_30d.messages,
            }
        },
        "models": model_rows,
    });
    if !cfg!(test) {
        if let Ok(mut c) = scan_cache().lock() {
            c.last_key = Some((home.to_path_buf(), tz_offset_min));
            c.last_at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            c.last_value = Some(out.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(civil_from_days(days_from_civil(2026, 2, 28) + 1), "2026-03-01");
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 28) + 1), "2024-02-29");
        let t = parse_ts("2026-06-22T20:12:09.862Z").unwrap();
        assert_eq!(civil_from_days(local_day_index(t, 0)), "2026-06-22");
        assert_eq!(civil_from_days(local_day_index(t, 420)), "2026-06-23");
    }

    #[test]
    fn dedups_and_persists() {
        let base = std::env::temp_dir().join(format!("cgs-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let proj = base.join(".claude/projects/p");
        fs::create_dir_all(&proj).unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let ts = {
            let d = civil_from_days(now.div_euclid(86400));
            format!("{d}T00:30:00.000Z")
        };
        let line = |id: &str, model: &str, out: u64| format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","model":"{model}","content":[{{"type":"text","text":"x"}}],"usage":{{"input_tokens":10,"output_tokens":{out},"cache_read_input_tokens":100,"cache_creation_input_tokens":5}}}}}}"#);
        fs::write(proj.join("a.jsonl"), [line("m1", "cc/claude-opus-5-5", 3), line("m1", "cc/claude-opus-5-5", 7), line("m2", "claude-sonnet-5", 1), line("m3", "<synthetic>", 9)].join("\n")).unwrap();
        fs::write(proj.join("b.jsonl"), line("m1", "claude-opus-5-5", 7)).unwrap();

        let app = base.join("app");
        let v = refresh(&base, &app, 0).unwrap();
        let days = v["days"].as_array().unwrap();
        assert_eq!(days.len(), 30);
        let last = &days[29]["models"];
        assert_eq!(last["claude-opus-5-5"]["output"], 7);
        assert_eq!(last["claude-opus-5-5"]["messages"], 1);
        assert_eq!(last["claude-sonnet-5"]["cacheRead"], 100);
        assert!(last.get("<synthetic>").is_none());

        // transcripts deleted -> history is kept
        fs::remove_dir_all(base.join(".claude")).unwrap();
        let v2 = refresh(&base, &app, 0).unwrap();
        assert_eq!(v2["days"][29]["models"]["claude-opus-5-5"]["output"], 7);
        let _ = fs::remove_dir_all(&base);
    }

    fn pb_varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 { b |= 0x80; }
            out.push(b);
            if v == 0 { break; }
        }
    }
    fn pb_msg(f: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        pb_varint(((f << 3) | 2) as u64, &mut out);
        pb_varint(payload.len() as u64, &mut out);
        out.extend_from_slice(payload);
        out
    }
    fn pb_u64(f: u32, val: u64) -> Vec<u8> {
        let mut out = Vec::new();
        pb_varint((f << 3) as u64, &mut out);
        pb_varint(val, &mut out);
        out
    }

    #[test]
    fn scans_antigravity_sqlite_steps_and_gen_metadata() {
        let base = std::env::temp_dir().join(format!("cgs-ag-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let conv = base.join(".gemini/antigravity/conversations");
        fs::create_dir_all(&conv).unwrap();
        let db = conv.join("test-conv.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)", []).unwrap();
        conn.execute("CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB)", []).unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let usage_pb = [
            pb_u64(2, 12000),
            pb_u64(5, 8000),
            pb_u64(9, 400),
            pb_u64(10, 200),
            pb_msg(7, b"bot-abc"),
            pb_msg(11, b"bot-abc"),
        ].concat();
        let chat_pb = [
            pb_msg(4, &usage_pb),
            pb_msg(19, b"gemini-4-carbon"),
            pb_msg(21, b"Gemini 4 Carbon (High)"),
        ].concat();
        let gen_pb = [pb_msg(1, &chat_pb), pb_msg(4, b"step-uuid-1")].concat();

        let step_ts = pb_u64(1, now);
        let step_bot = pb_msg(7, b"bot-abc");
        let step_pb = [pb_msg(1, &step_ts), pb_msg(9, &step_bot), pb_msg(12, b"step-uuid-1")].concat();

        conn.execute("INSERT INTO gen_metadata VALUES (0, ?1)", [&gen_pb]).unwrap();
        conn.execute("INSERT INTO gen_metadata VALUES (1, ?1)", [&gen_pb]).unwrap(); // duplicate responseId should dedup
        conn.execute("INSERT INTO steps VALUES (0, ?1)", [&step_pb]).unwrap();
        drop(conn);

        let app = base.join("app");
        let v = refresh(&base, &app, 0).unwrap();
        assert_eq!(v["providers"]["antigravity"]["total30dTokens"], 20600);
        assert_eq!(v["providers"]["antigravity"]["messages30d"], 1);
        let ag_day = &v["antigravityDays"][29]["models"]["gemini-4-carbon"];
        assert_eq!(ag_day["input"], 12000);
        assert_eq!(ag_day["cacheRead"], 8000);
        assert_eq!(ag_day["output"], 600);
        let _ = fs::remove_dir_all(&base);
    }
}
