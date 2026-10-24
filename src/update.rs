//! Desktop self-update: check the GitHub release feed, download the matching
//! artifact, verify its published SHA-256, and hand the archive to the shipped
//! `apply-update` script.
//!
//! Why a script applies the update instead of this process replacing itself:
//! Windows locks a running image (the replacement must happen after the process
//! exits), and every platform needs the server restarted behind the tray or
//! autostart entry. `apply-update.bat` / `apply-update.sh` wait for this process,
//! extract over the install directory (never touching `storage/`), and restart.
//!
//! Nothing here runs automatically: `/api/update/check` is read-only and
//! `/api/update/apply` is a privileged (loopback or token) action, so a LAN
//! client cannot push a binary onto the host.

use serde_json::Value;

use crate::config;

const REPO: &str = "aiskendi/pencarimovie-server";
const API_LATEST: &str = "https://api.github.com/repos/aiskendi/pencarimovie-server/releases/latest";
const APK_VERSION_JSON: &str =
    "https://github.com/aiskendi/pencarimovie-server/releases/download/apk-server-latest/version.json";
const APK_RELEASES_URL: &str =
    "https://github.com/aiskendi/pencarimovie-server/releases/tag/apk-server-latest";

/// `linux-x86_64`, `windows-x86_64`, `macos-aarch64` — the artifact suffix the
/// release workflow publishes. `None` on a platform we do not build for.
/// The public release page, for the places that can only LINK to an update
/// (the streams-list notice, a container, a bare build).
pub fn releases_url() -> String {
    format!("https://github.com/{REPO}/releases/latest")
}

pub fn apk_releases_url() -> String {
    APK_RELEASES_URL.to_string()
}

pub fn platform_key() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("windows", "x86_64") => Some("windows-x86_64"),
        ("macos", "aarch64") => Some("macos-aarch64"),
        ("macos", "x86_64") => Some("macos-x86_64"),
        _ => None,
    }
}

/// `v2.8.9` → `(2, 8, 9)`; tolerates `2.8.8-5-g9fc84e1-dirty` (build stamps).
fn numeric(version: &str) -> (u64, u64, u64) {
    let head = version.trim().trim_start_matches('v');
    let head = head.split('-').next().unwrap_or(head);
    let mut it = head.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

fn is_newer(latest: &str, current: &str) -> bool {
    numeric(latest) > numeric(current)
}

fn asset_of(release: &Value, suffix: &str) -> Option<String> {
    release
        .get("assets")
        .and_then(|a| a.as_array())?
        .iter()
        .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
        .find(|n| n.ends_with(suffix))
        .map(|n| n.to_string())
}

fn asset_url(release: &Value, name: &str) -> Option<String> {
    release
        .get("assets")
        .and_then(|a| a.as_array())?
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .map(|u| u.to_string())
}

async fn latest_release() -> Value {
    let client = crate::httpc::client();
    let req = client
        .get(API_LATEST)
        .header("User-Agent", config::user_agent())
        .header("Accept", "application/vnd.github+json");
    match tokio::time::timeout(std::time::Duration::from_secs(15), req.send()).await {
        Ok(Ok(res)) if res.status().is_success() => res.json::<Value>().await.unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

async fn latest_apk_release() -> Value {
    let client = crate::httpc::client();
    let req = client
        .get(APK_VERSION_JSON)
        .header("User-Agent", config::user_agent())
        .header("Accept", "application/json");
    match tokio::time::timeout(std::time::Duration::from_secs(10), req.send()).await {
        Ok(Ok(res)) if res.status().is_success() => res.json::<Value>().await.unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn build_apk_check_answer(client: &str, current: &str, release: &Value) -> Value {
    let effective_current = if !client.trim().is_empty() {
        client.trim().to_string()
    } else {
        current.to_string()
    };
    if release.is_null() {
        return serde_json::json!({
            "ok": false,
            "app_managed": true,
            "current": effective_current,
            "client": client,
            "build": config::build_id(),
            "error": "Could not reach the Android APK release feed.",
        });
    }
    let latest = release
        .get("versionName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let apk_url = release
        .get("apkUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let download_url = if !apk_url.is_empty() {
        apk_url
    } else {
        APK_RELEASES_URL.to_string()
    };
    let update_available = !latest.is_empty() && is_newer(&latest, &effective_current);
    serde_json::json!({
        "ok": true,
        "app_managed": true,
        "current": effective_current.clone(),
        "client": client,
        "build": config::build_id(),
        "current_display": config::clean_version(&effective_current),
        "latest_display": config::clean_version(&latest),
        "latest": latest,
        "update_available": update_available,
        "asset": Value::Null,
        "can_apply": false,
        "apply_blocked_reason": "app_managed",
        "apply_hint": "Updates are installed via the PencariMovie Server APK",
        "notes_url": APK_RELEASES_URL,
        "releases_url": download_url,
    })
}

/// True when this binary lives INSIDE an app shell that ships its own updater.
///
/// On Android the binary is unpacked from the APK, and the APK updates itself from
/// the `apk-server-latest` feed, so a desktop release is not something this process
/// may install. The same is true of ANY build started with `--app-version` (the app
/// shell told us its own product version), which is why the check is not Android-only:
/// a desktop-format release downloaded over an app install would replace a working
/// binary with an incompatible one.
fn app_managed_for(client: &str) -> bool {
    cfg!(target_os = "android") || !client.trim().is_empty()
}

/// Environment / filesystem markers that mean "this process runs in a container".
///
/// A container's writable layer is thrown away by the next `docker compose up`
/// and the published IMAGE is what carries the new build, so replacing the
/// binary inside the running container is not an update — it is a change that
/// disappears on restart. `PORT` is deliberately not a marker (the PHP launchers
/// export it on a desktop).
const CONTAINER_MARKERS: [&str; 8] = [
    "DYNO",                // Heroku
    "RENDER",              // Render
    "FLY_APP_NAME",        // Fly.io
    "RAILWAY_ENVIRONMENT", // Railway
    "K_SERVICE",           // Google Cloud Run
    "WEBSITE_SITE_NAME",   // Azure App Service
    "KUBERNETES_SERVICE_HOST",
    "PM_PAAS", // explicit opt-in for any other container host
];

/// Pure so it can be tested without touching the filesystem or the environment.
pub fn container_predicate(get: impl Fn(&str) -> bool, container_file: bool) -> bool {
    container_file || CONTAINER_MARKERS.iter().any(|k| get(k))
}

pub fn runs_in_container() -> bool {
    let files = std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists();
    container_predicate(|k| crate::storage::env(k).is_some(), files)
}

/// Why the button may not self-update on this host. Empty = it may.
fn apply_blocked_reason(asset: Option<&str>) -> &'static str {
    if runs_in_container() {
        "container"
    } else if asset.is_none() {
        "no_asset"
    } else if !apply_script().is_file() {
        "no_script"
    } else {
        ""
    }
}

const CONTAINER_HINT: &str =
    "This server runs in a container, so it updates by pulling a new image — run `docker compose pull && docker compose up -d` (or redeploy the service) instead of replacing the binary.";

/// `GET /api/update/check` — never downloads anything.
pub async fn check() -> Value {
    let current = config::app_version();
    let client = config::client_version();
    let is_android = app_managed_for(&client);
    let cache_key = if is_android {
        "check_android"
    } else {
        "check_desktop"
    };

    if let Some(hit) = crate::cache::get("update", cache_key) {
        return (*hit).clone();
    }

    if is_android {
        let release = latest_apk_release().await;
        let res = build_apk_check_answer(&client, &current, &release);
        let ttl = if res.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            3600
        } else {
            300
        };
        crate::cache::put("update", cache_key, &res, ttl, 0);
        return res;
    }

    let release = latest_release().await;
    if release.is_null() {
        // In a container the actionable answer does not depend on the feed: the
        // image is the unit of update, so say that instead of "check your network".
        let res = if runs_in_container() {
            serde_json::json!({
                "ok": false,
                "container": true,
                "current": current,
                "build": config::build_id(),
                "apply_blocked_reason": "container",
                "apply_hint": CONTAINER_HINT,
                "releases_url": releases_url(),
                "error": CONTAINER_HINT,
            })
        } else {
            serde_json::json!({
                "ok": false,
                "current": current,
                "build": config::build_id(),
                "error": "Could not reach the release feed. Check this machine's network, then retry.",
            })
        };
        crate::cache::put("update", cache_key, &res, 300, 0);
        return res;
    }
    let latest = release
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let Some(key) = platform_key() else {
        let res = serde_json::json!({
            "ok": false,
            "current": current,
            "build": config::build_id(),
            "latest": latest,
            "error": "No update artifact is published for this platform.",
        });
        crate::cache::put("update", cache_key, &res, 3600, 0);
        return res;
    };
    let asset = asset_of(&release, &format!("pencarimovie-server-{key}.tar.gz"));
    // `can_apply` is what the update bar acts on: a container, a release with no
    // artifact for THIS arch, and a bare build with no apply script can all SEE
    // the update but must not offer a button that cannot work.
    let reason = apply_blocked_reason(asset.as_deref());
    let res = serde_json::json!({
        "ok": true,
        "current": current,
        "build": config::build_id(),
        // Clean X.Y.Z forms for display: a badge or an update bar must never
        // show a git stamp like `2.8.9-4-g02e7dea`, and `latest` already starts
        // with "v" so the UI must not prepend another one.
        "current_display": config::clean_version(&current),
        "latest_display": config::clean_version(&latest),
        "latest": latest,
        "update_available": is_newer(&latest, &current),
        "asset": asset,
        "can_apply": reason.is_empty(),
        "apply_blocked_reason": reason,
        "apply_hint": if reason == "container" { CONTAINER_HINT } else { "" },
        "notes_url": release.get("html_url").and_then(|v| v.as_str()).unwrap_or(""),
        "releases_url": releases_url(),
    });
    crate::cache::put("update", cache_key, &res, 3600, 0);
    res
}

fn updates_dir() -> std::path::PathBuf {
    let dir = crate::storage::storage_path("updates");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn apply_script() -> std::path::PathBuf {
    let name = if cfg!(windows) {
        "apply-update.bat"
    } else {
        "apply-update.sh"
    };
    crate::storage::app_root().join(name)
}

/// `POST /api/update/apply` — download the artifact, verify its hash, then run
/// the apply script detached. Returns as soon as the script is started; the
/// server stops a moment later, so the caller may lose the connection.
pub async fn apply() -> Value {
    // Belt and braces: an app-managed build must never download a DESKTOP tarball,
    // even if a future caller reaches `apply()` without going through `check()`.
    let client = config::client_version();
    if app_managed_for(&client) {
        return serde_json::json!({
            "ok": false,
            "app_managed": true,
            "current": config::app_version(),
            "error": "This engine runs inside an app that updates itself. Use the app's own update banner.",
        });
    }
    // Downloading a tarball inside a container replaces the binary in a layer
    // that the next container start throws away — and `process::exit(0)` below
    // would bounce the container back onto the OLD image. Refuse before the
    // download, with the command that actually works.
    if runs_in_container() {
        return serde_json::json!({
            "ok": false,
            "container": true,
            "apply_blocked_reason": "container",
            "current": config::app_version(),
            "error": CONTAINER_HINT,
        });
    }
    let info = check().await;
    if info.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return info;
    }
    if info.get("update_available").and_then(|v| v.as_bool()) != Some(true) {
        return serde_json::json!({
            "ok": true,
            "up_to_date": true,
            "current": config::app_version(),
            "message": "This is already the latest version.",
        });
    }
    let Some(asset) = info.get("asset").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return serde_json::json!({
            "ok": false,
            "error": "The release has no artifact for this platform.",
        });
    };
    // Check for the apply script BEFORE downloading: on a bare build (cargo
    // target dir, or a package extracted without the launcher scripts) the swap
    // is impossible, and downloading 30 MB first only to fail made the button
    // look dead.
    let script = apply_script();
    if !script.is_file() {
        return serde_json::json!({
            "ok": false,
            "manual": true,
            "asset": asset,
            "releases_url": releases_url(),
            "error": format!(
                "This build has no {} next to the binary, so it cannot replace itself. Download the release and extract it over this folder.",
                script.file_name().unwrap_or_default().to_string_lossy()
            ),
        });
    }
    let release = latest_release().await;
    let Some(url) = asset_url(&release, &asset) else {
        return serde_json::json!({ "ok": false, "error": "Asset URL is missing from the release." });
    };
    let dest = updates_dir().join(&asset);
    if !crate::httpc::download_file(&url, &dest, 600).await {
        return serde_json::json!({ "ok": false, "error": "Download failed. Check this machine's network." });
    }
    // Integrity: the release ships a `<asset>.sha256` sidecar holding just the hash.
    if let Some(sum_url) = asset_url(&release, &format!("{asset}.sha256")) {
        let want = crate::httpc::get_contents(&sum_url, 30, Default::default())
            .await
            .unwrap_or_default();
        let want = want.split_whitespace().next().unwrap_or("").to_lowercase();
        let got = match std::fs::read(&dest) {
            Ok(bytes) => {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(&bytes);
                format!("{:x}", h.finalize())
            }
            Err(_) => String::new(),
        };
        if want.len() == 64 && got != want {
            let _ = std::fs::remove_file(&dest);
            return serde_json::json!({
                "ok": false,
                "error": "The downloaded archive failed its SHA-256 check; nothing was installed.",
            });
        }
    }
    let started = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", script.to_string_lossy().as_ref(), dest.to_string_lossy().as_ref()])
            .spawn()
    } else {
        std::process::Command::new("sh")
            .arg(&script)
            .arg(&dest)
            .spawn()
    };
    match started {
        Ok(_) => {
            // Let the HTTP response go out, then stop so the script can replace
            // the binary and start the new one behind the tray/autostart entry.
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                let _ = crate::cache::flush();
                std::process::exit(0);
            });
            serde_json::json!({
                "ok": true,
                "starting": true,
                "asset": asset,
                "message": "Update downloaded and verified. The server restarts now.",
            })
        }
        Err(e) => serde_json::json!({ "ok": false, "error": format!("Could not start the updater: {e}") }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_versions_compare_and_ignore_build_stamps() {
        assert_eq!(numeric("v2.8.9"), (2, 8, 9));
        assert_eq!(numeric("2.8.8-5-g9fc84e1-dirty"), (2, 8, 8));
        assert!(is_newer("v2.9.0", "2.8.8"));
        assert!(is_newer("v2.8.9", "2.8.8-5-gabc"));
        assert!(!is_newer("v2.8.8", "2.8.8-5-gabc"));
        assert!(!is_newer("v2.7.9", "2.8.0"));
    }

    #[test]
    fn an_app_shell_is_never_offered_a_desktop_artifact() {
        // `--app-version` means an app shell owns the update path, so the desktop
        // GitHub feed must be refused (the APK updates from `apk-server-latest`).
        assert!(app_managed_for("1.0.55"));
        assert!(app_managed_for(" 1.0.55 "));
        assert_eq!(app_managed_for(""), cfg!(target_os = "android"));

        // When a newer APK is available in version.json, it reports update_available
        // and points to the APK feed without offering desktop artifacts.
        let newer = serde_json::json!({
            "versionName": "1.0.72",
            "versionCode": 10072,
            "apkUrl": "https://example.test/pencarimovie-server.apk"
        });
        let ans = build_apk_check_answer("1.0.71", "", &newer);
        assert_eq!(ans["ok"], true);
        assert_eq!(ans["app_managed"], true);
        assert_eq!(ans["update_available"], true);
        assert_eq!(ans["current_display"], "1.0.71");
        assert_eq!(ans["latest_display"], "1.0.72");
        assert_eq!(ans["can_apply"], false);
        assert_eq!(ans["releases_url"], "https://example.test/pencarimovie-server.apk");

        // Same version means no update needed.
        let same = build_apk_check_answer("1.0.72", "", &newer);
        assert_eq!(same["update_available"], false);
    }

    #[test]
    fn container_markers_block_self_update() {
        // A desktop has none of these markers and no `/.dockerenv`.
        assert!(!container_predicate(|_| false, false));
        // Docker / Podman marker file, and every PaaS marker, mean "update the
        // image", not "replace the binary".
        assert!(container_predicate(|_| false, true));
        assert!(container_predicate(|k| k == "RENDER", false));
        assert!(container_predicate(|k| k == "KUBERNETES_SERVICE_HOST", false));
        assert!(container_predicate(|k| k == "PM_PAAS", false));
        // `PORT` is exported by the desktop launchers too — never a marker.
        assert!(!container_predicate(|k| k == "PORT", false));
    }

    #[test]
    fn platform_keys_are_the_published_names() {
        // The keys must match the artifact suffixes release.yml uploads.
        let key = platform_key().expect("this test runs on a supported platform");
        let published = ["linux-x86_64", "linux-aarch64", "windows-x86_64", "macos-aarch64", "macos-x86_64"];
        assert!(published.contains(&key), "{key}");
    }

    #[test]
    fn each_arch_picks_its_own_artifact() {
        // Multi-arch release: every platform key must select exactly its own
        // tarball, never a sibling's (a Pi must not get the x86_64 build).
        let names = [
            "linux-x86_64",
            "linux-aarch64",
            "windows-x86_64",
            "macos-aarch64",
            "macos-x86_64",
        ];
        let assets: Vec<Value> = names
            .iter()
            .flat_map(|k| {
                [
                    serde_json::json!({ "name": format!("pencarimovie-server-{k}.tar.gz") }),
                    serde_json::json!({ "name": format!("pencarimovie-server-{k}.tar.gz.sha256") }),
                ]
            })
            .collect();
        let release = serde_json::json!({ "assets": assets });
        for k in names {
            let want = format!("pencarimovie-server-{k}.tar.gz");
            assert_eq!(asset_of(&release, &want).as_deref(), Some(want.as_str()), "{k}");
        }
        // An arch with no published artifact gets None (the caller then reports
        // `no_asset` instead of downloading the wrong binary).
        assert!(asset_of(&release, "pencarimovie-server-linux-armv7.tar.gz").is_none());
    }
}
