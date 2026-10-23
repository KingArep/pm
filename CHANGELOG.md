# Changelog

All notable changes to the PencariMovie Server / Downloader project will be documented in this file.

## [Unreleased]

### Fixed

- **The system tray is now part of the binary, and the icon actually shows.** The
  tray was a separate `powershell.exe` process running `tray.ps1`. Windows stores
  the per-app tray settings in `HKCU\Control Panel\NotifyIconSettings` keyed by the
  **hosting executable**, so that icon shared `powershell.exe`'s single entry with
  every other PowerShell-hosted tray (it read `9Router` on the machine that
  reported this) and could only ever sit in the "hidden icons" flyout; running
  `pencarimovie-server.exe` itself produced no icon at all. Three separate causes,
  all removed:
  - `pencarimovie-server --tray` now creates the icon in-process over
    `Shell_NotifyIconW` (`src/tray.rs`, `user32`/`shell32` only, no new
    dependency). Verified: the icon appears with the tooltip
    `PencariMovie Server - port N` **and** the shell writes a
    `NotifyIconSettings` entry whose `ExecutablePath` is `pencarimovie-server.exe`.
  - The logo is compiled in (`include_bytes!("../tray.ico")`), so the icon is not
    the stock/blank one — and not missing at all — in a folder that holds nothing
    but the executable.
  - **No console window survives a tray start.** `--tray` (and a double-clicked
    exe, which also gets an icon) releases its console with `FreeConsole()` rather
    than hiding it: hiding leaves the process still owning a console, so conhost
    kept the window and the launching `cmd` alive forever. `start-tray.bat` uses a
    plain `start`, so the batch returns at once; verified `MainWindowHandle = 0`
    and no `conhost` child. Because there is no console to redirect,
    [`log::stderr_log`](src/log.rs:73) also appends to `storage/server.log` while
    the tray runs, which is the file "Open log" opens.
  - `tray.ps1` no longer runs the tray; it is a shim that forwards an old
    `HKCU Run` entry to `pencarimovie-server.exe --tray`. Nothing is registered
    for login any more, because the exe was never the login entry — the tray menu
    and `autostart-on.bat` now write the binary's own command line.
- **`--tray` works on Linux too**: the binary writes its embedded tray script
  (`desktop/linux/tray.sh`) to `storage/tray/pm-tray.sh` and runs it, so
  `start-tray.sh`/autostart need only `./pencarimovie-server --tray` (`yad` still
  draws the icon; without it the server runs and prints the URL). macOS keeps the
  LaunchAgent: an `NSStatusBar` item needs AppKit, i.e. a `.app` bundle.

### Changed

- `--tray` / `--no-tray` (and `PM_NO_TRAY`) are documented flags; a
  double-clicked `pencarimovie-server.exe` gets a tray icon as well as the
  browser. `--tray` hides its own console, so the launchers no longer need
  `powershell -WindowStyle Hidden`.

## [2.9.7] - 2026-10-22

### Fixed

- **Series returned "No Streams Found" when opened through an external id** (`tt…`,
  `tmdb:…`). Two separate causes, both on the origin query rather than the matcher:
  - `/stream-files` was sent `year=…` for series. The origin applies it as a real
    filter and series episode rows are un-tagged, so `title=Hey Kay Navin&year=2026`
    answered 0 rows where `title=Hey Kay Navin` answered 10. `year` is now omitted for
    series (matching the PHP at `backend.php:10180`), with a no-year retry for movies.
  - The external-id route never ran the episode ladder. The PHP calls
    `fd_fetch_episode_stream_files(0, …)` (`backend.php:17093`); this port only did the
    title search, so one episode listed 20 files through `tt…` and 86 through
    `pm:post:…`. The route now runs the same ladder with the external id as the origin
    `id`.
- **`--workers` default raised from 1 to 4 on every platform.** `workers × inflight`
  is the `upload.getFile` part budget shared by all viewers, and 1 connection meant 8
  parts for the whole process — a second stream starved the first. A real session at
  `--workers 1` logged repeated `download refused`; at 4 it logged 55/55 served with
  zero refusals. `PM_WORKERS` overrides it.
- **Manifest descriptor trimming no longer floods the log.** One line per dropped
  bridged catalog (3,021 of the last 4,000 log lines with a 300-catalog upstream) is
  now a single summary with `from_bytes`/`to_bytes`/`dropped`.

### Added

- **PHP `MUST NOT REGRESS` matcher cases ported** from
  `legacy/php/test_kudrat_series.php`: 11 cases covering "The Glory" (2022) vs
  "Glory" (2025) and the embedded-name-year titles ("Kudrat 1968: High Council 2026").
  They pin the strict leading-"The" check, the single-word year guard and the
  single-word foreign-indicator exemption.

## [2.3.3] - 2026-09-29

### Added

- **Nuvio TV Smart Player & Web UI Enhancements**:
  - Subtitle rail selection with mouse and touch interaction in the player subtitle modal.
  - Fullscreen auto-rotation, smooth mouse seeking, and auto-progression to next episode.
  - Auto-retry and recovery mechanism for playback stalls and network startup glitches.
  - Unified Sources and Episodes panels with responsive glass-card styling and no title truncation on large screens.
  - Fixed "Continue Watching" series history stack preserving clean back navigation without popstate corruption.
- **Sub-Second Stream Resolution & Fast-Path Metadata Indexing**:
  - Direct metadata lookups for instant stream card generation.
  - Direct PM stream resolution bypasses slow upstream requests and forwards post metadata directly.
  - Strict title matching distinguishing "Title" from "The Title", with full AKA alias search support.
- **Master Upstream Catalog Toggle (`upstream_enabled`)**:
  - One-switch control in Catalog Configuration to toggle upstream addons (e.g. AIOMetadata) without wiping custom URLs.
  - Prevents dashboard hangs by filtering out non-local `up_*` catalogs from web category rows.
- **Same-Origin Stream Cookie Auth**:
  - Cookie credentials automatically forwarded on same-origin requests for seamless remote and Cloudflare Tunnel playback.

## [2.1.8] - 2026-09-18

### Added

- **Server Password Authentication & Token Model**:
  - Remote and public requests (VPS public IP, Cloudflare Tunnel) are guarded by password authentication (default `123456`) and persistent 32-character tokens.
  - Manifest and stream paths support clean token URL routing (`/<token>/manifest.json`, `/<token>/stream/...`).
  - Localhost and private LAN (RFC-1918) requests remain password-free with instant access.
  - CLI management commands: `pms password <new>`, `pms reset-password`, `pms token`, and `pms token rotate` (implemented in [`pencarimovie-linux.sh`](pencarimovie-linux.sh:1), [`pencarimovie-termux.sh`](pencarimovie-termux.sh:1), [`pencarimovie-windows.bat`](pencarimovie-windows.bat:1), and [`auth-write.ps1`](auth-write.ps1:1)).
- **Cloudflare Zero Trust Named Tunnel & Token Persistence**:
  - Support for custom Named Tunnel tokens via Settings UI and `POST /api/tunnel/enable`.
  - Added CLI command `pms tunnel <TOKEN>` across Linux, Windows, and Termux launchers.
  - Dedicated token persistence in `storage/tunnel/token.txt` preserved across server restarts and `pms stop`.
  - Automatic tunnel warm-up in [`warmup-ipc.php`](warmup-ipc.php:1) on `pms start` or `pms restart` when previously enabled.
  - Dynamic discovery of mapped public hostnames matching local listening port from connector logs.
- **VPS & Remote Server Manifest Mode**:
  - Direct IP detection in frontend (`_isIpHost`) preventing public VPS addresses from showing TryCloudflare tunnel badges when tunnels are disabled.
  - Added `mode=server` to [`backend.php`](backend.php:6102) yielding `PencariMovie (Server)` / `org.pencarimovie.addon.server`.
  - Addon modal displays "🌐 Server Manifest" on VPS IPs and automatically selects the Server manifest in Stremio Sync.
  - Dynamically hides the Wi-Fi/LAN vs Localhost toggle when accessing from a public VPS IP or Cloudflare Tunnel.
- **Eclipse Music Addon Integration**:
  - Native discovery and streaming for 500,000+ audio tracks under `/eclipse` routes (`/eclipse/manifest.json`, `/eclipse/search`, `/eclipse/stream/{id}`, `/eclipse/catalog/{id}`, `/eclipse/resolve`).
  - Advanced caption and title parsing in [`backend.php`](backend.php:8165) separating artist, title, and featured collaborations with multi-word connector handling.
- **Stremio HTTP API Sync**:
  - Browser-side Stremio HTTP API sync (`api.strem.io` login → `addonCollectionGet` → `addonCollectionSet`) without transmitting user credentials to the PHP backend.

### Security

- **Caddy Sensitive File Shield**:
  - Added `@blocked` route matcher in [`Caddyfile`](Caddyfile:1) returning HTTP 404 for sensitive files and directories (`storage/*`, `vendor/*`, `bin/*`, `patches/*`, `Caddyfile*`, `*.sh`, `*.bat`, `*.key`, `*.log`, `*.lock`, `*.md`, `*.yml`, `*.yaml`, `composer.json`, `package.json`).
- **Progressive Brute-Force Rate Limiting**:
  - Exponential lockout ladder (30s, 2m, 10m, 30m) for failed password submissions tracked in `storage/cache/auth_lockout.json` returning HTTP 429 with `Retry-After`.
- **Locked Stream Responses**:
  - Unauthenticated remote stream requests receive an informative Stremio stream banner directing users to authenticate via `#addon`, rather than failing silently with 401.
- **Native PHP Static Boundary**:
  - Direct built-in PHP server execution (`php -S ... router.php`) strictly isolates file serving to `public/` via `realpath()` and `str_starts_with()` checks.

### Changed

- **Stream Object Specification**:
  - Playable `/stream` objects comply with AIOStreams specification (`name`, `description`, `url`, `behaviorHints`).
  - Stream URLs end with clean file extensions (`.mp4`, `.flac`, `.mkv`) to guarantee playback compatibility across Stremio Web, Android, and desktop players.
- **Release Packaging**:
  - Updated [`scripts/build-release.bat`](scripts/build-release.bat:1) and [`scripts/package-unix.sh`](scripts/package-unix.sh:1) to include [`pencarimovie-linux.sh`](pencarimovie-linux.sh:1), [`pencarimovie-termux.sh`](pencarimovie-termux.sh:1), [`README.md`](README.md:1), and [`patches/`](patches/) across all release archives.
  - Added automated line ending normalization (CRLF → LF) for all shell scripts and `bin/php` when building Unix packages on Windows.
  - Exported [`update.ps1`](update.ps1:1) to `dist/` alongside [`pencarimovie-windows.bat`](pencarimovie-windows.bat:1).
- **Process Management**:
  - Preserved tunnel token state during `stop.sh` and `stop.bat` process teardown.
  - Improved working directory resolution in Unix start scripts ensuring Caddy directives resolve from the repository root.

## [1.0.1] - 2026-08-29

### Added

- **Stremio & Nuvio Addon Integration**:
  - Implemented Stremio addon manifest and endpoints (`/manifest.json`, `/catalog/...`, `/meta/...`, `/stream/...`) in [`backend.php`](backend.php:2585).
  - Enables direct playback and library browsing from Stremio and Nuvio players with automatic Telegram bot stream resolution.
- **Windows System Tray Helper**:
  - Added background tray management scripts ([`tray.ps1`](tray.ps1:1), [`start-hidden.ps1`](start-hidden.ps1:1)) and tray icons ([`tray.ico`](tray.ico), [`tray.png`](tray.png)).
  - Runs FrankenPHP silently with hidden window and provides tray menu actions for opening the web dashboard and stopping the server.
- **One-File Installers with Start-Time OTA Updates**:
  - Added [`pencarimovie-windows.bat`](pencarimovie-windows.bat:1), [`pencarimovie-linux.sh`](pencarimovie-linux.sh:1), and [`pencarimovie-termux.sh`](pencarimovie-termux.sh:1).
  - Automatically queries GitHub `releases/latest` on start, updates the application files in-place while preserving `storage/` bot session data.
- **Version Check & Minimum Version Enforcement**:
  - Implemented version verification against WordPress API endpoint (`/fastdownloader/v1/version`) with local hourly caching.
  - Added full-screen update notification overlay in [`public/index.html`](public/index.html:27) and [`public/styles.css`](public/styles.css:681).
- **LAN IP Detection & Network Access**:
  - Cross-platform network IP discovery in [`start.bat`](start.bat:1), [`start.sh`](start.sh:1), and [`start-termux.sh`](start-termux.sh:1) displaying local and LAN network URLs upon startup.

### Changed

- **Frontend Streaming UI Overhaul**:
  - Integrated Netflix-style FlixBrowse UI directly into [`public/index.html`](public/index.html:1), [`public/app.js`](public/app.js:1), and [`public/styles.css`](public/styles.css:1).
  - Added rich search overlays, file detail views, inline Telegram stream player, and one-click MadelineProto downloader.
- **Backend Architecture & Security**:
  - Session-based authentication with automatic Telegram bot login via MadelineProto.
  - Eliminated plaintext configuration files; API credentials are securely fetched from the WordPress backend and encrypted with the bot token.
  - Enhanced FrankenPHP storage directory resolution to protect sessions in temp directory environments.
- **Launcher Scripts**:
  - Refactored [`start.bat`](start.bat:1), [`start.sh`](start.sh:1), [`start-termux.sh`](start-termux.sh:1), [`stop.bat`](stop.bat:1), [`stop.sh`](stop.sh:1), [`restart.bat`](restart.bat:1), [`restart.sh`](restart.sh:1), and [`restart-termux.sh`](restart-termux.sh:1) for unified port handling (default `8088`), clean output banners, and process lifecycle management.
- **Project Documentation**:
  - Updated [`README.md`](README.md:1) and [`RELEASE.md`](RELEASE.md:1) with Stremio setup guides, one-line installer commands, and packaging specifications.
