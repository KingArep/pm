# 🎬 PencariMovie Server

<p align="center">
  <strong>Self-hosted Telegram MTProto media stream bridge & local file downloader for Stremio, Nuvio, Eclipse Music, and web clients.</strong><br>
  Local Execution • Direct MTProto Transport • Zero Cloud Relay
</p>

<p align="center">
  <a href="#-quick-install">Quick Install</a> •
  <a href="#-1-click-cloud-deployment">Cloud Deploy</a> •
  <a href="#-docker--environment-variables">Docker & .env</a> •
  <a href="#-stremio-nuvio--eclipse-music-setup">App Setup</a> •
  <a href="#-server-security--remote-token-auth">Security & Auth</a> •
  <a href="#-cli-commands-pms">CLI Usage</a> •
  <a href="#-features">Features</a> •
  <a href="#-legal-disclaimer--dmca-policy">Legal & DMCA</a>
</p>

---

## 💡 What is PencariMovie Server?

**PencariMovie Server** is a lightweight, self-hosted streaming utility and protocol bridge connecting client applications to Telegram's MTProto API. It converts user-authorized Telegram file references into local HTTP streams with range-request (`HTTP 206`) seek support for media players including **Stremio**, **Nuvio**, and web browsers.

- **Client-Side MTProto Bridge**: Directly streams authorized media to the user's local player over MTProto ([`grammers`](https://codeberg.org/Lonami/grammers), compiled into the binary).
- **No Remote File Hosting**: The server does not host, store, or re-transmit copyrighted files; streams originate from Telegram's network directly to the local client.
- **Local & Private**: Runs on the user's own hardware (localhost / private LAN) without passing credentials or media bytes to external proxy servers.
- **Cross-Platform**: Available as a native Windows tray application, Linux daemon, Android APK, Termux script, and Docker container.

---

## 🗂️ Repository layout

This repository **is** the Rust server: the crate is at the root, and the
dashboard is compiled into the binary. There is no PHP at runtime.

```
Cargo.toml   the engine: axum + grammers. `cargo build --release` -> one binary.
src/         the engine sources (routes, telegram, matcher, assets, auth, ...).
public/      the dashboard (Nuvio shell + the downloader overlays), embedded at
             COMPILE TIME by src/assets.rs. Never moved: the addon UI and every
             release artifact stage from this path.
desktop/     the release launchers (start / stop / restart / tray / autostart),
             copied next to the binary in each tarball.
docs/        internal design notes (kept locally, not published).
legacy/      superseded implementations - see legacy/README.md. Not built,
             not released, not deployed.
storage/     runtime state (sessions, auth, caches). Gitignored: only
             `.gitkeep` is tracked.
android/     NOT TRACKED. The APK projects are a local build tree because they
             hold the release signing key; only the built APK is published, as
             a release asset. See ANDROID-SIGNING.md.
```

Deployment in one line each: `Dockerfile` builds the crate from source;
`Dockerfile.release` packages the prebuilt musl binary; `heroku.yml` and
`render.yaml` run the published image — all of them the **Rust** engine. The
PHP/FrankenPHP engine that preceded it lives in [`legacy/php/`](legacy/php).

Follow-up, deliberately out of scope for the layout refactor: the root installers
(`pencarimovie-linux.sh`, `pencarimovie-termux.sh`, `pencarimovie-windows.bat`,
`update.ps1`) still carry PHP-era fast paths that start a bundled `frankenphp`/
`php -S` when one is present in the install directory. They download and launch
the Rust tarballs, so the fallback is dead weight rather than a bug — see
`legacy/php/README.md` § "The installers did NOT move".

---

## 🚀 Quick Install

Launch the server with a single command on your platform of choice:

#### 📱 Android (APK for TV / Phone / Tablet)

> [**📥 Download Android APK (telegra.my/apk)**](https://telegra.my/apk) - universal
> _(Install on your Android TV, phone, or tablet, tap **Start Server**, and stream)_
> _(The link serves the `universal` build so it runs on any device. Smaller per-ABI APKs
> (`arm64-v8a`, `armeabi-v7a`) are on the
> [APK releases page](https://github.com/aiskendi/pencarimovie-server/releases/tag/apk-server-latest).
> The app updates itself from that release feed.)_

#### 🪟 Windows (10 / 11)

Run in **PowerShell**:

```powershell
irm telegra.my/win | iex
```

_(Runs in the background with a System Tray icon. Manage anytime with `pms start` / `pms stop`)._

#### 🐧 Linux

Run in terminal:

```bash
curl -fsSL telegra.my/linux | bash
```

#### 🍏 macOS (Apple Silicon & Intel)

Run in terminal:

```bash
curl -fsSL telegra.my/mac | bash
```

#### 🐳 Docker (Any OS / NAS / VPS)

```bash
docker run -d \
  --name pencarimovie-server \
  --restart unless-stopped \
  -p 8088:8088 \
  -v pencarimovie-data:/app/storage \
  ghcr.io/aiskendi/pencarimovie-server:latest
```

Once started, open the web dashboard:
👉 **`http://127.0.0.1:8088`** _(or your local LAN IP printed in terminal)_

---

## ☁️ 1-Click Cloud Deployment

Deploy your personal PencariMovie Server to the cloud with a single click:

| Platform    | One-Click Deploy                                                                                                                                                                                                           | Description                                                                |
| :---------- | :------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | :------------------------------------------------------------------------- |
| **Orkestr** | [![Deploy on Orkestr](https://img.shields.io/badge/Deploy%20on-Orkestr-0070f3?style=for-the-badge&logo=docker&logoColor=white)](https://orkestr.eu/new?repo=https%3A%2F%2Fgithub.com%2Faiskendi%2Fpencarimovie-server.git) | Quick container setup on European cloud infrastructure.                    |
| **Railway** | [![Deploy on Railway](https://railway.com/button.svg)](https://railway.com/deploy/ZHHNFC)                                                                                                                                  | Pre-configured template with automatic networking and Docker build.        |
| **Render**  | [![Deploy to Render](https://render.com/images/deploy-to-render-button.svg)](https://render.com/deploy?repo=https://github.com/aiskendi/pencarimovie-server)                                                               | Deploys using native Docker configuration in `render.yaml`.                |
| **Heroku**  | [![Deploy to Heroku](https://www.herokucdn.com/deploy/button.svg)](https://heroku.com/deploy?template=https://github.com/aiskendi/pencarimovie-server&env[SERVER_TOKEN]=66ff097863dedb7ccd8401d5ffe616f2)                  | Deploys via `app.json` container stack with a pre-configured access token. |

> 💡 **Permanent Addon URL on Cloud / PaaS**:
> Always set a static **`SERVER_TOKEN`** (e.g. `66ff097863dedb7ccd8401d5ffe616f2`) in your cloud provider's environment variables. This keeps your Stremio / Nuvio manifest URL (`https://<your-app>.up.railway.app/<token>/manifest.json`) permanent so you never need to re-install addons when your container restarts or redeploys.

---

## 🐳 Docker & Environment Variables

PencariMovie Server can be fully configured using a `.env` file or container environment variables, allowing you to set static passwords, tokens, and ports.

### Docker Compose (`docker-compose.yml`)

```yaml
services:
  pencarimovie:
    image: ghcr.io/aiskendi/pencarimovie-server:latest
    container_name: pencarimovie-server
    restart: unless-stopped
    ports:
      - "${PORT:-8088}:8088"
    env_file:
      - .env
    volumes:
      - ./storage:/app/storage
```

### Environment Variables (`.env`)

Create a `.env` file in your project directory:

```ini
# Server Port (auto-assigned by PaaS or default 8088)
PORT=8088

# Remote Access & Security (Permanent token and dashboard password)
SERVER_PASSWORD=your_secure_password
SERVER_TOKEN=66ff097863dedb7ccd8401d5ffe616f2

# Optional: Cloudflare Zero Trust Named Tunnel Token (auto-connects on startup)
TUNNEL_TOKEN=eyJh...

# Optional: Custom Telegram Bot API Token (overrides guest auto-provisioning)
BOT_TOKEN=123456789:ABCdefGHIjklMNOpqrsTUVwxyz

# Optional: a whole bot POOL from the environment (comma / space / newline
# separated). Use this on any host that runs more than one instance: the pool
# file lives on each instance's own disk, so bots added in the dashboard are
# only visible to the instance that happened to answer the request.
# BOT_TOKENS=111111111:AAA...,222222222:BBB...,333333333:CCC...

# Optional: Local Network IP Override (if running behind multi-adapter Docker bridge)
# LAN_IP=192.168.1.100

# Optional: Performance Tuning (0 = auto; the defaults suit most hosts)
# PM_INFLIGHT=0
# PM_MAX_STREAMS=0
# PM_PIPELINE=0

# Optional: Where sessions, auth and caches are stored
# PM_APP_ROOT=/app/storage
```

### Configuration Variables Reference

| Variable          | Default              | Description                                                                                                                                                                                                                                                                                                    |
| :---------------- | :------------------- | :------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `PORT`            | `8088`               | Port where the server listens (or provided by PaaS runtime).                                                                                                                                                                                                                                                   |
| `SERVER_PASSWORD` | `123456`             | Password for dashboard access on public IPs, tunnels and cloud hosts. LAN is password-free.                                                                                                                                                                                                                    |
| `SERVER_TOKEN`    | _(auto-generated)_   | Static access token for public addon URLs (`/<token>/manifest.json`); makes the link permanent.                                                                                                                                                                                                                |
| `BOT_TOKEN`       | _(auto-provision)_   | Telegram Bot API token. If omitted, the server mints and rotates guest bots.                                                                                                                                                                                                                                   |
| `BOT_TOKENS`      | _(auto-provision)_   | Pool of bot tokens, comma/space/newline separated (alias `TG_BOT_TOKENS`). **Required for a multi-instance deploy** — the on-disk pool is per instance, so a bot added in the dashboard is only used by the instance that stored it. Every instance logs in all of these and downloads are spread across them. |
| `PM_APP_ROOT`     | `storage/`           | Where sessions, auth and caches live.                                                                                                                                                                                                                                                                          |
| `PM_INFLIGHT`     | _(auto)_             | Maximum concurrent requests (`0` = auto).                                                                                                                                                                                                                                                                      |
| `PM_MAX_STREAMS`  | `0`                  | Maximum simultaneous streams. `0` = **unlimited** (the default). Set a number only on a host with a short request timeout — past the cap a viewer gets a fast `503` + `Retry-After` instead of dying on the proxy timeout.                                                                                     |
| `PM_PIPELINE`     | _(auto)_             | Download pipeline depth (`0` = auto).                                                                                                                                                                                                                                                                          |
| `PM_CACHE_DB`     | `storage/cache.redb` | Cache database path; `off` keeps the cache in memory only.                                                                                                                                                                                                                                                     |
| `PM_LISTEN_HOST`  | _(auto)_             | Bind address: loopback locally, all interfaces on a PaaS.                                                                                                                                                                                                                                                      |

---

## 📺 Stremio, Nuvio & Eclipse Music Setup

### 1. Stremio Setup

1. Open the dashboard at `http://127.0.0.1:8088` and click **Addon / Stremio** in the top navigation.
2. **Local Sync (Recommended)**: Click **Install via Stremio API Sync** to push the addon across all your Stremio devices automatically with 1 click.
3. **Manual / Web**: Copy your manifest URL (`http://<LAN-IP>:8088/manifest.json` or `http://<SERVER-IP>:8088/<token>/manifest.json` on public servers) and paste it into Stremio's Addon search box.

### 2. Nuvio Setup

1. Open **Nuvio** on your TV, phone, or tablet connected to the same Wi-Fi.
2. Go to **Profile** ➔ **Content & Discovery** ➔ **Addons**.
3. Enter your manifest URL:
   - **Local Wi-Fi / LAN**: `http://<YOUR-LAN-IP>:8088/manifest.json`
   - **Remote / VPS / Tunnel**: `http://<YOUR-SERVER-IP>:8088/<token>/manifest.json` (or your HTTPS tunnel URL).

### 3. Eclipse Music Setup

1. Open the **Eclipse Music** app (`https://eclipsemusic.app`) on your device.
2. Go to **Settings** ➔ **Connections** ➔ **Add Connection** ➔ **Addon**.
3. Enter your Eclipse manifest URL: `http://<YOUR-LAN-IP>:8088/eclipse/manifest.json` (or `http://127.0.0.1:8088/eclipse`).
4. Search and stream tracks directly with direct seekable playback.

---

## 🔒 Server Security & Remote Token Auth

PencariMovie Server includes built-in security to keep private servers safe when hosted remotely:

- **Local Access is Password-Free**: Requests from localhost (`127.0.0.1`) and private home Wi-Fi (RFC-1918 subnets like `192.168.x.x` or `10.x.x.x`) bypass authentication completely.
- **Public & VPS Protection**: Requests from public IP addresses or Cloudflare Tunnels are protected by password auth (`SERVER_PASSWORD`, default `123456`).
- **Remote Token Routing**: Remote media players authenticate cleanly via path tokens in the manifest URL:
  ```text
  http://<vps-ip>:8088/<token>/manifest.json
  ```
- **CLI Password & Token Management**:
  ```bash
  pms password <new_password>   # Changes server password
  pms reset-password           # Resets password to default (123456)
  pms token                    # Prints current access token
  pms token rotate             # Generates a new access token (invalidates old one)
  ```

---

## 🛠️ CLI Commands (`pms`)

The installer registers a global `pms` command on your system:

```bash
pms start              # Starts server in background (checks for updates)
pms stop               # Stops server and background helper processes
pms restart            # Restarts the server
pms tunnel             # Enables Cloudflare Quick Tunnel (or Named Tunnel if token set)
pms autostart [on|off] # Configures auto-start on system boot
pms password <new>     # Sets the server password
pms reset-password     # Resets password to default (123456)
pms token              # Displays the active 32-character access token
pms token rotate       # Rotates the access token
pms uninstall          # Completely removes server, configuration, and CLI commands
```

_(Works across Windows, macOS, Linux, and Termux)._

---

## ✨ Features

- **🔌 Plug & Play Local Setup**: Runs directly on user machines with auto-configured local session handling.
- **⚡ High-Throughput Streaming**: HTTP 206 range-request support for instant video and audio seeking.
- **📺 Stremio & Nuvio Protocol Compatible**: Implements the Stremio Addon v3 protocol specification for media playback.
- **🎛️ Custom Upstream Addon Bridging**: Easily integrate and toggle custom third-party metadata catalogs.
- **💻 Local Web Player Interface**: Built-in responsive web dashboard for local file access and player playback.
- **📡 Home Network LAN Access**: Access streams on connected TV and mobile devices via local Wi-Fi (`http://<LAN-IP>:8088`).
- **☁️ Optional Secure Tunneling**: Connect remotely using Cloudflare Tunnel integration (`pms tunnel`).
- **🤖 Bot Pool Rotation**: Configure multiple Bot API credentials to balance high-concurrency personal transfers.
- **⚡ System Service Integration**: Built-in system tray (`pencarimovie-server --tray`: native on Windows, `yad` on Linux, no script beside the binary) and Android background service support.

---

## ⚖️ Legal Disclaimer & DMCA Policy

### Disclaimer

**PencariMovie Server** is an open-source, non-commercial software tool designed for personal media management, protocol translation, and client-side streaming of user-accessible Telegram files.

1. **No Content Hosting**: This software and its developers **do not host, store, cache, upload, or transmit any media files or copyrighted material**. All media transfers occur directly between the client user, the local software instance, and Telegram's infrastructure.
2. **Third-Party Indexing & Catalogs**: Any metadata, filenames, or search results displayed through client addons or third-party catalog feeds are provided by external sources. The project maintainers do not own, control, or operate third-party Telegram channels or upload services.
3. **User Responsibility**: Users of this software are solely responsible for ensuring that their use complies with local copyright laws, intellectual property regulations, and Telegram's Terms of Service. The developers explicitly disclaim any liability for copyright infringement, unauthorized access, or misuse of this software by third parties.

### Notice & Takedown (DMCA / Copyright Inquiries)

If you are a copyright owner, content producer, or legal representative wishing to report unauthorized references or request exclusion of specific titles from discovery endpoints:

- Please submit a formal written notice detailing:
  1. The copyrighted work claimed to have been infringed;
  2. The specific identification or URL/query where the reference occurs;
  3. Your contact information (name, organization, email, and phone number);
  4. A statement affirming that you hold the copyright or are authorized to act on behalf of the copyright owner.
- Send all DMCA and copyright inquiries to: **`dmca@pencarimovie.com`** (or submit an issue to the repository maintainer).
- Notices are reviewed expeditiously, and qualifying metadata references or index entries will be promptly delisted and excluded.

---

## 🔒 Open Source & Privacy

- **Open Source**: Licensed under GPL-3.0.
- **Local Isolation**: Media requests, streams, and sessions are processed locally without third-party tracking or cloud relays.
- **Secure Boundaries**: Admin actions (settings, bot pool, tunnel triggers) are restricted to local requests only and blocked across public tunnels.

---

## 🙏 Credits & Acknowledgments

Built on the shoulders of these fantastic open-source projects:

- [**Rust**](https://www.rust-lang.org/) — The engine and its single-binary build.
- [**grammers**](https://codeberg.org/Lonami/grammers) — Telegram MTProto client library.
- [**axum**](https://github.com/tokio-rs/axum) — HTTP server and router.
- [**cloudflared**](https://github.com/cloudflare/cloudflared) — Cloudflare tunnel client enabling seamless TryCloudflare quick tunnels.

---

<p align="center">
  <sub>Open-source project hosted at <a href="https://github.com/aiskendi/pencarimovie-server">github.com/aiskendi/pencarimovie-server</a></sub>
</p>
