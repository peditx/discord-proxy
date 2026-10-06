<p align="center">
  <img src="src-tauri/icons/icon.png" width="96" alt="Discord Proxy">
</p>

<h1 align="center">Discord Proxy</h1>

<p align="center">
  <b>Route every Discord connection through the proxy you pick.</b><br>
  A small Windows app built with Tauri 2 — HTTP, SOCKS4 and SOCKS5, authenticated proxies included.
</p>

<p align="center">
  <a href="readme-fa.md">فارسی</a> · <a href="readme-tr.md">Türkçe</a> · <a href="readme-ru.md">Русский</a>
</p>

![Discord Proxy](docs/screenshot.png)

## Features

- HTTP / HTTPS, SOCKS5 (username &amp; password, IPv4 and IPv6) and SOCKS4 / SOCKS4a upstream proxies
- A local relay that every Discord connection is funnelled through
- An optional Windows system-wide proxy that only you can switch on
- Launch / kill Discord from inside the app, with the proxy applied
- Live status: relay state, active proxy, connection count
- Minimize to tray, with Connect / Disconnect / Quit in the tray menu
- Self-update: check for a newer release and install it from the app

## How it works

```
Discord ──► 127.0.0.1:17999 (local relay) ──► your proxy ──► discord.com
```

1. Add a proxy and make it active.
2. Start the **local relay** — an HTTP proxy listening on `127.0.0.1`.
3. Launch Discord from the app (it is started with `--proxy-server`), or turn on the
   **Windows system proxy** switch so a Discord you open yourself goes through the relay
   as well.

The relay exists because Chromium cannot send proxy credentials on its own and does not
support authenticated SOCKS5. The relay handles both and pushes everything through a
single gate.

### Proxy types

| Type | Support |
| --- | --- |
| HTTP / HTTPS | `CONNECT` tunneling with Basic authentication |
| SOCKS5 | username / password, IPv4 and IPv6 |
| SOCKS4 / SOCKS4a | ✔ |

## Windows system proxy

The **Windows system proxy** switch points every application on the machine at the local
relay. It is:

- **never enabled on its own** — only your own click on the switch, or on
  *Start with system proxy* in the updater dialog, can turn it on;
- **refused when Windows is using a PAC script** (`AutoConfigURL`) — Windows ignores a
  plain proxy server in that case anyway, so the app reports an error instead;
- **restored automatically** when the relay stops or the app closes, so the system is
  never left pointing at a dead proxy.

### Update.exe and the updater session

`Update.exe` is a .NET program, not Chromium — it only reads Windows' own proxy settings
and would bypass the relay entirely. When you launch Discord, the app offers to switch the
system proxy on **for the update only**:

- the dialog offers *OK* (do nothing) and *Start with system proxy*;
- once started, the session switches the system proxy off again **20 seconds after the
  updater goes quiet**;
- if you have already enabled the system proxy yourself, the offer never appears and
  nothing is touched.

## Self-update

The **Check for update** button in the top bar asks GitHub for the latest release:

| Label | Meaning |
| --- | --- |
| Check for update | not checked yet, or the check failed |
| Latest version | this build is the newest one |
| Update to vX.Y.Z | a newer release is waiting — clicking it downloads the installer and restarts the app |

On startup the app also checks quietly in the background; a failure there changes nothing.

## Install

Download `Discord.Proxy_<version>_x64-setup.exe` from
[Releases](https://github.com/peditx/discord-proxy/releases) and run it. The installer is
NSIS; updates installed from inside the app use the same file.

## Settings

Settings and saved proxies are stored in:

```
%APPDATA%\com.peditx.discordproxy\store.json
```

## Build

The app is built on GitHub Actions only — see
[.github/workflows/build.yml](.github/workflows/build.yml). Every push to `main` produces
the NSIS installer as an artifact; pushing a tag (`v1.2.3`) also attaches it to a release.

Local build (Windows, optional):

```bash
cargo tauri build
```

## Development

```bash
cargo tauri dev
```

There is no frontend build step — the raw HTML/CSS/JS in [src/](src/) is loaded as-is and
talks to the Rust backend through `window.__TAURI__.core.invoke`.

### Project layout

```
src/                     UI (no build step)
src-tauri/src/
  lib.rs                 state + Tauri commands
  relay.rs               local relay (plain HTTP and CONNECT)
  dial.rs                upstream dialer: HTTP / SOCKS5 / SOCKS4
  sys.rs                 Windows registry, finding / launching Discord
  store.rs               models + JSON persistence
.github/workflows/
  build.yml              Windows build and release
docs/
  screenshot.png         screenshot used above
```

## Credits

Designed by **PeDitX** · [peditx.ir](https://peditx.ir) · 2026
