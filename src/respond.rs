//! Port of `fd_json()` (backend.php:448) and the Stremio JSON writer.

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use serde_json::Value;

const CORS: [(&str, &str); 5] = [
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Methods", "GET, POST, OPTIONS, DELETE, HEAD"),
    (
        // Kept in step with the preflight handler in `routes.rs`, which echoes
        // whatever the browser asked for (the Eclipse web client sends its own
        // `X-Client-*` headers).
        "Access-Control-Allow-Headers",
        "Content-Type, Authorization, X-API-Secret, X-Auth-Token, X-Client-Platform, X-Client-Version, X-Requested-With, Range",
    ),
    (
        "Access-Control-Expose-Headers",
        "Accept-Ranges, Content-Range, Content-Length, Content-Type",
    ),
    ("Access-Control-Allow-Private-Network", "true"),
];

/// Stamp the full CORS set (including `Access-Control-Allow-Private-Network`)
/// onto a hand-built response.
///
/// Chrome's Private Network Access blocks a page on a public HTTPS origin
/// (`https://eclipsemusic.app`) from reading a **loopback** response unless the
/// RESPONSE says `Access-Control-Allow-Private-Network: true` — passing the
/// `OPTIONS` preflight is not enough, it only earns the request the right to be
/// SENT. The media responses in [`crate::download`] are built by hand, so they
/// are stamped here at the single dispatch point: every branch (206, 503, 400,
/// HEAD) inherits it, and `Access-Control-Allow-Origin: *` is left untouched.
pub fn allow_private_network(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    for (k, v) in CORS {
        if !h.contains_key(k) {
            h.insert(k, axum::http::HeaderValue::from_static(v));
        }
    }
    resp
}

pub fn json_response(status: u16, body: &Value) -> Response {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store, no-cache, must-revalidate")
        .header(header::CONTENT_LENGTH, text.len().to_string());
    for (k, v) in CORS {
        builder = builder.header(k, v);
    }
    if status >= 400 {
        builder = builder.header(header::CONNECTION, "close");
    }
    builder.body(Body::from(text)).unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("{\"ok\":0}"))
            .unwrap()
    })
}

pub fn stremio_json(status: u16, body: &Value, cache_control: &str) -> Response {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::CONTENT_LENGTH, text.len().to_string());
    for (k, v) in CORS {
        builder = builder.header(k, v);
    }
    builder.body(Body::from(text)).unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("{\"ok\":0}"))
            .unwrap()
    })
}

pub fn text_response(status: u16, content_type: &str, body: String) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CONTENT_LENGTH, body.len().to_string())
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap()
        })
}

pub fn redirect(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap()
        })
}

/// Serve an asset whose bytes are already in memory (embedding or disk).
///
/// `bytes::Bytes` is used so the embedded (`&'static`) case is zero-copy — a
/// 3 MB `app.bundle.js` must not be cloned per request.
pub fn bytes_response(content_type: &str, bytes: bytes::Bytes) -> Option<Response> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from(bytes))
        .ok()
}

/// Serve a dashboard asset read from `<app_root>/public`.
///
/// `Cache-Control: no-store` matches what `backend.php:15053` sends for
/// `public/` — the frontend bumps its own `?v=` cache-busters, so an aggressive
/// cache here would serve a stale bundle after an update.
pub fn file_response(path: &std::path::Path, content_type: &str) -> Option<Response> {
    let bytes = std::fs::read(path).ok()?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from(bytes))
        .ok()
}

/// Unknown addon/API path: JSON 404, never an SPA fallback.
pub fn not_found_json() -> Response {
    json_response(404, &serde_json::json!({"ok": 0, "error": "Not found"}))
}

/// PHP `fd_stremio_locked_stream()` (backend.php:1793). Never a 401.
pub fn stremio_locked_stream(base_url: &str) -> Response {
    stremio_json(
        200,
        &serde_json::json!({
            "streams": [{
                "name": "PencariMovie",
                "description": "Addon URL changed or password required.\nOpen PencariMovie and re-install the addon from #addon.",
                "externalUrl": format!("{}/#addon", rtrim(base_url))
            }]
        }),
        "no-cache, no-store, must-revalidate",
    )
}

/// PHP `fd_eclipse_locked_response()` (backend.php:1807).
pub fn eclipse_locked_response(base_url: &str) -> Response {
    stremio_json(
        200,
        &serde_json::json!({
            "error": "Addon URL changed or password required. Open PencariMovie and re-install the addon from #addon.",
            "externalUrl": format!("{}/#addon", rtrim(base_url))
        }),
        "no-cache, no-store, must-revalidate",
    )
}

pub fn rtrim(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}
