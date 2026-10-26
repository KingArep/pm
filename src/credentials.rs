//! Port of the credential helpers, backend.php:516-548 and 6324-6459:
//! `fd_get_api_secret`, `fd_save_api_secret`, `fd_clear_api_secret`,
//! `fd_decrypt_credentials`, `fd_fetch_credentials_from_wordpress`,
//! `fd_load_cached_api_credentials`, `fd_save_cached_api_credentials`.
//!
//! WordPress encrypts api_id/api_hash with AES-256-CBC using
//! `hash('sha256', $botToken, true)` as the key; the IV comes back separately.
//! No bot token or session is ever copied between stacks.

use aes::Aes256;
use cbc::Decryptor;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::httpc::http_json;
use crate::log::log;
use crate::storage::{env, storage_path, storage_dir};

type Aes256CbcDec = Decryptor<Aes256>;

pub fn api_secret_path() -> std::path::PathBuf {
    storage_path("api_secret.key")
}

pub fn get_api_secret() -> String {
    if let Some(s) = env("FD_API_SECRET") {
        return s.trim().to_string();
    }
    std::fs::read_to_string(api_secret_path())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub fn save_api_secret(secret: &str) {
    if secret.is_empty() {
        return;
    }
    let _ = std::fs::write(api_secret_path(), secret);
}

pub fn clear_api_secret() {
    let _ = std::fs::remove_file(api_secret_path());
}

fn creds_path() -> std::path::PathBuf {
    storage_path("api_credentials.json")
}

pub fn load_cached_api_credentials() -> Option<(i32, String)> {
    if let Ok(text) = std::fs::read_to_string(creds_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            let api_id = v.get("api_id").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
            let api_hash = v
                .get("api_hash")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if api_id > 0 && !api_hash.is_empty() {
                return Some((api_id, api_hash));
            }
        }
    }
    // Environment overrides (debug only, same intent as $overrides in PHP).
    let env_id = env("FD_API_ID").and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
    if let (true, Some(hash)) = (env_id > 0, env("FD_API_HASH")) {
        return Some((env_id, hash));
    }
    None
}

pub fn save_cached_api_credentials(api_id: i32, api_hash: &str) {
    let body = serde_json::json!({"api_id": api_id, "api_hash": api_hash});
    let _ = std::fs::write(creds_path(), body.to_string());
}

fn decode_iv(iv: &str) -> Option<[u8; 16]> {
    if iv.len() == 32 && iv.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Ok(b) = hex::decode(iv) {
            if b.len() == 16 {
                let mut out = [0u8; 16];
                out.copy_from_slice(&b);
                return Some(out);
            }
        }
    }
    if let Ok(b) = STANDARD.decode(iv) {
        if b.len() == 16 {
            let mut out = [0u8; 16];
            out.copy_from_slice(&b);
            return Some(out);
        }
    }
    None
}

fn try_decrypt(key: &[u8; 32], iv: &[u8; 16], input: &[u8]) -> Option<(i32, String)> {
    let mut buf = input.to_vec();
    let pt = Aes256CbcDec::new_from_slices(key, iv)
        .ok()?
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .ok()?;
    let text = String::from_utf8_lossy(pt).to_string();
    let v: Value = serde_json::from_str(&text).ok()?;
    let api_id = v.get("api_id").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
    let api_hash = v.get("api_hash").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if api_id > 0 && !api_hash.is_empty() {
        return Some((api_id, api_hash));
    }
    None
}

/// `fd_decrypt_credentials()`: AES-256-CBC, key = raw `sha256(botToken)`, PKCS#7.
pub fn decrypt_credentials(encrypted: &str, iv: &str, bot_token: &str) -> Option<(i32, String)> {
    let mut hasher = Sha256::new();
    hasher.update(bot_token.as_bytes());
    let digest = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    let ivb = decode_iv(iv)?;

    if let Ok(raw) = STANDARD.decode(encrypted) {
        if let Some(v) = try_decrypt(&key, &ivb, &raw) {
            return Some(v);
        }
    }
    if encrypted.len() % 2 == 0 && encrypted.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Ok(raw) = hex::decode(encrypted) {
            if let Some(v) = try_decrypt(&key, &ivb, &raw) {
                return Some(v);
            }
        }
    }
    None
}

/// `fd_fetch_credentials_from_wordpress()`: POST `/save-bot-token` and decrypt.
pub async fn fetch_credentials_from_wordpress(bot_token: &str) -> Option<(i32, String)> {
    let url = format!("{}/save-bot-token", crate::config::wp_api_base());
    let resp = http_json(
        &url,
        &[("bot_token".to_string(), bot_token.to_string())],
        "POST",
        30,
    )
    .await;
    if !resp.is_object() {
        return None;
    }
    if let Some(secret) = resp.get("api_secret").and_then(|v| v.as_str()) {
        save_api_secret(secret);
    }
    let encrypted = resp
        .get("encrypted_credentials")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let iv = resp
        .get("encryption_iv")
        .or_else(|| resp.get("credentials_iv"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !encrypted.is_empty() && !iv.is_empty() {
        if let Some((api_id, api_hash)) = decrypt_credentials(encrypted, iv, bot_token) {
            save_cached_api_credentials(api_id, &api_hash);
            return Some((api_id, api_hash));
        }
        log("credential decrypt failed", &serde_json::json!({}));
    }
    None
}

/// Path used by `/api/session` diagnostics only.
pub fn storage_root() -> std::path::PathBuf {
    storage_dir()
}

/* ── provision ticket + sealed-token keypair ─────────────────────────────── */

fn provision_ticket_path() -> std::path::PathBuf {
    storage_path("provision_ticket.json")
}

fn provision_key_path() -> std::path::PathBuf {
    storage_path("provision_keypair.json")
}

fn sign_key_path() -> std::path::PathBuf {
    storage_path("device_signkey.json")
}

fn auth_token_path() -> std::path::PathBuf {
    storage_path("auth_token.json")
}

/// base64url without padding — the encoding PM-Auth uses on the wire.
pub fn b64u(bytes: impl AsRef<[u8]>) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes.as_ref())
}

pub fn b64u_decode(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .unwrap_or_default()
}

/// 16 random bytes, hex. One per request: the origin refuses a repeated nonce.
pub fn new_nonce() -> String {
    let mut buf = [0u8; 16];
    for b in buf.iter_mut() {
        *b = rand::random::<u8>();
    }
    hex::encode(buf)
}

/// The install's Ed25519 keypair `(verifying, signing)`, created once.
///
/// The signing half NEVER leaves this machine — that is what makes a captured
/// `auth_token` useless elsewhere.
pub fn sign_keypair() -> ([u8; 32], [u8; 32]) {
    if let Ok(text) = std::fs::read_to_string(sign_key_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            let pk = v.get("pk").and_then(|x| x.as_str()).unwrap_or("");
            let sk = v.get("sk").and_then(|x| x.as_str()).unwrap_or("");
            if let (Ok(pk_raw), Ok(sk_raw)) = (hex::decode(pk), hex::decode(sk)) {
                if pk_raw.len() == 32 && sk_raw.len() == 32 {
                    let mut p = [0u8; 32];
                    let mut s = [0u8; 32];
                    p.copy_from_slice(&pk_raw);
                    s.copy_from_slice(&sk_raw);
                    return (p, s);
                }
            }
        }
    }

    let mut sk_bytes = [0u8; 32];
    for b in sk_bytes.iter_mut() {
        *b = rand::random::<u8>();
    }
    let signing = ed25519_dalek::SigningKey::from_bytes(&sk_bytes);
    let pk = signing.verifying_key().to_bytes();
    let body = serde_json::json!({"pk": hex::encode(pk), "sk": hex::encode(sk_bytes)});
    let _ = std::fs::write(sign_key_path(), body.to_string());
    (pk, sk_bytes)
}

/// The short-lived token the origin minted for this install, if still valid.
pub fn auth_token() -> String {
    let Ok(text) = std::fs::read_to_string(auth_token_path()) else {
        return String::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return String::new();
    };
    let token = v.get("token").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let exp = v.get("expires_at").and_then(|x| x.as_i64()).unwrap_or(0);
    // A minute of slack so a token does not expire mid-flight.
    if token.is_empty() || exp <= crate::storage::now() + 60 {
        return String::new();
    }
    token
}

pub fn save_auth_token(token: &str, expires_at: i64) {
    if token.is_empty() {
        return;
    }
    let body = serde_json::json!({"token": token, "expires_at": expires_at});
    let _ = std::fs::write(auth_token_path(), body.to_string());
}

pub fn clear_auth_token() {
    let _ = std::fs::remove_file(auth_token_path());
}

/// Sign `METHOD\nendpoint\nts\nnonce` with the install key.
///
/// Returns `(proof_b64u, public_key_b64u)`. The origin rebuilds the same string
/// and checks the key's thumbprint against the one burned into the token.
/// Make sure this install holds a usable PM-Auth token, enrolling if it does not.
///
/// This is the call that was missing: `/provision-session` is the ONLY place
/// the origin enrolled an install, so an install that already had a bot session
/// never leased again and therefore never held a token. It kept working purely
/// on the fleet-wide `X-API-Secret` — which is exactly what we are retiring, so
/// EVERY existing install (old build and new build alike) would have stopped
/// playing the moment the secret was refused. Measured on the dev box: no
/// `device_signkey.json`, no `auth_token.json`, no ticket, while playback ran.
///
/// The credential is the Ed25519 key this install generates and keeps; only the
/// public half travels. Returns true when a token is available afterwards.
pub async fn ensure_enrolled() -> bool {
    let bot = crate::session::get_bot_id();
    if bot.is_empty() {
        // Nothing to scope a token to yet: the lease path will enrol.
        return false;
    }
    if !auth_token().is_empty() {
        return true;
    }
    let (pk, _sk) = sign_keypair();
    let url = format!("{}/auth-enroll", crate::config::wp_api_base());
    let params = vec![
        ("install_pub".to_string(), b64u(pk)),
        ("device_id".to_string(), crate::diagnostics::get_device_id()),
        ("bot_id".to_string(), bot),
    ];
    let resp = crate::httpc::http_json(&url, &params, "POST", 20).await;
    let ok = resp
        .get("ok")
        .map(|v| v.as_bool().unwrap_or(false) || v.as_i64().unwrap_or(0) != 0)
        .unwrap_or(false);
    let token = resp.get("auth_token").and_then(|v| v.as_str()).unwrap_or("");
    let expires_at = resp.get("auth_expires_at").and_then(|v| v.as_i64()).unwrap_or(0);
    if !ok || token.is_empty() {
        crate::log::stderr_log(
            "pm-auth enrol failed",
            Some(&serde_json::json!({
                "status_ok": ok,
                "message": resp.get("message").cloned().unwrap_or(serde_json::Value::Null),
            })),
        );
        return false;
    }
    save_auth_token(token, expires_at);
    true
}

pub fn sign_proof(method: &str, endpoint: &str, ts: i64, nonce: &str) -> Option<(String, String)> {
    use ed25519_dalek::Signer as _;
    let (pk, sk) = sign_keypair();
    let signing = ed25519_dalek::SigningKey::from_bytes(&sk);
    let msg = format!("{}\n{}\n{}\n{}", method.to_ascii_uppercase(), endpoint, ts, nonce);
    let sig = signing.sign(msg.as_bytes());
    Some((b64u(sig.to_bytes()), b64u(pk)))
}

/// Last path segment of a URL, ignoring the query — must match `pm_auth_endpoint()`
/// on the origin, which uses the last segment of the matched REST route.
pub fn endpoint_of(url: &str) -> String {
    let path = url.split('?').next().unwrap_or("");
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some(seg) if !seg.is_empty() => seg.to_string(),
        _ => "unknown".to_string(),
    }
}

/// The bot-scoped ticket the origin minted with the lease.
///
/// Deliberately NOT the site-wide `api_secret`: a ticket names one bot and
/// expires, so a leaked install cannot reach every protected route forever.
pub fn provision_ticket() -> String {
    let Ok(text) = std::fs::read_to_string(provision_ticket_path()) else {
        return String::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return String::new();
    };
    let ticket = v.get("ticket").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let expires = v.get("expires_at").and_then(|x| x.as_i64()).unwrap_or(0);
    if ticket.is_empty() || expires <= crate::storage::now() {
        return String::new();
    }
    ticket
}

pub fn save_provision_ticket(ticket: &str, expires_at: i64, bot_id: &str) {
    if ticket.is_empty() {
        return;
    }
    let body = serde_json::json!({
        "ticket": ticket,
        "expires_at": expires_at,
        "bot_id": bot_id,
    });
    let _ = std::fs::write(provision_ticket_path(), body.to_string());
}

pub fn clear_provision_ticket() {
    let _ = std::fs::remove_file(provision_ticket_path());
}

/// Headers that authorise the bot-scoped routes (`/resolve-file`, `/resolve-files`).
///
/// Empty when no ticket is held: the origin then falls back to `X-API-Secret`,
/// which is what an install that has not provisioned yet still has to use.
pub fn ticket_headers() -> Vec<(String, String)> {
    let ticket = provision_ticket();
    if ticket.is_empty() {
        return Vec::new();
    }
    let mut headers = vec![
        ("X-PM-Ticket".to_string(), ticket),
        ("X-PM-Device".to_string(), crate::diagnostics::get_device_id()),
    ];
    // The ticket is bound to a bot id. Routes without a `bot_id` parameter
    // (/lookup-id, /catalog-ids) need it named explicitly, otherwise they are
    // reachable only through the 60-minute install token and go dark an hour
    // after boot once `api_secret` is retired.
    let bot = crate::session::get_bot_id();
    if !bot.is_empty() {
        headers.push(("X-PM-Bot".to_string(), bot));
    }
    headers
}

/// X25519 keypair used to receive the bot token sealed. Generated once per
/// install and kept, so a restart can still open a lease issued earlier.
pub fn provision_keypair() -> ([u8; 32], [u8; 32]) {
    if let Ok(text) = std::fs::read_to_string(provision_key_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            let pk = v.get("pk").and_then(|x| x.as_str()).unwrap_or("");
            let sk = v.get("sk").and_then(|x| x.as_str()).unwrap_or("");
            if let (Ok(pk_raw), Ok(sk_raw)) = (hex::decode(pk), hex::decode(sk)) {
                if pk_raw.len() == 32 && sk_raw.len() == 32 {
                    let mut p = [0u8; 32];
                    let mut s = [0u8; 32];
                    p.copy_from_slice(&pk_raw);
                    s.copy_from_slice(&sk_raw);
                    return (p, s);
                }
            }
        }
    }

    let mut sk_bytes = [0u8; 32];
    for b in sk_bytes.iter_mut() {
        *b = rand::random::<u8>();
    }
    let secret = crypto_box::SecretKey::from(sk_bytes);
    let public = crypto_box::PublicKey::from(&secret);
    let (pk, sk) = (public.to_bytes(), secret.to_bytes());

    let body = serde_json::json!({"pk": hex::encode(pk), "sk": hex::encode(sk)});
    let _ = std::fs::write(provision_key_path(), body.to_string());
    (pk, sk)
}

/// `sodium_crypto_box_seal_open()`: 32-byte ephemeral public key ‖ ciphertext,
/// nonce = BLAKE2b(epk ‖ recipient_pk, 24).
///
/// Returns `None` for anything that does not open — a truncated payload, a
/// ciphertext sealed to another install, or a tampered one.
pub fn open_sealed_token(sealed: &[u8], pk: &[u8; 32], sk: &[u8; 32]) -> Option<String> {
    if sealed.len() < 32 + 16 {
        return None;
    }
    let (epk_raw, ciphertext) = sealed.split_at(32);
    let epk: [u8; 32] = epk_raw.try_into().ok()?;

    let mut hasher = blake2::Blake2b::<blake2::digest::consts::U24>::new();
    hasher.update(epk);
    hasher.update(*pk);
    let nonce_bytes = hasher.finalize();
    let nonce = crypto_box::aead::generic_array::GenericArray::from_slice(nonce_bytes.as_slice());

    use crypto_box::aead::Aead;
    let sender = crypto_box::PublicKey::from(epk);
    let recipient = crypto_box::SecretKey::from(*sk);
    let sealed_box = crypto_box::SalsaBox::new(&sender, &recipient);
    let plain = sealed_box.decrypt(nonce, ciphertext).ok()?;
    String::from_utf8(plain).ok()
}

#[cfg(test)]
mod provision_tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    /// A REAL `sodium_crypto_box_seal()` payload produced by the production PHP
    /// (libsodium 1.x). Using a captured vector rather than one of our own means
    /// this asserts interop with the origin, not self-consistency.
    const LIBSODIUM_PK: &str = "udoJBDkUCX7xbLkFydw+7UjX8eLTvLmdfMtOby0/UFY=";
    const LIBSODIUM_SK: &str = "JalE9p9FkbseEg4xMCUp3+dN176YTnIM15M7LUQEGP0=";
    const LIBSODIUM_SEALED: &str = "Uxyvbz/qzQZQYJkZIH0yCG046XEdlKffK/C9H6+WKG5aGcJpzVBPGvOgTy9IyLEBgQ8eapWRy0gNWndz4/mL+yn0enNrW/W24hZKfo9mPVPaTvWZsTsHcDkArJFd7GH40wI=";
    const LIBSODIUM_PLAIN: &str = "123456789:AAHsealed-interop-token-abcdefghijklmnop";

    fn key32(b64: &str) -> [u8; 32] {
        let raw = STANDARD.decode(b64).expect("base64");
        let mut out = [0u8; 32];
        out.copy_from_slice(&raw);
        out
    }

    #[test]
    fn opens_a_token_sealed_by_libsodium() {
        let pk = key32(LIBSODIUM_PK);
        let sk = key32(LIBSODIUM_SK);
        let sealed = STANDARD.decode(LIBSODIUM_SEALED).expect("base64");
        assert_eq!(
            open_sealed_token(&sealed, &pk, &sk).as_deref(),
            Some(LIBSODIUM_PLAIN)
        );
    }

    #[test]
    fn refuses_a_box_sealed_to_another_install() {
        let pk = key32(LIBSODIUM_PK);
        let mut wrong = key32(LIBSODIUM_SK);
        wrong[31] ^= 0x01;
        let sealed = STANDARD.decode(LIBSODIUM_SEALED).expect("base64");
        assert_eq!(open_sealed_token(&sealed, &pk, &wrong), None);
    }

    #[test]
    fn pm_auth_encoding_and_endpoint_match_the_origin() {
        // b64url, no padding — the origin's pm_auth_b64u().
        // FF FE FD -> 111111 111111 111011 111101 -> "_" "_" "7" "9"
        assert_eq!(b64u([0xffu8, 0xfe, 0xfd]), "__79");
        assert_eq!(b64u_decode("__79"), vec![0xff, 0xfe, 0xfd]);
        assert!(b64u_decode("not base64 !!").is_empty());

        // `pm_auth_endpoint()` takes the last segment of the route; the client
        // derives it from the URL, query included.
        assert_eq!(endpoint_of("https://pencarimovie.com/wp-json/pencarimovie-server/v1/resolve-file?bot_id=1"), "resolve-file");
        assert_eq!(endpoint_of("https://pencarimovie.com/wp-json/fastdownloader/v1/media-ids"), "media-ids");
        assert_eq!(endpoint_of("https://x/v1/media-ids/"), "media-ids");
        assert_eq!(endpoint_of(""), "unknown");
    }

    /// The token file is process-wide state, so the tests that write it MUST NOT
    /// run concurrently. Measured without this: `…_are_built_when_a_token_exists`
    /// deletes the file (there was no previous token) while
    /// `…_follow_the_token` is between building the headers and re-reading
    /// `auth_token()`, so it saw "token present" next to an empty header list and
    /// failed with `left: []`.
    fn token_file_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        &LOCK
    }

    /// The shipping header wrapper, with a token definitely present.
    ///
    /// `pm_auth_headers_follow_the_token` can pass by taking the early-return
    /// path (no token on this machine), which would hide a regression. This
    /// writes a dummy token, checks the exact header names, and restores the
    /// previous state — the engine re-mints on its next lease, so it is safe.
    #[test]
    fn pm_auth_headers_are_built_when_a_token_exists() {
        let _guard = token_file_lock().lock().unwrap_or_else(|e| e.into_inner());
        let path = auth_token_path();
        let previous = std::fs::read(&path).ok();

        save_auth_token("v1.dummy.claims", crate::storage::now() + 3600);
        let headers = crate::httpc::pm_auth_headers(
            "GET",
            "https://pencarimovie.com/wp-json/pencarimovie-server/v1/resolve-file?bot_id=1",
        );

        match previous {
            Some(bytes) => {
                let _ = std::fs::write(&path, bytes);
            }
            None => clear_auth_token(),
        }

        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec!["X-PM-Auth", "X-PM-Sign-Pub", "X-PM-Ts", "X-PM-Nonce", "X-PM-Proof"],
            "the engine must send all five PM-Auth headers"
        );
        // 64-byte Ed25519 signature -> 86 base64url chars.
        assert_eq!(headers[4].1.len(), 86);
        assert_eq!(headers[0].1, "v1.dummy.claims");
    }

    /// The shipping header wrapper: exactly five headers when a token is held,
    /// and nothing at all when it is not (so the legacy path stays in charge).
    /// Reads storage only — never writes.
    #[test]
    fn pm_auth_headers_follow_the_token() {
        let _guard = token_file_lock().lock().unwrap_or_else(|e| e.into_inner());
        let headers = crate::httpc::pm_auth_headers(
            "GET",
            "https://pencarimovie.com/wp-json/pencarimovie-server/v1/resolve-file?bot_id=1",
        );
        if auth_token().is_empty() {
            assert!(headers.is_empty(), "headers attached without a token");
            return;
        }
        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec!["X-PM-Auth", "X-PM-Sign-Pub", "X-PM-Ts", "X-PM-Nonce", "X-PM-Proof"]
        );
        assert!(!headers[0].1.is_empty(), "empty token");
        assert!(!headers[4].1.is_empty(), "empty proof");
        // A proof must be 64 bytes → 86 base64url chars.
        assert_eq!(headers[4].1.len(), 86);
    }

    fn pm_auth_proof_is_a_real_ed25519_signature() {
        use ed25519_dalek::{Signer as _, Verifier as _};
        let mut sk_bytes = [0u8; 32];
        for b in sk_bytes.iter_mut() {
            *b = rand::random::<u8>();
        }
        let sk = ed25519_dalek::SigningKey::from_bytes(&sk_bytes);
        let pk = sk.verifying_key();

        let ts = 1_790_000_000_i64;
        let nonce = "0123456789abcdef";
        // Same string the origin rebuilds in pm_auth_verify_proof().
        let msg = format!("GET\nresolve-file\n{ts}\n{nonce}");
        let sig = sk.sign(msg.as_bytes());
        assert!(pk.verify(msg.as_bytes(), &sig).is_ok());
        // …and it does not verify for a different endpoint.
        let other = format!("GET\nmedia-ids\n{ts}\n{nonce}");
        assert!(pk.verify(other.as_bytes(), &sig).is_err());
    }

    #[tokio::test]
    #[ignore = "hits the live origin; run: cargo test -- --ignored"]
    async fn live_pm_auth_authorises_without_the_shared_secret() {
        use ed25519_dalek::Signer as _;

        // In-memory key: the test must not touch the install's stored key.
        let mut sk_bytes = [0u8; 32];
        for b in sk_bytes.iter_mut() {
            *b = rand::random::<u8>();
        }
        let sk = ed25519_dalek::SigningKey::from_bytes(&sk_bytes);
        let pk = sk.verifying_key().to_bytes();

        let device = crate::diagnostics::get_device_id();
        let lease_url = format!(
            "{}/provision-session?_nocache={}",
            crate::config::wp_api_base(),
            crate::storage::now()
        );
        let lease = crate::httpc::http_json_h(
            &lease_url,
            &[],
            "GET",
            25,
            &[
                ("X-PM-Sign-Pub".to_string(), b64u(pk)),
                ("X-PM-Device".to_string(), device.clone()),
            ],
        )
        .await;

        let token = lease.get("auth_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
        assert!(!token.is_empty(), "origin sent no auth_token: {lease}");
        assert!(
            !lease.get("install_id").and_then(|v| v.as_str()).unwrap_or("").is_empty(),
            "origin sent no install_id: {lease}"
        );
        let bot_id = lease.get("bot_id").and_then(|v| v.as_i64()).unwrap_or(0);
        assert!(bot_id > 0);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(25))
            .build()
            .expect("client");
        let probe = format!(
            "{}/resolve-file?bot_id={}&short_code=pm_selftest",
            crate::config::wp_api_base(),
            bot_id
        );

        // No X-API-Secret and no ticket on any of these — proof only.
        async fn call(
            client: &reqwest::Client,
            url: &str,
            token: &str,
            pk: &[u8; 32],
            sk: &ed25519_dalek::SigningKey,
            signed_endpoint: &str,
            ts: i64,
            nonce: &str,
        ) -> u16 {
            use ed25519_dalek::Signer as _;
            let msg = format!("GET\n{}\n{}\n{}", signed_endpoint, ts, nonce);
            let sig = sk.sign(msg.as_bytes());
            let resp = client
                .get(url)
                .header("X-PM-Auth", token)
                .header("X-PM-Sign-Pub", b64u(*pk))
                .header("X-PM-Ts", ts.to_string())
                .header("X-PM-Nonce", nonce)
                .header("X-PM-Proof", b64u(sig.to_bytes()))
                .send()
                .await
                .expect("request failed");
            resp.status().as_u16()
        }

        let now = crate::storage::now();
        let nonce = new_nonce();
        let ok = call(&client, &probe, &token, &pk, &sk, "resolve-file", now, &nonce).await;
        assert!(ok != 401 && ok != 403, "PM-Auth was refused (status {ok})");

        // Replay of the same nonce must be refused.
        let replay = call(&client, &probe, &token, &pk, &sk, "resolve-file", now, &nonce).await;
        assert!(replay == 401 || replay == 403, "replayed nonce accepted (status {replay})");

        // A proof signed for another endpoint must be refused.
        let wrong_ep = call(&client, &probe, &token, &pk, &sk, "media-ids", now, &new_nonce()).await;
        assert!(wrong_ep == 401 || wrong_ep == 403, "cross-endpoint proof accepted (status {wrong_ep})");

        // A stale timestamp must be refused.
        let stale = call(&client, &probe, &token, &pk, &sk, "resolve-file", now - 300, &new_nonce()).await;
        assert!(stale == 401 || stale == 403, "stale proof accepted (status {stale})");
    }

    #[test]
    fn refuses_truncated_and_tampered_payloads() {
        let pk = key32(LIBSODIUM_PK);
        let sk = key32(LIBSODIUM_SK);
        let sealed = STANDARD.decode(LIBSODIUM_SEALED).expect("base64");

        // Shorter than an ephemeral key + tag: no room for a box at all.
        assert_eq!(open_sealed_token(&sealed[..40], &pk, &sk), None);

        // One flipped ciphertext byte must fail the Poly1305 tag.
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x80;
        assert_eq!(open_sealed_token(&tampered, &pk, &sk), None);

        // A different ephemeral key means a different shared secret.
        let mut swapped = sealed.clone();
        swapped[0] ^= 0xFF;
        assert_eq!(open_sealed_token(&swapped, &pk, &sk), None);
    }

    /// Live end-to-end: the origin must seal the token to our key (no plaintext
    /// `bot_token` in the body) and mint a ticket the protected routes accept.
    #[tokio::test]
    #[ignore = "hits the live origin; run: cargo test -- --ignored"]
    async fn live_lease_seals_the_token_and_mints_a_usable_ticket() {
        use base64::Engine as _;

        // In-memory keypair: the test must not touch the install's storage.
        let mut sk_bytes = [0u8; 32];
        for b in sk_bytes.iter_mut() {
            *b = rand::random::<u8>();
        }
        let secret = crypto_box::SecretKey::from(sk_bytes);
        let pk = crypto_box::PublicKey::from(&secret).to_bytes();

        let device = crate::diagnostics::get_device_id();
        let headers = vec![
            ("X-PM-Pub".to_string(), STANDARD.encode(pk)),
            ("X-PM-Device".to_string(), device.clone()),
        ];
        let url = format!(
            "{}/provision-session?_nocache={}_{}",
            crate::config::wp_api_base(),
            crate::storage::now(),
            rand::random::<u16>()
        );
        let resp = crate::httpc::http_json_h(&url, &[], "GET", 25, &headers).await;

        // `session::truthy` is private to that module; the origin answers
        // `"ok": 1` (a number), so both shapes must be accepted here.
        let ok = resp
            .get("ok")
            .map(|v| v.as_bool().unwrap_or(false) || v.as_i64().unwrap_or(0) != 0)
            .unwrap_or(false);
        assert!(ok, "lease failed: {resp}");
        assert!(
            resp.get("bot_token").is_none(),
            "origin sent a PLAINTEXT token even though we offered a key: {resp}"
        );

        let sealed_b64 = resp
            .get("sealed_token")
            .and_then(|v| v.as_str())
            .expect("sealed_token missing");
        let sealed = STANDARD.decode(sealed_b64).expect("sealed base64");
        let token = open_sealed_token(&sealed, &pk, &sk_bytes).expect("sealed token did not open");

        // A Telegram bot token: <digits>:<30-40 url-safe chars>.
        let (id_part, secret_part) = token.split_once(':').expect("not a bot token");
        assert!(id_part.chars().all(|c| c.is_ascii_digit()), "bad token id: {token}");
        assert!(secret_part.len() >= 30, "bad token secret: {token}");

        let ticket = resp.get("ticket").and_then(|v| v.as_str()).expect("ticket missing");
        let bot_id = resp.get("bot_id").and_then(|v| v.as_i64()).unwrap_or(0);
        assert!(bot_id > 0 && ticket.contains('.'), "bad ticket/bot_id");

        // The ticket must open a protected route on its own: no X-API-Secret.
        let probe = format!(
            "{}/resolve-file?bot_id={}&short_code=pm_selftest",
            crate::config::wp_api_base(),
            bot_id
        );
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(25))
            .build()
            .expect("client");
        let status = client
            .get(&probe)
            .header("X-PM-Ticket", ticket)
            .header("X-PM-Device", &device)
            .send()
            .await
            .expect("probe failed")
            .status()
            .as_u16();
        assert!(
            status != 401 && status != 403,
            "ticket was refused by the protected route (status {status})"
        );
    }
}
