//! Port of the `resolve` group, backend.php:3397-3967 and 4196-4470:
//! `fd_resolve_cache_store`, `fd_resolve_shortcode_cached`,
//! `fd_save_resolve_cache`, `fd_invalidate_resolve_cache`,
//! `fd_resolve_shortcode_fresh`, `fd_resolve_shortcode_concurrent`,
//! `fd_resolve_shortcode`, `fd_resolve_shortcodes_batch`,
//! `fd_warmup_resolve_batch`.
//!
//! The same 2h expiry (Telegram `file_reference` tokens die), the same 10 s fresh
//! memo, the same "poll 3x with force=1&nocache=1" retry ladder and the same
//! "not found for this request" short-circuit.
//!
//! Storage is [`crate::cache`] (L1 memory + redb), keyed `resolve` / `resolve_fresh`
//! on `short_code:bot_id` — a file id is **bot-specific**, so the bot is part of
//! the key. The old `resolve_cache_<md5>.json` files and the unbounded
//! `Mutex<HashMap>` are gone (the latter never evicted).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;

use crate::cache;
use crate::config::wp_api_base;
use crate::httpc::http_json;
use crate::log::log;
use crate::session::{get_bot_id, get_bot_pool, pick_pool_bot};
use crate::storage::now;

/// Freshness window for a resolved `file_id_mt` (the embedded `file_reference`
/// dies on Telegram's side; the download path re-resolves on expiry).
///
/// 2 h was wrong. Measured on 2026-10-24: every Eclipse track logged
/// `FILE_REFERENCE_EXPIRED (400)` on its FIRST byte, then a 7-20 s re-resolve the
/// browser never waited for, so playback died while the origin resolved the same
/// code instantly. A reference lives ~1 h, so the cache has to expire sooner than
/// it does: 50 min keeps a hit usable and still saves the resolve on the next tap.
const RESOLVE_TTL: i64 = 3000;
/// The "one refresh per burst" memo the player's parallel Range requests share.
const FRESH_MEMO_TTL: i64 = 10;
/// How long a FAILED prewarm resolve suppresses a retry for that code.
///
/// Without it every `/stream` re-attempted the same 12 unresolvable codes: the
/// stress run recorded ~4 400 L1 misses and ~1 700 background resolve attempts in
/// one 12 s pass, all for codes the origin had already refused, and the extra
/// origin traffic is exactly what makes a Play slow. 60 s is short enough that a
/// newly indexed file still warms on the next browse.
const RESOLVE_MISS_TTL: i64 = 60;

fn miss_key(short_code: &str, bot_id: &str) -> String {
    cache_key(short_code, bot_id)
}

fn cache_key(short_code: &str, bot_id: &str) -> String {
    format!("{}:{}", short_code, bot_id)
}

fn has_id(v: &Value) -> bool {
    let a = v.get("file_id_mt").and_then(|x| x.as_str()).unwrap_or("");
    let b = v.get("file_id").and_then(|x| x.as_str()).unwrap_or("");
    !a.is_empty() || !b.is_empty()
}

/// Only a resolved id is worth keeping: an error body must never be cached, or the
/// next request would replay it for two hours.
pub fn resolve_shortcode_cached(short_code: &str, bot_id: &str) -> Option<Value> {
    let key = cache_key(short_code, bot_id);
    let hit = cache::get("resolve", &key)?;
    // Two origin answers carry an id but NOT the catalog metadata:
    //   • `/resolve-files` (batch) — `ok/short_code/file_id_mt/bot_id` only;
    //   • `/resolve-file`'s stale-refresh branch — `refreshed:1` + the id, no
    //     `title`.
    // Cached as a "resolve" they are ids with nothing attached, and every
    // metadata consumer breaks: `/eclipse/stream/<code>` answered
    // `{"error":"Track not found"}` in **4.6 ms** for a code the origin resolves
    // perfectly. A real resolve always knows the size AND the file name, so
    // anything less is a MISS and gets resolved again.
    if has_id(&hit) && file_size_of(&hit) > 0 && !jstr(&hit, "title").is_empty() {
        Some((*hit).clone())
    } else {
        None
    }
}

fn jstr(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").trim().to_string()
}

fn file_size_of(v: &Value) -> u64 {
    v.get("file_size")
        .and_then(|x| match x {
            Value::Number(n) => n.as_u64().or_else(|| n.as_i64().map(|i| i.max(0) as u64)),
            Value::String(s) => s.trim().parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

pub fn save_resolve_cache(short_code: &str, bot_id: &str, data: &Value) {
    if !has_id(data) {
        return;
    }
    // The single choke point for every writer, so a metadata-less id can never
    // be persisted as a resolve (see `resolve_shortcode_cached`). The id is
    // still returned to the caller and the origin keeps it in `tg_file_id`, so
    // the next single resolve is instant.
    if file_size_of(data) == 0 || jstr(data, "title").is_empty() {
        log(
            "not caching an id-only answer as a resolve",
            &serde_json::json!({
                "short_code": short_code,
                "bot_id": bot_id,
                "has_size": file_size_of(data) > 0,
                "has_title": !jstr(data, "title").is_empty(),
            }),
        );
        return;
    }
    // Stamp WHEN the id was minted. A Telegram `file_reference` lives ~1 h, and
    // the download path refuses an id that was never PROVEN by a real fetch once
    // it is older than that — otherwise every Play starts with a guaranteed
    // `FILE_REFERENCE_EXPIRED` plus a re-resolve the browser player does not
    // wait for (its `<audio>` element errors after ~20 s).
    let mut stamped = data.clone();
    if let Some(obj) = stamped.as_object_mut() {
        obj.insert("resolved_at".to_string(), serde_json::json!(now()));
    }
    let key = cache_key(short_code, bot_id);
    cache::put("resolve", &key, &stamped, RESOLVE_TTL, 0);
    // A success always clears the negative marker, so a code that resolves later is
    // immediately eligible again.
    cache::invalidate("resolve_miss", &key);
}

pub fn invalidate_resolve_cache(short_code: &str, bot_id: &str) {
    let key = cache_key(short_code, bot_id);
    cache::invalidate("resolve", &key);
    cache::invalidate("resolve_fresh", &key);
}

/// `fd_resolve_shortcode_fresh()`: one refresh per burst, memoised 10 s so the
/// player's other in-flight Range requests reuse it instead of re-resolving.
pub async fn resolve_shortcode_fresh(short_code: &str, bot_id: &str) -> Value {
    let key = cache_key(short_code, bot_id);
    if let Some(v) = cache::get("resolve_fresh", &key) {
        if has_id(&v) {
            log(
                "fresh resolve memo hit - reusing the refresh",
                &serde_json::json!({"short_code": short_code, "bot_id": bot_id}),
            );
            return (*v).clone();
        }
    }
    log(
        "fresh resolve memo miss - resolving with force+nocache",
        &serde_json::json!({"short_code": short_code, "bot_id": bot_id}),
    );
    // `force=1&nocache=1` IS the refresh: the origin drops the ephemeral caches,
    // re-mints the reference with one send and persists it. There is no separate
    // `stale` parameter to send — one knob, one behaviour.
    let res = resolve_shortcode(short_code, bot_id, true).await;
    if has_id(&res) {
        cache::put("resolve_fresh", &key, &res, FRESH_MEMO_TTL, 0);
    }
    res
}

fn not_found_result(v: &Value) -> bool {
    v.get("message")
        .and_then(|m| m.as_str())
        .map(|m| m.to_ascii_lowercase().contains("not found for this request"))
        .unwrap_or(false)
}

/// `fd_resolve_shortcode()`: WordPress `/resolve-file` with the retry ladder.
pub async fn resolve_shortcode(short_code: &str, bot_id: &str, bypass_cache: bool) -> Value {
    let mut bot = bot_id.trim().to_string();
    if bot.is_empty() {
        bot = get_bot_id();
        if bot.is_empty() {
            return resolve_shortcode_concurrent(short_code, &[]).await;
        }
    }
    if !bypass_cache {
        if let Some(cached) = resolve_shortcode_cached(short_code, &bot) {
            return cached;
        }
    }

    let url = format!("{}/resolve-file", wp_api_base());
    let mut params = vec![
        ("short_code".to_string(), short_code.to_string()),
        ("bot_id".to_string(), bot.clone()),
    ];
    if bypass_cache {
        params.push(("force".to_string(), "1".to_string()));
        params.push(("nocache".to_string(), "1".to_string()));
    }

    let mut res = http_json(&url, &params, "GET", 6).await;
    if has_id(&res) {
        save_resolve_cache(short_code, &bot, &res);
        return res;
    }
    // An unanswered resolve used to leave no trail at all: the caller answered
    // "Track not found" and nothing said why.
    log(
        "resolve returned no file id",
        &serde_json::json!({
            "short_code": short_code,
            "bot_id": bot,
            "has_size": file_size_of(&res) > 0,
            "message": res.get("message").cloned().unwrap_or(Value::Null),
        }),
    );
    // A refusing origin is worth remembering: the prefetchers must not pile on.
    if note_resolve_rate_limit(&res.to_string()) {
        return res;
    }
    if not_found_result(&res) {
        return res;
    }

    // Telegram background relay was just triggered: poll 3x (1.2s, 1.5s, 1.5s).
    let mut retry = params.clone();
    if !retry.iter().any(|(k, _)| k == "force") {
        retry.push(("force".to_string(), "1".to_string()));
        retry.push(("nocache".to_string(), "1".to_string()));
    }
    for delay in [1200u64, 1500, 1500] {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        res = http_json(&url, &retry, "GET", 6).await;
        if has_id(&res) {
            save_resolve_cache(short_code, &bot, &res);
            return res;
        }
        if not_found_result(&res) {
            return res;
        }
    }
    res
}

/// `fd_resolve_shortcode_concurrent()`: the PHP races candidate bots with
/// `curl_multi` and returns the first winner. This port walks the same
/// candidate order serially — identical results, and it avoids N concurrent
/// WordPress calls from one request (which the origin rate-limits).
pub async fn resolve_shortcode_concurrent(short_code: &str, candidates: &[String]) -> Value {
    let mut bots: Vec<String> = candidates.to_vec();
    if bots.is_empty() {
        if let Some(picked) = pick_pool_bot() {
            let id = picked.get("bot_id").and_then(|v| v.as_str()).unwrap_or("");
            if !id.is_empty() {
                bots.push(id.to_string());
            }
        }
    }
    let active = get_bot_id();
    if !active.is_empty() && !bots.contains(&active) {
        bots.push(active);
    }
    for b in get_bot_pool() {
        let id = b.get("bot_id").and_then(|v| v.as_str()).unwrap_or("");
        if !id.is_empty() && !bots.contains(&id.to_string()) {
            bots.push(id.to_string());
        }
    }

    let mut last = Value::Object(serde_json::Map::new());
    for bot in bots {
        // Boxed: `resolve_shortcode_concurrent` is reachable from
        // `resolve_shortcode`, so the future would otherwise be infinitely sized.
        let res = Box::pin(resolve_shortcode(short_code, &bot, false)).await;
        if has_id(&res) {
            return res;
        }
        if not_found_result(&res) {
            return res;
        }
        if res.is_object() && !res.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            last = res;
        }
    }
    last
}

/// `fd_resolve_shortcodes_batch()`: the WordPress BATCH endpoint. Falls back to
/// singles for anything the batch did not answer.
pub async fn resolve_shortcodes_batch(short_codes: &[String], bot_id: &str, timeout: u64) -> HashMap<String, Value> {
    let mut out: HashMap<String, Value> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let codes: Vec<String> = short_codes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect();
    if codes.is_empty() {
        return out;
    }
    let effective = if bot_id.trim().is_empty() {
        get_bot_id()
    } else {
        bot_id.trim().to_string()
    };

    let url = format!("{}/resolve-files", wp_api_base());
    let params = vec![
        ("short_codes".to_string(), codes.join(",")),
        ("bot_id".to_string(), effective.clone()),
        ("_t".to_string(), now().to_string()),
    ];
    let res = http_json(&url, &params, "GET", timeout).await;
    if let Some(results) = res.get("results").and_then(|v| v.as_object()) {
        for code in &codes {
            if let Some(row) = results.get(code) {
                if row.is_object() {
                    out.insert(code.clone(), row.clone());
                    if has_id(row) && !effective.is_empty() {
                        // Keep the richer fields: a batch row only carries
                        // ok/short_code/file_id_mt/bot_id, while a single
                        // resolve also returns file_size/title/mime.
                        let mut merged = resolve_shortcode_cached(code, &effective)
                            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                        if let (Some(dst), Some(src)) = (merged.as_object_mut(), row.as_object()) {
                            for (k, v) in src {
                                dst.insert(k.clone(), v.clone());
                            }
                        }
                        save_resolve_cache(code, &effective, &merged);
                    }
                }
            }
        }
    }

    for code in &codes {
        // A refusing origin must STOP the per-code fallback, not walk it: measured
        // on a real phone, a 285-code warm turned into 285 consecutive HTTP 503s and
        // starved the user's own Play. One 503 is enough to know the origin is busy.
        if resolve_cooldown_remaining_sec() > 0 {
            log(
                "resolve batch singles stopped (origin busy)",
                &serde_json::json!({"remaining_codes": codes.len(), "bot_id": effective}),
            );
            break;
        }
        let row = out.get(code);
        if row.map(has_id).unwrap_or(false) {
            continue;
        }
        if row
            .and_then(|r| r.get("message"))
            .and_then(|m| m.as_str())
            .map(|m| m.to_ascii_lowercase().contains("not found"))
            .unwrap_or(false)
        {
            continue;
        }
        let single = resolve_shortcode(code, &effective, false).await;
        out.insert(code.clone(), single);
    }
    out
}

/// How long the origin is refusing resolves for, in seconds (0 = clear).
///
/// The TDLib app keeps `resolveCooldownUntilMs` for exactly this reason: opening a
/// catalog page fires 24 prefetches at once, and before this gate the device logged
/// `[Resolve] upstream rate limit — pausing resolves for 5s` AND `[PostFiles] page 0
/// failed: timeout` **in the same second** — the prefetches were the load that
/// tripped the limiter. Prefetchers consult this before adding work.
pub fn resolve_cooldown_remaining_sec() -> i64 {
    // Was `resolve_cooldown.marker` (mtime + a number in the file = two syscalls on
    // every prewarm decision). The deadline is now the value itself.
    let Some(v) = cache::get("cooldown", "resolve") else {
        return 0;
    };
    let until = v.get("until").and_then(|x| x.as_i64()).unwrap_or(0);
    (until - crate::storage::now()).max(0)
}

fn note_resolve_rate_limit(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    let hit = lower.contains("rate limit")
        || lower.contains("flood")
        || lower.contains("too many requests")
        || lower.contains("retry after")
        || lower.contains("server busy");
    if hit {
        // 15 min cap: the resolver has been measured sending `retry_after: 137`.
        let secs = 5i64.max(15);
        cache::put(
            "cooldown",
            "resolve",
            &serde_json::json!({"until": crate::storage::now() + secs}),
            secs,
            0,
        );
        log(
            "resolve rate limited - pausing prefetches",
            &serde_json::json!({"seconds": secs, "message": msg}),
        );
    }
    hit
}

/// `fd_prewarm_streams_batch()` (backend.php:3701) and the Kotlin twin.
///
/// A stream list is built from file METADATA only; the Bot API `file_id` is
/// resolved later, when the player asks for bytes — so the first Play paid a
/// 1-3 s resolve. This warms every short code of the list that is not already
/// cached, in the BACKGROUND, so Play on a title you just browsed is instant.
/// Never blocks the response and never fails it: a resolve error is only logged.
///
/// Returns how many codes were sent to the background (0 = all already warm).
pub fn prewarm_streams_batch(codes: Vec<String>, bot_id: &str, context: &str) -> usize {
    prewarm_streams_batch_limited(codes, bot_id, context, 12)
}

/// How many codes one prewarm may resolve.
///
/// Measured on a real phone: warming a 285-code movie list turned into 285 separate
/// `/resolve-file` calls, every one of them **HTTP 503** ("server busy resolving
/// file"), and the storm starved the user's own Play (11.7 s to first byte). The
/// Kotlin app bounds the same work through its cooldown; here the count is bounded
/// too. The point of the warm is the few items a user actually taps, not the tail.
pub const PREWARM_LIMIT: usize = 12;

pub fn prewarm_streams_batch_limited(
    codes: Vec<String>,
    bot_id: &str,
    context: &str,
    limit: usize,
) -> usize {
    let effective = if bot_id.trim().is_empty() {
        get_bot_id()
    } else {
        bot_id.trim().to_string()
    };
    if effective.is_empty() {
        return 0;
    }
    let mut missing: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for c in codes {
        let c = c.trim().to_string();
        if c.is_empty() || !seen.insert(c.clone()) {
            continue;
        }
        match resolve_shortcode_cached(&c, &effective) {
            Some(v) if has_id(&v) => {}
            _ => {
                // A code the origin just refused is not worth another attempt yet.
                if cache::get("resolve_miss", &miss_key(&c, &effective)).is_some() {
                    continue;
                }
                missing.push(c)
            }
        }
    }
    if missing.is_empty() {
        return 0;
    }
    // Warm what the user is most likely to tap, not the whole tail — see PREWARM_LIMIT.
    let before = missing.len();
    missing.truncate(limit.max(1));
    if resolve_cooldown_remaining_sec() > 0 {
        log(
            "prewarm skipped (resolve cooldown)",
            &serde_json::json!({"context": context, "missing": before}),
        );
        return 0;
    }
    // Never compete with live playback: a resolve in flight holds the same origin
    // and Telegram budget the stream needs.
    if crate::download::active_streams() > 0 {
        log(
            "prewarm skipped (stream active)",
            &serde_json::json!({"context": context, "missing": before}),
        );
        return 0;
    }
    let n = missing.len();
    let ctx = context.to_string();
    log(
        "prewarm streams batch",
        &serde_json::json!({"context": ctx, "missing": n, "bot_id": effective}),
    );
    // Detached: the list goes back to the player now. `warmup_resolve_batch`
    // persists each hit, so the NEXT Play skips the resolve entirely.
    tokio::spawn(async move {
        let res = warmup_resolve_batch(&missing, &effective).await;
        for code in &res.failed_codes {
            cache::put(
                "resolve_miss",
                &miss_key(code, &effective),
                &serde_json::json!(1),
                RESOLVE_MISS_TTL,
                0,
            );
        }
        log(
            "prewarm streams batch finished",
            &serde_json::json!({
                "context": ctx,
                "resolved": res.resolved,
                "failed": res.failed,
                "suppressed_for_secs": RESOLVE_MISS_TTL,
            }),
        );
    });
    n
}

pub struct WarmupResult {
    pub total: usize,
    pub resolved: usize,
    pub failed: usize,
    pub failed_codes: Vec<String>,
    pub results: HashMap<String, Value>,
    pub bot_id: String,
}

/// `fd_warmup_resolve_batch()`: resolve and PERSIST so the next Play is instant.
pub async fn warmup_resolve_batch(short_codes: &[String], bot_id: &str) -> WarmupResult {
    let mut seen: HashSet<String> = HashSet::new();
    let unique: Vec<String> = short_codes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect();
    let effective = if bot_id.trim().is_empty() {
        get_bot_id()
    } else {
        bot_id.trim().to_string()
    };
    let results = resolve_shortcodes_batch(&unique, &effective, 60).await;
    let mut resolved = 0usize;
    let mut failed_codes: Vec<String> = Vec::new();
    for code in &unique {
        match results.get(code) {
            Some(row) if has_id(row) => resolved += 1,
            _ => failed_codes.push(code.clone()),
        }
    }
    log(
        "warmup resolve batch completed",
        &serde_json::json!({
            "total": unique.len(),
            "resolved": resolved,
            "failed": failed_codes.len(),
            "bot_id": effective
        }),
    );
    WarmupResult {
        total: unique.len(),
        resolved,
        failed: failed_codes.len(),
        failed_codes,
        results,
        bot_id: effective,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real `/resolve-files` row shape (measured against the origin): an id
    /// and nothing else. Caching it as a resolve is what made `/eclipse/stream`
    /// answer "Track not found" for a code that resolves fine.
    #[test]
    fn a_batch_row_is_never_cached_as_a_resolve() {
        let batch = serde_json::json!({
            "ok": 1, "short_code": "batch1", "file_id_mt": "AQADbatch", "bot_id": 8469916399i64
        });
        save_resolve_cache("batch1", "test-bot", &batch);
        assert!(resolve_shortcode_cached("batch1", "test-bot").is_none());

        let full = serde_json::json!({
            "ok": 1, "file_id_mt": "AQADbatch", "file_size": 18552780,
            "title": "02.Billie.Eilish.LUNCH.flac", "mime": "audio/x-flac"
        });
        save_resolve_cache("batch1", "test-bot", &full);
        let hit = resolve_shortcode_cached("batch1", "test-bot").expect("a full row is usable");
        assert_eq!(hit["title"], "02.Billie.Eilish.LUNCH.flac");
    }

    /// Entries already on disk from an older build must read as a MISS, not as a
    /// metadata-less hit, or the 2 h redb copy keeps breaking playback after the
    /// upgrade.
    #[test]
    fn a_stored_entry_without_a_size_reads_as_a_miss() {
        let key = cache_key("poison1", "test-bot");
        cache::put(
            "resolve",
            &key,
            &serde_json::json!({"ok": 1, "file_id_mt": "AQADpoison"}),
            60,
            0,
        );
        assert!(cache::get("resolve", &key).is_some(), "the raw entry is there");
        assert!(resolve_shortcode_cached("poison1", "test-bot").is_none());
    }
}
