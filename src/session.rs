//! Port of the `telegram_session` group, backend.php:554-1075 and 6324-6459:
//! `fd_get_bot_id`, `fd_clear_bot_id`, `fd_get_bot_session_path`,
//! `fd_has_local_session`, `fd_load_session_meta`, `fd_save_session_meta`,
//! `fd_clear_session_meta`, `fd_get_bot_pool`, `fd_save_bot_pool`,
//! `fd_add_pool_bot`, `fd_remove_pool_bot`, `fd_pick_pool_bot`,
//! `fd_is_guest_provision_in_progress`, `fd_auto_provision_guest`,
//! `fd_find_orphan_session_bot_id`, `fd_clear_session_directory`,
//! `fd_clear_session`, `fd_measure_clock_offset`, `fd_clock_skew_message`,
//! `fd_friendly_login_error`.
//!
//! The bot is never copied from the PHP install: provisioning asks WordPress
//! for a guest token (`GET {WP_API_BASE}/provision-session` -> `{ok, bot_token,
//! api_secret}`) and this port logs that bot in with grammers, persisting its
//! own session under `storage/sessions/<bot_id>/session.rust.json`.
//!
//! Locking: the PHP uses `flock()`. Here the same semantics (exclusive,
//! non-blocking, 30s wait, 300s staleness reclaim, heartbeat file) are
//! reproduced with an `O_EXCL` lock file and a heartbeat stamp.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use rand::RngCore;
use serde_json::Value;

use crate::config::wp_api_base;
use crate::credentials::{save_api_secret, get_api_secret};
use crate::httpc::http_json;
use crate::log::log;
use crate::storage::{now, storage_path};

/* ── identity ────────────────────────────────────────────────────────────── */

pub fn session_meta_path() -> PathBuf {
    storage_path("session_meta.json")
}

pub fn bot_pool_path() -> PathBuf {
    storage_path("bot_pool.json")
}

pub fn bot_id_cache_path() -> PathBuf {
    storage_path("bot_id.txt")
}

pub fn load_session_meta() -> Value {
    fs::read_to_string(session_meta_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}))
}

pub fn save_session_meta(bot_id: &str, bot_username: &str, bot_name: &str) {
    let body = serde_json::json!({
        "bot_id": bot_id,
        "bot_username": bot_username,
        "bot_name": bot_name,
    });
    let _ = fs::write(session_meta_path(), body.to_string());
    add_pool_bot(serde_json::json!({
        "bot_id": bot_id,
        "bot_username": bot_username,
        "bot_name": bot_name,
        "is_active": true,
        "status": "online",
        "updated_at": now(),
    }));
    clear_bot_id();
}

pub fn clear_session_meta() {
    let _ = fs::remove_file(session_meta_path());
}

/// Bot tokens supplied by the environment, in order.
///
/// `BOT_TOKENS` (plural) is a comma / semicolon / whitespace separated list;
/// `TG_BOT_TOKENS` is an alias, and the legacy single `BOT_TOKEN` /
/// `TG_BOT_TOKEN` counts as a one-element list.
///
/// Why this exists: the bot pool is per-process DISK state (`bot_pool.json`
/// plus `sessions/<bot_id>/session.rust.json`). On a PaaS with more than one
/// instance — Heroku `web.1..web.N`, Fly machines, a scaled Render service —
/// every instance has its own ephemeral disk, so a bot added through
/// `/api/bots/add` exists on exactly ONE instance. Measured on Heroku with 5
/// dynos: 14 `/api/bots` reads returned **5 distinct pools**, each holding only
/// its own auto-provisioned guest bot, and the bot the user added was visible
/// on one of them. An env list is the only bot state every instance shares.
/// Split a `BOT_TOKENS` value (comma / semicolon / any whitespace, so a
/// newline-separated `heroku config:set` paste works) and append each token not
/// already present.
fn parse_token_list(raw: &str, out: &mut Vec<String>) {
    for part in raw.split([',', ';', '\n', '\r', '\t', ' ']) {
        let token = part.trim();
        // A bot token is always `<digits>:<secret>`; requiring the colon keeps a
        // stray word in the list from being treated as a credential.
        if !token.is_empty() && token.contains(':') && !out.iter().any(|t| t == token) {
            out.push(token.to_string());
        }
    }
}

pub fn env_bot_tokens() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // `std::env::var` directly (not the `env()` helper) so an EMPTY `BOT_TOKENS`
    // cannot shadow the singular fallbacks.
    for key in ["BOT_TOKENS", "TG_BOT_TOKENS"] {
        if let Ok(raw) = std::env::var(key) {
            parse_token_list(&raw, &mut out);
        }
    }
    for key in ["BOT_TOKEN", "TG_BOT_TOKEN"] {
        if let Ok(raw) = std::env::var(key) {
            parse_token_list(&raw, &mut out);
        }
    }
    out
}

/// The env token that owns `bot_id`, so a bot listed only in `BOT_TOKENS` can be
/// logged in on demand (it has no session file on a freshly booted instance).
pub fn env_token_for(bot_id: &str) -> Option<String> {
    let id = bot_id.trim();
    if id.is_empty() {
        return None;
    }
    env_bot_tokens()
        .into_iter()
        .find(|t| t.split(':').next().unwrap_or("").trim() == id)
}

pub fn get_bot_id() -> String {
    let meta = load_session_meta();
    let from_meta = meta
        .get("bot_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !from_meta.is_empty() {
        return from_meta;
    }
    if let Some(first) = env_bot_tokens().first() {
        let id = first.split(':').next().unwrap_or("").trim();
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
            return id.to_string();
        }
    }
    let pool = read_pool_file();
    for b in &pool {
        let active = b.get("is_active").and_then(|v| v.as_bool()).unwrap_or(false);
        let id = b.get("bot_id").and_then(|v| v.as_str()).unwrap_or("");
        if active && !id.is_empty() {
            return id.to_string();
        }
    }
    if let Some(first) = pool.first() {
        let id = first.get("bot_id").and_then(|v| v.as_str()).unwrap_or("");
        if !id.is_empty() {
            return id.to_string();
        }
    }
    fs::read_to_string(bot_id_cache_path())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub fn clear_bot_id() {
    let _ = fs::remove_file(bot_id_cache_path());
}

pub fn has_local_session(bot_id: &str) -> bool {
    let id = bot_id.trim();
    if !id.is_empty() {
        return crate::telegram::has_session_file(id);
    }
    let active = get_bot_id();
    if !active.is_empty() && crate::telegram::has_session_file(&active) {
        return true;
    }
    for b in get_bot_pool() {
        let bid = b.get("bot_id").and_then(|v| v.as_str()).unwrap_or("");
        if !bid.is_empty() && crate::telegram::has_session_file(bid) {
            return true;
        }
    }
    false
}

/* ── bot pool ────────────────────────────────────────────────────────────── */

fn read_pool_file() -> Vec<Value> {
    fs::read_to_string(bot_pool_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// The on-disk pool, repaired with the active bot when `session_meta.json`
/// proves it existed. Every MUTATION uses this (never the env-merged view), so a
/// bot supplied by `BOT_TOKENS` is never written into `bot_pool.json`.
fn file_pool() -> Vec<Value> {
    let mut pool = read_pool_file();
    let active = get_bot_id();
    // Only repair a genuinely missing entry (session_meta.json on disk), exactly
    // like the PHP: never resurrect a cleared session.
    if !active.is_empty()
        && !pool
            .iter()
            .any(|b| b.get("bot_id").and_then(|v| v.as_str()) == Some(active.as_str()))
        && session_meta_path().exists()
    {
        let meta = load_session_meta();
        pool.push(serde_json::json!({
            "bot_id": active,
            "bot_username": meta.get("bot_username").and_then(|v| v.as_str()).unwrap_or(""),
            "bot_name": meta.get("bot_name").and_then(|v| v.as_str()).unwrap_or(active.as_str()),
            "status": "online",
            "added_at": now(),
            "updated_at": now(),
            "is_active": true,
        }));
        save_bot_pool(&pool);
    }
    pool
}

/// The pool as every reader sees it: the on-disk entries plus every bot the
/// environment supplies. On a multi-instance deploy this is what makes all
/// instances agree on the pool — see [`env_bot_tokens()`]. Env entries carry
/// `"is_env": true` and are NOT persisted, so unsetting the variable removes
/// them again.
pub fn get_bot_pool() -> Vec<Value> {
    let mut pool = file_pool();
    for token in env_bot_tokens() {
        let id = token.split(':').next().unwrap_or("").trim().to_string();
        if id.is_empty() {
            continue;
        }
        if pool
            .iter()
            .any(|b| b.get("bot_id").and_then(|v| v.as_str()) == Some(id.as_str()))
        {
            continue;
        }
        pool.push(serde_json::json!({
            "bot_id": id,
            "bot_username": "",
            "bot_name": format!("Bot {}", id),
            "status": "online",
            "source": "env",
            "is_env": true,
            "added_at": now(),
            "updated_at": now(),
        }));
    }
    pool
}

pub fn save_bot_pool(pool: &[Value]) {
    let body = serde_json::to_string_pretty(&Value::Array(pool.to_vec()))
        .unwrap_or_else(|_| "[]".to_string());
    let _ = fs::write(bot_pool_path(), body);
}

pub fn add_pool_bot(bot_data: Value) -> Vec<Value> {
    let bot_id = bot_data.get("bot_id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if bot_id.is_empty() {
        return Vec::new();
    }
    let mut pool = file_pool();
    let mut found = false;
    for item in pool.iter_mut() {
        if item.get("bot_id").and_then(|v| v.as_str()) == Some(bot_id.as_str()) {
            if let (Some(dst), Some(src)) = (item.as_object_mut(), bot_data.as_object()) {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
            found = true;
            break;
        }
    }
    if !found {
        let mut entry = serde_json::json!({
            "bot_id": bot_id,
            "bot_username": "",
            "bot_name": "",
            "status": "online",
            "added_at": now(),
            "updated_at": now(),
        });
        if let (Some(dst), Some(src)) = (entry.as_object_mut(), bot_data.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        pool.push(entry);
    }
    save_bot_pool(&pool);
    pool
}

/// Record a successfully booted env-supplied bot's REAL identity, so the pool
/// list shows `@username` instead of the placeholder name derived from its id
/// ("Bot 8990816622"). `get_bot_pool()` only fills that placeholder for an env
/// bot that has no on-disk entry yet, and this is what creates one.
pub fn remember_env_identity(bot_id: &str, username: &str, name: &str) {
    let id = bot_id.trim();
    if id.is_empty() {
        return;
    }
    add_pool_bot(serde_json::json!({
        "bot_id": id,
        "bot_username": username,
        "bot_name": if name.is_empty() { username } else { name },
        "status": "online",
        "source": "env",
        "is_env": true,
        "updated_at": now(),
    }));
}

pub fn remove_pool_bot(bot_id: &str) -> Vec<Value> {
    let id = bot_id.trim();
    if id.is_empty() {
        return get_bot_pool();
    }
    // File pool only: an env-supplied bot has no file entry, and filtering the
    // merged view would persist the env entries into `bot_pool.json`.
    let next: Vec<Value> = file_pool()
        .into_iter()
        .filter(|b| b.get("bot_id").and_then(|v| v.as_str()) != Some(id))
        .collect();
    save_bot_pool(&next);
    clear_session_directory(&crate::telegram::session_dir(id));
    let active = get_bot_id();
    if active == id {
        if let Some(first) = next.first() {
            save_session_meta(
                first.get("bot_id").and_then(|v| v.as_str()).unwrap_or(""),
                first.get("bot_username").and_then(|v| v.as_str()).unwrap_or(""),
                first.get("bot_name").and_then(|v| v.as_str()).unwrap_or(""),
            );
        } else {
            clear_session_meta();
            clear_session();
        }
    }
    if next.is_empty() {
        let _ = fs::remove_file(bot_pool_path());
        // With `BOT_TOKENS` set the machine still has bots, so promote the first
        // env bot instead of leaving the instance with no active session.
        match env_bot_tokens().first() {
            Some(token) => {
                let id = token.split(':').next().unwrap_or("").trim().to_string();
                save_session_meta(&id, "", "");
            }
            None => {
                clear_session_meta();
                clear_session();
            }
        }
    }
    get_bot_pool()
}

pub fn filter_and_pick_pool_bot<'a>(
    pool: &'a [Value],
    ts: i64,
    has_session: impl Fn(&str) -> bool,
    idx: usize,
) -> Option<(&'a Value, usize)> {
    let valid: Vec<&Value> = pool
        .iter()
        .filter(|b| {
            let id = b.get("bot_id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return false;
            }
            let cooldown = b.get("cooldown_until").and_then(|v| v.as_i64()).unwrap_or(0);
            if cooldown > ts {
                return false;
            }
            has_session(id)
        })
        .collect();
    if valid.is_empty() {
        return None;
    }
    let picked_idx = idx % valid.len();
    let next_idx = (idx + 1) % valid.len();
    Some((valid[picked_idx], next_idx))
}

pub fn pick_pool_bot() -> Option<Value> {
    let pool = get_bot_pool();
    let active = get_bot_id();
    let ts = now();
    let rr = storage_path("rr_bot_index.txt");
    let idx: usize = fs::read_to_string(&rr)
        .ok()
        .and_then(|t| t.trim().parse::<usize>().ok())
        .unwrap_or(0);

    if let Some((picked, next)) = filter_and_pick_pool_bot(&pool, ts, crate::telegram::has_session_file, idx) {
        let _ = fs::write(&rr, next.to_string());
        return Some(picked.clone());
    }

    if !active.is_empty() {
        let meta = load_session_meta();
        return Some(serde_json::json!({
            "bot_id": active,
            "bot_username": meta.get("bot_username").and_then(|v| v.as_str()).unwrap_or(""),
            "bot_name": meta.get("bot_name").and_then(|v| v.as_str()).unwrap_or(""),
        }));
    }
    None
}

/// Exactly ONE guest bot is leased per instance — no more, and there is **no
/// knob** to raise it.
///
/// Every guest bot is a lease from the origin's shared pool
/// (`provision-session`, `<20` provisions per bot, 300 candidates ≈ 6 000
/// leases total). A per-instance multiplier would therefore let anyone drain that
/// budget just by scaling out or by setting an environment variable, so the limit
/// is a constant rather than a setting.
///
/// A user who wants a bigger pool adds **their own bots** — `BOT_TOKENS` in the
/// environment, or the dashboard's Bot Pool UI — which costs the origin nothing.
/// Their bots count towards this same one-bot check, so adding one means no guest
/// is leased at all.
pub const GUEST_BOTS_PER_INSTANCE: usize = 1;

/* ── login flood guard ───────────────────────────────────────────────────── */

/// When Telegram rate-limited us, and until when — kept in the server's own cache
/// (`storage/cache.redb`, see [`crate::cache`]), not in a file of its own.
///
/// Telegram answers `auth.importBotAuthorization` with `420 FLOOD_WAIT` after too
/// many logins, and **every further attempt re-arms the wait**. A public addon is
/// the worst case for that: a client that retries (a player reopening a stream,
/// a dozen viewers at once) turns one throttle into a permanent lockout.
/// Measured on this deployment: 6 bots x 5 dynos logging in on one restart
/// produced `FLOOD_WAIT (value: 455..961)` and every subsequent
/// `/api/download` answered 403 for ~15 minutes.
///
/// The entry's TTL is the wait itself, so it expires by construction and nothing
/// has to clean it up.
const FLOOD_NS: &str = "telegram_flood";
const FLOOD_KEY: &str = "global";

/// Seconds left on the recorded flood wait (0 = nothing recorded / expired).
pub fn flood_remaining() -> i64 {
    crate::cache::get(FLOOD_NS, FLOOD_KEY)
        .and_then(|v| v.get("until").and_then(|u| u.as_i64()))
        .map(|until| (until - now()).max(0))
        .unwrap_or(0)
}

/// The Telegram error text that produced the recorded wait, verbatim.
///
/// Kept so a refused login can quote Telegram instead of inventing its own
/// wording — "FLOOD_WAIT ... (value: 460)" is what an operator can act on.
pub fn flood_last_error() -> String {
    crate::cache::get(FLOOD_NS, FLOOD_KEY)
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()))
        .unwrap_or_default()
}

/// Record a `FLOOD_WAIT` seen in a boot error. Returns the seconds recorded
/// (0 when the error was not a flood wait). Never shortens an existing wait.
pub fn note_flood(error: &str) -> i64 {
    let secs = parse_flood_seconds(error);
    if secs <= 0 {
        return 0;
    }
    if secs <= flood_remaining() {
        return secs;
    }
    let body = serde_json::json!({
        "until": now() + secs,
        "seconds": secs,
        "recorded_at": now(),
        "error": error,
    });
    crate::cache::put(FLOOD_NS, FLOOD_KEY, &body, secs, secs);
    log("telegram login flood wait recorded", &body);
    secs
}

/// `(value: 460)`, `value: 460`, `FLOOD_WAIT_460` — grammers/Telegram render the
/// wait in all three shapes.
fn parse_flood_seconds(error: &str) -> i64 {
    let lower = error.to_ascii_lowercase();
    if !lower.contains("flood_wait") && !lower.contains("flood wait") {
        return 0;
    }
    let digits_after = |needle: &str| -> i64 {
        if let Some(pos) = lower.find(needle) {
            let rest = &lower[pos + needle.len()..];
            let mut num = String::new();
            for ch in rest.chars() {
                if ch.is_ascii_digit() {
                    num.push(ch);
                } else if !num.is_empty() {
                    break;
                } else if ch != '_' && ch != ' ' && ch != ':' && ch != '(' {
                    break;
                }
            }
            num.parse::<i64>().unwrap_or(0)
        } else {
            0
        }
    };
    let mut secs = digits_after("value:");
    if secs == 0 {
        secs = digits_after("flood_wait_");
    }
    if secs == 0 {
        // Last resort: the first number after the FLOOD_WAIT marker.
        if let Some(pos) = lower.find("flood_wait") {
            let rest = &lower[pos..];
            let num: String = rest
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            secs = num.parse::<i64>().unwrap_or(0);
        }
    }
    if secs > 0 {
        secs.min(86_400)
    } else {
        0
    }
}

/* ── guest provisioning ──────────────────────────────────────────────────── */

fn provision_lock_path() -> PathBuf {
    storage_path("guest_provision.lock")
}

fn provision_heartbeat_path() -> PathBuf {
    storage_path("guest_provision.heartbeat")
}

/// `fd_is_guest_provision_in_progress()`: stale locks (>300s) are reclaimed.
pub fn is_guest_provision_in_progress() -> bool {
    let file = provision_lock_path();
    let meta = match fs::metadata(&file) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let age = crate::storage::mtime_secs(&file)
        .map(|m| now() - m)
        .unwrap_or(0);
    let _ = meta;
    if age > 300 {
        log("reclaimed stale guest_provision.lock", &serde_json::json!({"age": age}));
        let _ = fs::remove_file(&file);
        return false;
    }
    if let Ok(hb) = fs::read_to_string(provision_heartbeat_path()) {
        if let Ok(v) = hb.trim().parse::<i64>() {
            if now() - v < 30 {
                return true;
            }
        }
    }
    age < 120
}

struct ProvisionLock;

impl ProvisionLock {
    async fn acquire(wait_ms: u64) -> Option<ProvisionLock> {
        let file = provision_lock_path();
        let start = std::time::Instant::now();
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(&file) {
                Ok(mut f) => {
                    let _ = f.write_all(b"1");
                    let _ = fs::write(provision_heartbeat_path(), now().to_string());
                    return Some(ProvisionLock);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(m) = crate::storage::mtime_secs(&file) {
                        if now() - m > 300 {
                            let _ = fs::remove_file(&file);
                            continue;
                        }
                    }
                    if start.elapsed().as_millis() as u64 > wait_ms {
                        return None;
                    }
                    let _ = fs::write(provision_heartbeat_path(), now().to_string());
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(_) => return None,
            }
        }
    }
}

impl Drop for ProvisionLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(provision_lock_path());
        let _ = fs::remove_file(provision_heartbeat_path());
    }
}

pub struct ClockProbe {
    pub ok: bool,
    pub offset: i64,
    pub server_time: i64,
    pub local_time: i64,
    pub error: Option<String>,
}

fn parse_http_date(raw: &str) -> Option<i64> {
    // RFC 7231 IMF-fixdate: "Mon, 20 Oct 2026 00:32:39 GMT" — the FIRST token
    // is the weekday, so it must be skipped (reading parts[0] as the day made
    // every probe report "clock probe unreachable").
    let mut parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() >= 6 && parts[0].ends_with(',') {
        parts.remove(0);
    }
    let parts = parts;
    if parts.len() < 5 {
        return None;
    }
    let day: i64 = parts[0].parse().ok()?;
    let month = match parts[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[2].parse().ok()?;
    let time: Vec<&str> = parts[3].split(':').collect();
    if time.len() != 3 {
        return None;
    }
    let hh: i64 = time[0].parse().ok()?;
    let mm: i64 = time[1].parse().ok()?;
    let ss: i64 = time[2].parse().ok()?;
    Some(days_from_civil(year, month, day) * 86400 + hh * 3600 + mm * 60 + ss)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn date_of(resp: &reqwest::Response) -> Option<i64> {
    let date = resp.headers().get(reqwest::header::DATE)?;
    parse_http_date(date.to_str().ok()?)
}

/// `fd_measure_clock_offset()`: `Date` header vs local time.
pub async fn measure_clock_offset() -> ClockProbe {
    let local = now();
    for url in ["https://pencarimovie.com/", "https://core.telegram.org/"] {
        // HEAD is cheap, but some edges answer it without a Date header, so a
        // GET is tried before giving up on the host.
        for use_head in [true, false] {
            let resp = {
                let c = crate::httpc::client();
                let req = if use_head { c.head(url) } else { c.get(url) };
                req.timeout(Duration::from_secs(4)).send().await
            };
            if let Ok(resp) = resp {
                if let Some(server) = date_of(&resp) {
                    return ClockProbe {
                        ok: true,
                        // PHP: `offset = $localTime - $serverTime` — POSITIVE
                        // means the local clock is AHEAD. Subtracting the other
                        // way round flips the "ahead of"/"behind" wording of
                        // clock_skew_message(), and a device 40 s BEHIND was
                        // told it was ahead — the one fact the user cannot
                        // check for themselves. Pinned by a unit test.
                        offset: now() - server,
                        server_time: server,
                        local_time: now(),
                        error: None,
                    };
                }
            }
        }
    }
    ClockProbe {
        ok: false,
        offset: 0,
        server_time: 0,
        local_time: local,
        error: Some("clock probe unreachable".to_string()),
    }
}

pub fn clock_skew_message(offset_seconds: i64) -> String {
    let off = offset_seconds.abs();
    let mins = off / 60;
    let secs = off % 60;
    let human = if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    };
    // offset is `local - server` (PHP), so positive = local is ahead.
    let dir = if offset_seconds > 0 { "ahead of" } else { "behind" };
    format!("Your clock is {} {} Telegram's server time.", human, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sign convention is PHP's (`local - server`) and it drives the only
    /// user-facing sentence here, so it is worth pinning: getting it backwards
    /// tells a fast clock it is slow.
    #[test]
    fn clock_skew_message_says_ahead_for_a_fast_clock() {
        assert!(clock_skew_message(40).contains("40s ahead of"));
        assert!(clock_skew_message(-40).contains("40s behind"));
        assert!(clock_skew_message(72).contains("1m 12s ahead of"));
        assert_eq!(
            clock_skew_message(-432),
            "Your clock is 7m 12s behind Telegram's server time."
        );
    }

    /// The guest-bot limit is a constant, not a setting: raising it must stay a
    /// code change, so nobody can farm the origin's provisioning from a config var.
    #[test]
    fn one_guest_bot_per_instance_is_a_constant() {
        assert_eq!(GUEST_BOTS_PER_INSTANCE, 1);
    }

    /// Telegram renders the login wait in several shapes, and a missed parse means
    /// the guard does nothing while the addon keeps re-arming the throttle.
    #[test]
    fn flood_wait_is_parsed_from_every_shape() {
        assert_eq!(
            parse_flood_seconds(
                "bot sign-in failed: request error: rpc error 420: FLOOD_WAIT caused by \
                 auth.importBotAuthorization (value: 460)"
            ),
            460
        );
        assert_eq!(parse_flood_seconds("FLOOD_WAIT_120"), 120);
        assert_eq!(parse_flood_seconds("rpc error 420: FLOOD_WAIT 45"), 45);
        assert_eq!(parse_flood_seconds("bot sign-in failed: invalid token"), 0);
        assert_eq!(parse_flood_seconds(""), 0);
    }

    /// The recorded wait must actually gate the next login — that is the whole
    /// point of the guard (a public addon otherwise retries itself into a lockout).
    #[test]
    fn a_recorded_flood_wait_blocks_the_next_login() {
        assert_eq!(flood_remaining(), 0);
        let secs = note_flood(
            "rpc error 420: FLOOD_WAIT caused by auth.importBotAuthorization (value: 30)",
        );
        assert_eq!(secs, 30);
        assert!(flood_remaining() > 0, "a recorded wait must gate the next login");
        assert!(flood_remaining() <= 30);
    }

    /// `BOT_TOKENS` is the only bot state every instance of a scaled-out deploy
    /// shares, so the parsing has to survive whatever an operator pastes into a
    /// PaaS config var: commas, spaces, newlines, duplicates, and the usual
    /// accidental junk.
    #[test]
    fn bot_token_list_parses_every_separator_and_dedupes() {
        let mut out = Vec::new();
        parse_token_list(
            "111:AAA, 222:BBB\n333:CCC\t111:AAA  ,\n  ;444:DDD",
            &mut out,
        );
        assert_eq!(out, vec!["111:AAA", "222:BBB", "333:CCC", "444:DDD"]);

        // A word without the `id:secret` colon is not a credential.
        let mut out = Vec::new();
        parse_token_list("BOT_TOKEN 555:EEE notatoken", &mut out);
        assert_eq!(out, vec!["555:EEE"]);

        let mut out = Vec::new();
        parse_token_list("", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn round_robin_spreads_across_valid_pool_bots() {
        let pool = vec![
            serde_json::json!({"bot_id": "111", "cooldown_until": 0}),
            serde_json::json!({"bot_id": "222", "cooldown_until": 0}),
            serde_json::json!({"bot_id": "333", "cooldown_until": 0}),
        ];
        let has_session = |_id: &str| true;
        let ts = 1000;

        let (first, next1) = filter_and_pick_pool_bot(&pool, ts, has_session, 0).unwrap();
        assert_eq!(first["bot_id"], "111");
        assert_eq!(next1, 1);

        let (second, next2) = filter_and_pick_pool_bot(&pool, ts, has_session, next1).unwrap();
        assert_eq!(second["bot_id"], "222");
        assert_eq!(next2, 2);

        let (third, next3) = filter_and_pick_pool_bot(&pool, ts, has_session, next2).unwrap();
        assert_eq!(third["bot_id"], "333");
        assert_eq!(next3, 0);

        // Wraps back to first bot
        let (fourth, next4) = filter_and_pick_pool_bot(&pool, ts, has_session, next3).unwrap();
        assert_eq!(fourth["bot_id"], "111");
        assert_eq!(next4, 1);
    }

    #[test]
    fn round_robin_skips_cooldown_and_sessionless_bots() {
        let ts = 1000;
        let pool = vec![
            serde_json::json!({"bot_id": "111", "cooldown_until": 2000}), // on cooldown (2000 > 1000)
            serde_json::json!({"bot_id": "222", "cooldown_until": 0}),    // valid
            serde_json::json!({"bot_id": "333", "cooldown_until": 0}),    // has no session
            serde_json::json!({"bot_id": "444", "cooldown_until": 0}),    // valid
        ];
        let has_session = |id: &str| id != "333";

        let (first, next1) = filter_and_pick_pool_bot(&pool, ts, has_session, 0).unwrap();
        assert_eq!(first["bot_id"], "222");
        assert_eq!(next1, 1);

        let (second, next2) = filter_and_pick_pool_bot(&pool, ts, has_session, next1).unwrap();
        assert_eq!(second["bot_id"], "444");
        assert_eq!(next2, 0);

        // Wraps between 222 and 444 only
        let (third, next3) = filter_and_pick_pool_bot(&pool, ts, has_session, next2).unwrap();
        assert_eq!(third["bot_id"], "222");
        assert_eq!(next3, 1);
    }

}

pub fn is_clock_or_authkey_error(message: &str) -> bool {
    let s = message.to_ascii_lowercase();
    if s.is_empty() {
        return false;
    }
    if s.contains("session is busy") || s.contains("lock") || s.contains("econnrefused") {
        return false;
    }
    s.contains("auth_key")
        || s.contains("new_nonce_hash")
        || s.contains("msg_id too")
        || s.contains("message id")
        || s.contains("time delta")
        || s.contains("clock")
        || s.contains("sync your date")
}

pub fn friendly_login_error(message: &str) -> String {
    if is_clock_or_authkey_error(message) {
        return "Your device clock is out of sync with Telegram.\nEnable 'Set time automatically' (Automatic date and time / NTP) in your device Settings, then restart the PencariMovie Server.".to_string();
    }
    message.to_string()
}

pub fn find_orphan_session_bot_id() -> String {
    let dir = storage_path("sessions");
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if crate::telegram::has_session_file(&name) {
                return name;
            }
        }
    }
    String::new()
}

pub fn clear_session_directory(dir: &std::path::Path) {
    let _ = fs::remove_dir_all(dir);
}

pub fn clear_session() {
    let _ = fs::remove_dir_all(storage_path("sessions"));
    // Legacy PHP (MadelineProto) session directory, if this install ever had one.
    let _ = fs::remove_dir_all(storage_path("session.madeline"));
    crate::telegram::disconnect_all();
}

pub struct ProvisionResult {
    pub bot_id: String,
    pub bot_username: String,
    pub bot_name: String,
    pub error: Option<String>,
}

impl ProvisionResult {
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            bot_id: String::new(),
            bot_username: String::new(),
            bot_name: String::new(),
            error: Some(msg.into()),
        }
    }
}

/// PHP truthiness for a JSON scalar (`"ok": 1` counts as true).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0 || n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => !s.is_empty() && s != "0" && s != "false",
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Null) | None => false,
    }
}

fn random_suffix() -> String {
    let mut buf = [0u8; 8];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// `fd_auto_provision_guest()` (backend.php:976): same order of checks, same
/// error strings, same "try twice" loop — but the login is this stack's own.
pub async fn auto_provision_guest(
    tg: &crate::telegram::TelegramPool,
) -> Result<ProvisionResult, ProvisionResult> {
    // ── login flood guard ─────────────────────────────────────────────────
    // Never attempt a login while Telegram's FLOOD_WAIT is still running: the
    // attempt would fail AND push the wait further out, which is how a retrying
    // public addon locks itself out. Answer instantly instead.
    let flood = flood_remaining();
    if flood > 0 {
        let last = flood_last_error();
        return Err(ProvisionResult::error(if last.is_empty() {
            format!(
                "Telegram rate limit: bot login paused for another {}s. Please retry shortly.",
                flood
            )
        } else {
            // Quote Telegram, do not paraphrase it.
            format!(
                "Telegram rate limit: bot login paused for another {}s.\nTelegram said: {}",
                flood, last
            )
        }));
    }

    // ── clock pre-check ───────────────────────────────────────────────────
    let clock = measure_clock_offset().await;
    if clock.ok && clock.offset.abs() > 30 {
        log(
            "auto provision aborted: clock skew too large",
            &serde_json::json!({"offset_seconds": clock.offset}),
        );
        let human = clock_skew_message(clock.offset);
        return Err(ProvisionResult::error(format!(
            "Device clock out of sync!\n{}\n\nEnable 'Set time automatically' (Automatic date and time / NTP) in your device Settings, then restart the PencariMovie Server.",
            human
        )));
    }

    // ── existing session ──────────────────────────────────────────────────
    let existing = get_bot_id();
    if !existing.is_empty() && crate::telegram::has_session_file(&existing) {
        let meta = load_session_meta();
        match tg.boot(&existing, None).await {
            Ok(_) => {
                return Ok(ProvisionResult {
                    bot_id: existing,
                    bot_username: meta.get("bot_username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    bot_name: meta.get("bot_name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    error: None,
                })
            }
            Err(e) => {
                if is_clock_or_authkey_error(&e) {
                    return Err(ProvisionResult::error(friendly_login_error(&e)));
                }
                log(
                    "existing session boot failed",
                    &serde_json::json!({"bot_id": existing, "error": e}),
                );
            }
        }
    }

    if existing.is_empty() {
        let orphan = find_orphan_session_bot_id();
        if !orphan.is_empty() {
            log(
                "auto provision aborted: orphan session dir present without bot_id",
                &serde_json::json!({"orphan_bot_id": orphan}),
            );
            return Err(ProvisionResult::error(
                "A previous login did not finish. Please restart the PencariMovie Server and try again.",
            ));
        }
    }

    let _lock = match ProvisionLock::acquire(30_000).await {
        Some(l) => l,
        None => {
            log(
                "auto provision skipped: another provision is currently in progress",
                &serde_json::json!({}),
            );
            return Err(ProvisionResult::error(
                "Connecting guest bot in progress. Please refresh or try again in a few seconds.",
            ));
        }
    };

    // Re-check after acquiring the lock (another worker may have finished).
    let recheck = get_bot_id();
    if !recheck.is_empty() && crate::telegram::has_session_file(&recheck) {
        let meta = load_session_meta();
        if tg.boot(&recheck, None).await.is_ok() {
            return Ok(ProvisionResult {
                bot_id: recheck,
                bot_username: meta.get("bot_username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                bot_name: meta.get("bot_name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                error: None,
            });
        }
    }

    // Env-supplied tokens win when present (container / multi-instance installs).
    // Every token is tried in order: on a PaaS each instance must be able to log
    // in pool bots it did not add itself, and one expired token must not take the
    // whole pool down.
    let mut env_err: Option<String> = None;
    for token in env_bot_tokens() {
        let id_part = token.split(':').next().unwrap_or("").trim().to_string();
        let target = if !id_part.is_empty() && id_part.chars().all(|c| c.is_ascii_digit()) {
            id_part
        } else {
            format!("env_{}", random_suffix())
        };
        match tg.boot(&target, Some(&token)).await {
            Ok(entry) => {
                remember_env_identity(&entry.identity.id, &entry.identity.username, &entry.identity.first_name);
                save_session_meta(&entry.identity.id, &entry.identity.username, &entry.identity.first_name);
                let meta = load_session_meta();
                return Ok(ProvisionResult {
                    bot_id: meta.get("bot_id").and_then(|v| v.as_str()).unwrap_or(&entry.identity.id).to_string(),
                    bot_username: meta.get("bot_username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    bot_name: meta.get("bot_name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    error: None,
                });
            }
            Err(e) => {
                note_flood(&e);
                log(
                    "env bot token failed to boot",
                    &serde_json::json!({"bot_id": target, "error": e}),
                );
                env_err = Some(format!("bot {}: {}", target, e));
            }
        }
    }

    // The origin keeps its own pool of bots and hands out the least-used one on
    // every `provision-session` call, so leasing is also how a multi-instance
    // deploy builds a pool without anyone handling tokens — see
    // `ensure_pool_size()`.
    match lease_guest_bot(tg).await {
        Ok(res) => {
            save_session_meta(&res.bot_id, &res.bot_username, &res.bot_name);
            log(
                "guest bot provisioned",
                &serde_json::json!({"bot_id": res.bot_id, "username": res.bot_username}),
            );
            Ok(res)
        }
        Err(e) => Err(ProvisionResult::error(match env_err {
            // Report BOTH sides: the origin lease failure and why the pool bots
            // could not be used (usually a Telegram FLOOD_WAIT).
            Some(env) => format!("{} (pool bots: {})", e, env),
            None => e,
        })),
    }
}

/// One lease from the origin's bot pool: `GET /provision-session`, then log that
/// bot in here. Never touches `session_meta.json` — the caller decides which bot
/// becomes the active one (provisioning takes the first, `ensure_pool_size()`
/// keeps the bot that is already active).
async fn lease_guest_bot(tg: &crate::telegram::TelegramPool) -> Result<ProvisionResult, String> {
    let mut last_error: Option<String> = None;
    for attempt in 0..2 {
        let url = format!(
            "{}/provision-session?_nocache={}_{}",
            wp_api_base(),
            now(),
            (rand::random::<u32>() % 9000) + 1000
        );
        let resp = http_json(&url, &[], "GET", 6).await;
        // WordPress answers `"ok": 1` (a NUMBER) here, not `true`; `as_bool()`
        // alone silently rejected every valid provision response.
        let ok = truthy(resp.get("ok"));
        let token = resp.get("bot_token").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if !ok || token.is_empty() {
            // Surface what the origin actually said. This used to be logged only,
            // so every failure reached the player as the useless
            // "Could not obtain guest bot session from server." — including
            // "No eligible guest bot available (<10 subscribers, <20 provisions)"
            // and a Cloudflare/WP error page, which need completely different
            // reactions. `message` is the plugin's own error text.
            let detail = resp
                .get("message")
                .and_then(|m| m.as_str())
                .map(|m| m.trim().to_string())
                .unwrap_or_default();
            last_error = Some(if detail.is_empty() {
                format!(
                    "provision-session returned {}",
                    if ok { "no bot_token" } else { "ok=0" }
                )
            } else {
                format!("provision-session: {}", detail)
            });
            log(
                "provision endpoint returned error or incomplete payload",
                &serde_json::json!({"resp": resp, "attempt": attempt + 1}),
            );
            continue;
        }
        if let Some(secret) = resp.get("api_secret").and_then(|v| v.as_str()) {
            save_api_secret(secret);
        }
        let target = match resp.get("bot_id") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            Some(Value::Number(n)) => n.to_string(),
            _ => format!("prov_{}", random_suffix()),
        };

        match tg.boot(&target, Some(&token)).await {
            Ok(entry) => {
                let real_id = if entry.identity.id.is_empty() {
                    target.clone()
                } else {
                    entry.identity.id.clone()
                };
                return Ok(ProvisionResult {
                    bot_id: real_id,
                    bot_username: entry.identity.username.clone(),
                    bot_name: entry.identity.first_name.clone(),
                    error: None,
                });
            }
            Err(e) => {
                note_flood(&e);
                log(
                    "auto provision login failed",
                    &serde_json::json!({"error": e, "bot_id": target, "attempt": attempt + 1}),
                );
                if is_clock_or_authkey_error(&e) {
                    return Err(friendly_login_error(&e));
                }
                last_error = Some(e);
            }
        }
    }
    Err(last_error
        .unwrap_or_else(|| "Could not obtain guest bot session from server.".to_string()))
}

/// Make sure this instance holds at least [`GUEST_BOTS_PER_INSTANCE`] bot with a
/// usable session, leasing ONE guest bot from the origin when it does not.
///
/// Why per instance and not shared: `bot_pool.json` and `sessions/<bot_id>/` are
/// per-instance disk state, and the download candidate filter skips a pool bot
/// whose session lives on another instance. A pooled fleet therefore aggregates
/// one bot per instance (measured on Heroku: 5 dynos, 14 `/api/bots` reads, 5
/// distinct pools of one bot each), and nobody can multiply that from a config
/// var — see [`GUEST_BOTS_PER_INSTANCE`].
///
/// Returns how many bots the pool holds with a usable session.
pub async fn ensure_guest_bot(tg: &crate::telegram::TelegramPool) -> usize {
    let count = || {
        get_bot_pool()
            .iter()
            .filter(|b| {
                let id = b.get("bot_id").and_then(|v| v.as_str()).unwrap_or("");
                !id.is_empty() && crate::telegram::has_session_file(id)
            })
            .count()
    };
    // A user-supplied bot (env or dashboard) satisfies the requirement, so no
    // guest lease is spent.
    if count() >= GUEST_BOTS_PER_INSTANCE {
        return count();
    }
    // The same lock provisioning uses, so a startup top-up and a
    // request-triggered provision can never log in bots concurrently.
    let _lock = match ProvisionLock::acquire(60_000).await {
        Some(l) => l,
        None => return count(),
    };
    // Re-check after the lock: a concurrent provision may have just satisfied it.
    if count() >= GUEST_BOTS_PER_INSTANCE {
        return count();
    }
    let had_active = !get_bot_id().is_empty();
    match lease_guest_bot(tg).await {
        Ok(res) => {
            add_pool_bot(serde_json::json!({
                "bot_id": res.bot_id,
                "bot_username": res.bot_username,
                "bot_name": res.bot_name,
                "status": "online",
                "is_active": !had_active,
                "updated_at": now(),
            }));
            if !had_active {
                save_session_meta(&res.bot_id, &res.bot_username, &res.bot_name);
            }
            log(
                "pool leased guest bot",
                &serde_json::json!({"bot_id": res.bot_id, "bots": count()}),
            );
        }
        Err(e) => log("pool lease failed", &serde_json::json!({"error": e})),
    }
    count()
}

/// Only used by `/api/provision` to echo the secret; kept here so the route has
/// one import.
pub fn api_secret_for_response() -> String {
    get_api_secret()
}
