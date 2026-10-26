//! Port of the outbound HTTP helpers, backend.php:1901-2073 and 12667-12791:
//! `fd_http_json`, `fd_cached_wp_json`, `fd_fallback_url`,
//! `fd_is_connection_reset`, `fd_http_get_contents`,
//! `fd_http_fire_and_forget`, `fd_http_download_file`.
//!
//! Amp and cURL do not exist here; `reqwest` (rustls) is the single transport.
//! The behaviour that matters is preserved: JSON API + `X-API-Secret`, the
//! `telegra.my` DPI fallback, disk caching with a stale fallback, and a
//! timeout that never throws to the caller (returns `{}` like the PHP).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::config::{user_agent, wp_api_base, wp_api_base_fallback, WP_FALLBACK_HOST};
use crate::credentials::get_api_secret;
use crate::log::log;

pub fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(user_agent())
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

pub fn fallback_url(url: &str) -> Option<String> {
    let base = wp_api_base();
    if let Some(rest) = url.strip_prefix(&base) {
        return Some(format!("{}{}", wp_api_base_fallback(), rest));
    }
    if url.contains("pencarimovie.com") {
        return Some(url.replace("https://pencarimovie.com", &format!("https://{}", WP_FALLBACK_HOST)));
    }
    None
}

pub fn is_connection_reset(message: &str) -> bool {
    let s = message.to_ascii_lowercase();
    s.contains("econnreset")
        || s.contains("reset by peer")
        || s.contains("tls negotiation failed")
        || s.contains("socket hang up")
        || s.contains("etimedout")
        || s.contains("econnrefused")
        || s.contains("connection reset")
}

async fn raw_get(url: &str, timeout_secs: u64, extra: &HashMap<String, String>) -> (bool, u16, String, Option<String>) {
    let mut req = client().get(url).timeout(Duration::from_secs(timeout_secs.max(1)));
    req = req.header("Accept", "*/*");
    for (k, v) in extra {
        req = req.header(k.as_str(), v.as_str());
    }
    match req.send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let ok = r.status().is_success();
            match r.text().await {
                Ok(t) => (ok, status, t, None),
                Err(e) => (false, status, String::new(), Some(e.to_string())),
            }
        }
        Err(e) => (false, 0, String::new(), Some(e.to_string())),
    }
}

/// `fd_http_get_contents()`: one request, one retry through `telegra.my` when
/// the failure looks like an in-path firewall reset, never panics.
pub async fn get_contents(url: &str, timeout_secs: u64, extra: HashMap<String, String>) -> Option<String> {
    let (ok, status, text, err) = raw_get(url, timeout_secs, &extra).await;
    if ok {
        return Some(text);
    }
    if let Some(e) = &err {
        if is_connection_reset(e) {
            if let Some(alt) = fallback_url(url) {
                log("primary host failed, falling back to DPI proxy", &serde_json::json!({"url": url, "error": e}));
                let mut h = extra.clone();
                h.insert("Host".into(), "pencarimovie.com".into());
                let (ok2, _, text2, err2) = raw_get(&alt, timeout_secs, &h).await;
                if ok2 {
                    return Some(text2);
                }
                log("fallback host also failed", &serde_json::json!({"url": alt, "error": err2}));
            }
        }
    }
    log("http_get_contents failed", &serde_json::json!({"url": url, "status": status, "error": err}));
    None
}

fn encode_query(params: &[(String, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        ser.append_pair(k, v);
    }
    ser.finish()
}

/// `X-PM-Auth` plus a per-request proof, when this install holds a token.
///
/// This is the Option-A credential: the token names the install and the key it
/// is bound to, and the proof shows we still hold that key. A copied token is
/// inert — it cannot produce a valid proof from another machine.
pub(crate) fn pm_auth_headers(method: &str, url: &str) -> Vec<(String, String)> {
    let token = crate::credentials::auth_token();
    if token.is_empty() {
        return Vec::new();
    }
    let ts = crate::storage::now();
    let nonce = crate::credentials::new_nonce();
    // The signed endpoint must match the origin's `pm_auth_endpoint()`, which
    // takes the last segment of the matched REST route.
    let endpoint = crate::credentials::endpoint_of(url);
    let Some((proof, pubkey)) = crate::credentials::sign_proof(method, &endpoint, ts, &nonce) else {
        return Vec::new();
    };
    vec![
        ("X-PM-Auth".to_string(), token),
        ("X-PM-Sign-Pub".to_string(), pubkey),
        ("X-PM-Ts".to_string(), ts.to_string()),
        ("X-PM-Nonce".to_string(), nonce),
        ("X-PM-Proof".to_string(), proof),
    ]
}

/// `fd_http_json()`: GET (query string) or POST (JSON body) with X-API-Secret.
pub async fn http_json(url: &str, params: &[(String, String)], method: &str, timeout_secs: u64) -> Value {
    http_json_h(url, params, method, timeout_secs, &[]).await
}

/// [`http_json()`] plus caller headers.
///
/// Every call also carries the bot-scoped provision ticket when this install
/// holds one, so the protected routes authorise on a ticket that names one bot
/// and expires, rather than on the site-wide secret. Callers that need extra
/// headers pass them here — the lease advertises its X25519 public key this way.
pub async fn http_json_h(
    url: &str,
    params: &[(String, String)],
    method: &str,
    timeout_secs: u64,
    headers: &[(String, String)],
) -> Value {
    let mut target = url.to_string();
    let secret = get_api_secret();
    let extra: Vec<(String, String)> = headers
        .iter()
        .cloned()
        .chain(crate::credentials::ticket_headers())
        .chain(pm_auth_headers(method, &target))
        .collect();

    let mut body: Option<String> = None;
    if method.eq_ignore_ascii_case("GET") {
        let q = encode_query(params);
        if !q.is_empty() {
            target.push(if target.contains('?') { '&' } else { '?' });
            target.push_str(&q);
        }
    } else {
        let map: serde_json::Map<String, Value> = params
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        body = Some(Value::Object(map).to_string());
    }

    let call = |u: &str| {
        let target = u.to_string();
        let secret = secret.clone();
        let body = body.clone();
        let extra = extra.clone();
        async move {
            let mut req = client()
                .request(
                    if method.eq_ignore_ascii_case("GET") {
                        reqwest::Method::GET
                    } else {
                        reqwest::Method::POST
                    },
                    &target,
                )
                .timeout(Duration::from_secs(timeout_secs.max(1)))
                .header("Accept", "application/json")
                .header("X-Requested-With", "XMLHttpRequest");
            if !secret.is_empty() {
                req = req.header("X-API-Secret", secret);
            }
            for (k, v) in &extra {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(b) = body {
                req = req.header("Content-Type", "application/json").body(b);
            }
            match req.send().await {
                Ok(r) => {
                    let status = r.status().as_u16();
                    let ok = r.status().is_success();
                    match r.text().await {
                        Ok(t) => (ok, status, t, None::<String>),
                        Err(e) => (false, status, String::new(), Some(e.to_string())),
                    }
                }
                Err(e) => (false, 0, String::new(), Some(e.to_string())),
            }
        }
    };

    let (mut ok, mut status, mut text, mut err) = call(&target).await;
    if !ok {
        if let Some(e) = &err {
            if is_connection_reset(e) {
                if let Some(alt) = fallback_url(&target) {
                    log("wp request reset, retrying via fallback host", &serde_json::json!({"url": target}));
                    let r = call(&alt).await;
                    ok = r.0;
                    status = r.1;
                    text = r.2;
                    err = r.3;
                }
            }
        }
    }
    if !ok {
        log(
            "http_json failed",
            &serde_json::json!({"url": target, "status": status, "error": err}),
        );
        return Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(&text).unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
}

/// Is this a usable WordPress body (as opposed to an error envelope)?
fn truthy_ok(v: &Value) -> bool {
    v.get("ok")
        .map(|x| x.as_bool().unwrap_or(false) || x.as_i64().unwrap_or(0) != 0)
        .unwrap_or(false)
}

/// How long a stale WordPress body may still be served when a refresh fails.
const WP_STALE_GRACE: i64 = 21_600;

/// Did the call fail to answer at all?
///
/// `http_json()` collapses a transport error, a timeout and every 5xx into an
/// EMPTY object. A real answer is never empty: a miss is `{"ok":0,"error":…}`
/// and a hit always carries `ok`. So only the empty shape is worth a retry —
/// retrying an honest 404/400 would double the work and change nothing.
fn needs_wp_retry(body: &Value) -> bool {
    body.as_object().map(|m| m.is_empty()).unwrap_or(false)
}

/// `fd_cached_wp_json()`: cache keyed on the FULL url, with single-flight and a
/// stale fallback so a slow origin never blocks playback. Only a truthy `ok` body
/// is stored, so an error response cannot poison the entry.
///
/// The old version had **no single-flight**: a burst of cold requests for the same
/// URL all opened their own WordPress connection (the PHP guarded this with a
/// `flock`). `fetch_cached` coalesces them into one call.
pub async fn cached_wp_json(url: &str, ttl: i64, timeout_secs: u64) -> Value {
    let url_owned = url.to_string();
    let body = crate::cache::fetch_cached("wp_json", url, ttl, WP_STALE_GRACE, move || async move {
        let mut fresh = http_json(&url_owned, &[], "GET", timeout_secs).await;
        // One retry when the call did not answer at all. `http_json()` returns an
        // EMPTY object for a transport error or a 5xx (`{"ok":0,…}` bodies are
        // real answers and are left alone), and an empty body here is fatal: the
        // series/episode list is built from it, so a cold cache plus one origin
        // 504 answered "No Streams Found" — reproduced on `mal:54492:2:7` and
        // `mal:54492:2:12`, where the same episodes listed 9 and 7 streams a
        // minute later. Nothing else fails over in that window: the stale copy
        // is what usually covers this, and a fresh process has none.
        if needs_wp_retry(&fresh) {
            log(
                "wp request empty, retrying once",
                &serde_json::json!({"url": url_owned}),
            );
            tokio::time::sleep(Duration::from_millis(400)).await;
            fresh = http_json(&url_owned, &[], "GET", timeout_secs).await;
        }
        let cacheable = truthy_ok(&fresh);
        (fresh, cacheable)
    })
    .await;
    if !truthy_ok(&body) {
        if let Some(stale) = crate::cache::get_stale("wp_json", url) {
            log(
                "cached wp json served stale",
                &serde_json::json!({"url": url}),
            );
            return (*stale).clone();
        }
    }
    body
}

/// `fd_http_fire_and_forget()`: opens its own connection and returns at once;
/// the server-side work runs on another worker thread.
pub fn fire_and_forget(url: &str, params: &[(String, String)]) {
    let mut target = url.to_string();
    let q = encode_query(params);
    if !q.is_empty() {
        target.push(if target.contains('?') { '&' } else { '?' });
        target.push_str(&q);
    }
    let secret = get_api_secret();
    tokio::spawn(async move {
        let mut req = client().get(&target).timeout(Duration::from_secs(60));
        if !secret.is_empty() {
            req = req.header("X-API-Secret", secret);
        }
        let _ = req.send().await;
    });
}

/// `fd_http_download_file()`: resumable-ish file download to a path.
pub async fn download_file(url: &str, dest: &std::path::Path, timeout_secs: u64) -> bool {
    match client()
        .get(url)
        .timeout(Duration::from_secs(timeout_secs.max(1)))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.bytes().await {
            Ok(b) => std::fs::write(dest, &b).is_ok(),
            Err(_) => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a DEAD call is retried. An honest miss (`ok:0` + error) and an honest
    /// empty list (`ok:1`, no items) are answers, and retrying them would double
    /// the origin work for nothing.
    #[test]
    fn only_a_dead_wp_call_is_retried() {
        assert!(needs_wp_retry(&serde_json::json!({})));
        assert!(!needs_wp_retry(&serde_json::json!({"ok": 0, "error": "Not found"})));
        assert!(!needs_wp_retry(&serde_json::json!({"ok": 1, "items": []})));
        assert!(!needs_wp_retry(&serde_json::json!({"ok": 1, "total": 0})));
        assert!(!needs_wp_retry(&serde_json::Value::Null));
    }
}
