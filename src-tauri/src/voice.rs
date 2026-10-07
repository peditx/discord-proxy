//! Discord voice is UDP, and no HTTP/SOCKS-TCP proxy carries UDP - so voice
//! always went out direct and came back throttled: the 5000<->100ms ping cycle.
//! The fix is a small local TUN adapter that receives only /32 host routes for
//! Discord's voice servers and nothing else - never a default route, so games
//! and every other app on the machine never see it - and pushes those packets
//! into the user's proxy with a SOCKS5 UDP ASSOCIATE. A vendored sing-box
//! (`vendor/sing-box.exe`) owns the adapter.
//!
//! Creating a TUN adapter and interface routes needs admin, so the elevated
//! half is this very exe re-launched with `--voice-pipe=<name>`: it runs
//! `engine_main` instead of the app and never builds a window. The two halves
//! talk over an anonymous named pipe whose name carries a random nonce - a
//! medium-integrity process cannot read the elevated process' argv, so a
//! squatter cannot learn the name from there either.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};

use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, oneshot};

use crate::store::ProxyEntry;
use crate::{dial, sys, App};

/// One command in flight to the helper. The protocol is strictly
/// request/response, so a single reply is ever owed at a time - but requests
/// from different callers still queue up behind it.
struct Req {
    line: String,
    done: oneshot::Sender<Result<(), String>>,
}

pub struct Voice {
    tx: Option<mpsc::Sender<Req>>,
    /// The helper answered RELOAD and is carrying an engine.
    pub ready: bool,
    /// Set by `stop`, so a helper that leaves on purpose is not a crash.
    pub stopping: bool,
    pub error: Option<String>,
    /// Voice hosts the relay has shown us while the engine was up.
    pub seen: usize,
    /// Voice-server IPs actually routed into the TUN.
    pub ips: Vec<Ipv4Addr>,
}

impl Voice {
    pub fn new() -> Self {
        Self {
            tx: None,
            ready: false,
            stopping: false,
            error: None,
            seen: 0,
            ips: Vec::new(),
        }
    }
}

/// Discord's voice WebSocket. The UDP target is a literal IP handed over by
/// the voice payload later, so the CONNECT for this host is the one moment
/// where the destination can still be learned by name.
pub fn is_voice_host(host: &str) -> bool {
    host.trim_end_matches('.')
        .to_ascii_lowercase()
        .ends_with(".discord.media")
}

/// Cloudflare's published IPv4 ranges. `*.discord.media` sits behind them for
/// plain web traffic, and routing those would pull unrelated CDN traffic into
/// the tunnel - fail toward "nothing routed" instead.
const CF_V4: [(Ipv4Addr, u32); 15] = [
    (Ipv4Addr::new(173, 245, 48, 0), 20),
    (Ipv4Addr::new(103, 21, 244, 0), 22),
    (Ipv4Addr::new(103, 22, 200, 0), 22),
    (Ipv4Addr::new(103, 31, 4, 0), 22),
    (Ipv4Addr::new(141, 101, 64, 0), 18),
    (Ipv4Addr::new(108, 162, 192, 0), 18),
    (Ipv4Addr::new(190, 93, 240, 0), 20),
    (Ipv4Addr::new(188, 114, 96, 0), 20),
    (Ipv4Addr::new(197, 234, 240, 0), 22),
    (Ipv4Addr::new(198, 41, 128, 0), 17),
    (Ipv4Addr::new(162, 158, 0, 0), 15),
    (Ipv4Addr::new(104, 16, 0, 0), 13),
    (Ipv4Addr::new(104, 24, 0, 0), 14),
    (Ipv4Addr::new(172, 64, 0, 0), 13),
    (Ipv4Addr::new(131, 0, 72, 0), 22),
];

fn in_cidr(ip: Ipv4Addr, net: Ipv4Addr, bits: u32) -> bool {
    let mask = u32::MAX << (32 - bits);
    u32::from(ip) & mask == u32::from(net) & mask
}

/// Safe to hand the TUN: Discord's own address space, not an address the TUN
/// must not swallow, and not Cloudflare.
fn screen(ip: Ipv4Addr) -> bool {
    if ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_unspecified()
        // `is_reserved` is unstable on CI's stable toolchain: 0/8 + 240/4 by hand.
        || matches!(ip.octets()[0], 0 | 240..=255)
    {
        return false;
    }
    !CF_V4.iter().any(|(net, bits)| in_cidr(ip, *net, *bits))
}

/// A records only: an incomplete IPv6 denylist would be worse than no IPv6 at
/// all, and failing toward v4 leaves voice exactly as broken as it is today.
fn resolve_v4(host: &str) -> Vec<Ipv4Addr> {
    (host, 443)
        .to_socket_addrs()
        .map(|found| {
            found
                .filter_map(|addr| match addr {
                    SocketAddr::V4(v4) => Some(*v4.ip()),
                    SocketAddr::V6(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn reload_payload(entry: Option<&ProxyEntry>) -> String {
    match entry {
        Some(p) => serde_json::json!({
            "host": p.host,
            "port": p.port,
            "username": p.username,
            "password": p.password,
        })
        .to_string(),
        None => "null".to_string(),
    }
}

/// Send one line and wait for its one reply. A caller that never gets an
/// answer gets a sentence instead of a hang.
async fn request(
    tx: &mpsc::Sender<Req>,
    mut line: String,
    wait: std::time::Duration,
) -> Result<(), String> {
    let (done, reply) = oneshot::channel();
    // The helper reads whole lines with read_line - without this terminator
    // it would block forever and every request would time out.
    line.push('\n');
    tx.send(Req { line, done })
        .await
        .map_err(|_| "the voice helper is not running".to_string())?;
    match tokio::time::timeout(wait, reply).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => Err("the voice helper stopped".to_string()),
        Err(_) => Err("the voice helper did not answer in time".to_string()),
    }
}

// ------------------------------------------------------------------ lifecycle

/// Bring the helper up: prove the proxy can carry UDP, ask Windows for admin,
/// connect the control pipe, load the current proxy into the engine.
#[cfg(windows)]
pub async fn start(app: &AppHandle) -> Result<(), String> {
    if app.state::<App>().voice.lock().unwrap().tx.is_some() {
        return Ok(());
    }
    let entry = {
        let state = app.state::<App>();
        let store = state.store.lock().unwrap();
        store.active().cloned()
    }
    .ok_or("add a proxy and set it active first - voice goes through it too")?;

    // The UDP handshake decides this before any prompt appears: a UAC dialog
    // for a proxy that could never have carried voice is just noise.
    tokio::time::timeout(std::time::Duration::from_secs(4), dial::udp_probe(&entry))
        .await
        .map_err(|_| "the proxy did not answer within 4s".to_string())??;

    {
        let state = app.state::<App>();
        let mut v = state.voice.lock().unwrap();
        v.ready = false;
        v.stopping = false;
        v.error = None;
        v.seen = 0;
        v.ips.clear();
    }

    let pipe = format!(r"\\.\pipe\DiscordProxyVoice-{}", sys::random_hex(16)?);
    let arg = format!("--voice-pipe={pipe}");
    tauri::async_runtime::spawn_blocking(move || sys::run_elevated(&arg))
        .await
        .map_err(|e| format!("cannot start the voice helper: {e}"))??;
    // Switched off while the prompt was still on screen.
    if app.state::<App>().voice.lock().unwrap().stopping {
        return Ok(());
    }

    use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};
    let mut server = ServerOptions::new()
        .pipe_mode(PipeMode::Byte)
        .first_pipe_instance(true)
        .create(&pipe)
        .map_err(|e| format!("cannot create the voice control pipe: {e}"))?;
    // The helper retries opening the pipe for 45s in case UAC is still up.
    tokio::time::timeout(std::time::Duration::from_secs(45), server.connect())
        .await
        .map_err(|_| "the voice helper never connected - was the admin prompt answered?")?
        .map_err(|e| format!("the voice pipe failed: {e}"))?;

    let (rd, wr) = tokio::io::split(server);
    let (line_tx, line_rx) = mpsc::channel::<String>(16);
    let (tx, rx) = mpsc::channel::<Req>(32);
    {
        let state = app.state::<App>();
        let mut v = state.voice.lock().unwrap();
        if v.stopping {
            return Ok(()); // dropping `tx` closes the pipe and ends the helper
        }
        // Stored before the first request, so a stop arriving mid-start still
        // finds a sender to drop.
        v.tx = Some(tx.clone());
    }
    tauri::async_runtime::spawn(reader(rd, line_tx));
    tauri::async_runtime::spawn(control(app.clone(), rx, line_rx, wr));

    let line = format!("RELOAD {}", reload_payload(Some(&entry)));
    if let Err(e) = request(&tx, line, std::time::Duration::from_secs(20)).await {
        stop(app);
        return Err(e);
    }
    let state = app.state::<App>();
    let mut v = state.voice.lock().unwrap();
    if v.stopping {
        return Ok(());
    }
    v.ready = true;
    Ok(())
}

/// Drop the sender: the control loop notices, the pipe closes, and the helper
/// treats that EOF as "clean up and exit" - no separate stop message needed.
pub fn stop(app: &AppHandle) {
    let state = app.state::<App>();
    let mut v = state.voice.lock().unwrap();
    v.stopping = true;
    v.tx = None;
    v.ready = false;
    v.error = None;
    v.seen = 0;
    v.ips.clear();
}

/// Follow the active proxy. Called after every change: a switch the engine
/// never hears about keeps forwarding through the proxy that was there before.
pub async fn reload(app: &AppHandle) {
    let tx = match app.state::<App>().voice.lock().unwrap().tx.clone() {
        Some(tx) => tx,
        None => return,
    };
    let entry = {
        let state = app.state::<App>();
        let store = state.store.lock().unwrap();
        store.active().cloned()
    };
    // Whether this proxy can carry UDP at all is the sentence the UI needs
    // when voice stays broken after a switch - the engine cannot tell.
    let probe = match &entry {
        Some(p) => tokio::time::timeout(std::time::Duration::from_secs(4), dial::udp_probe(p))
            .await
            .map_err(|_| "the proxy did not answer within 4s".to_string())
            .and_then(|r| r),
        None => Err("there is no active proxy".to_string()),
    };
    let line = format!("RELOAD {}", reload_payload(entry.as_ref()));
    let result = request(&tx, line, std::time::Duration::from_secs(20)).await;
    let state = app.state::<App>();
    let mut v = state.voice.lock().unwrap();
    match result {
        Ok(()) => v.error = probe.err(),
        Err(e) => v.error = Some(e),
    }
}

// ------------------------------------------------------------------- learning

/// Watch the relay for voice CONNECTs, resolve them, screen them, and route
/// the survivors. Runs for the life of the app; with no engine up it has
/// nothing to say and does nothing.
pub fn spawn_learner(app: AppHandle, mut hosts: mpsc::Receiver<String>) {
    tauri::async_runtime::spawn(async move {
        while let Some(host) = hosts.recv().await {
            if !is_voice_host(&host) {
                continue;
            }
            let tx = {
                let state = app.state::<App>();
                let mut v = state.voice.lock().unwrap();
                if !v.ready {
                    continue;
                }
                v.seen += 1;
                v.tx.clone()
            };
            let tx = match tx {
                Some(tx) => tx,
                None => continue,
            };

            let name = host.clone();
            let ips = tauri::async_runtime::spawn_blocking(move || resolve_v4(&name))
                .await
                .unwrap_or_default();
            for ip in ips {
                if !screen(ip) {
                    continue;
                }
                if app.state::<App>().voice.lock().unwrap().ips.contains(&ip) {
                    continue;
                }
                let line = format!("ADD {ip}");
                match request(&tx, line, std::time::Duration::from_secs(12)).await {
                    Ok(()) => {
                        let state = app.state::<App>();
                        let mut v = state.voice.lock().unwrap();
                        if !v.ips.contains(&ip) {
                            v.ips.push(ip);
                        }
                    }
                    // The adapter may still have been coming up, or the engine
                    // may be mid-restart - the next CONNECT for this host
                    // tries again either way.
                    Err(e) => eprintln!("voice: cannot route {ip}: {e}"),
                }
            }
        }
    });
}

// ------------------------------------------------------------------- pipe IO

#[cfg(windows)]
type PipeRead = tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeServer>;
#[cfg(windows)]
type PipeWrite = tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeServer>;

/// Owns the read half and turns raw chunks into whole reply lines. Keeping it
/// out of the control loop is what lets that loop select between a request
/// channel and a reply channel without holding a buffer borrow across a branch.
#[cfg(windows)]
async fn reader(mut rd: PipeRead, lines: mpsc::Sender<String>) {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = match rd.read(&mut chunk).await {
            Ok(0) | Err(_) => return, // helper said goodbye
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            if lines.send(text).await.is_err() {
                return;
            }
        }
    }
}

/// Strictly serial request/response: one request is written, its reply is
/// awaited, the next request follows. Anything arriving meanwhile queues up.
#[cfg(windows)]
async fn control(
    app: AppHandle,
    mut rx: mpsc::Receiver<Req>,
    mut lines: mpsc::Receiver<String>,
    mut wr: PipeWrite,
) {
    use tokio::io::AsyncWriteExt;
    let mut pending: Option<oneshot::Sender<Result<(), String>>> = None;
    let mut backlog: VecDeque<Req> = VecDeque::new();
    let mut live = true;

    while live {
        // Flush everything the protocol currently allows to go out.
        while pending.is_none() {
            match backlog.pop_front() {
                Some(req) => match wr.write_all(req.line.as_bytes()).await {
                    Ok(()) => pending = Some(req.done),
                    Err(e) => {
                        let _ = req.done.send(Err(format!("the voice pipe broke: {e}")));
                        live = false;
                        break;
                    }
                },
                None => break,
            }
        }
        if !live {
            break;
        }

        tokio::select! {
            next = rx.recv() => match next {
                Some(req) => backlog.push_back(req),
                None => live = false, // stop() dropped the last sender
            },
            line = lines.recv() => match line {
                // The reader is gone, which means the pipe is - EOF on the
                // helper's side, the same thing stop() is built on.
                None => live = false,
                Some(text) => {
                    if let Some(done) = pending.take() {
                        if let Some(rest) = text.strip_prefix("ERR") {
                            let _ = done.send(Err(rest.trim_start().to_string()));
                        } else if text.trim() == "OK" {
                            let _ = done.send(Ok(()));
                        } else {
                            pending = Some(done); // a stray line, not the reply
                        }
                    }
                }
            },
        }
    }

    // Anything still owed an answer is told why instead of never resolving.
    if let Some(done) = pending.take() {
        let _ = done.send(Err("the voice helper stopped".to_string()));
    }
    while let Some(req) = backlog.pop_front() {
        let _ = req.done.send(Err("the voice helper stopped".to_string()));
    }

    let state = app.state::<App>();
    let mut v = state.voice.lock().unwrap();
    v.tx = None;
    v.ready = false;
    v.ips.clear();
    if !v.stopping && v.error.is_none() {
        v.error = Some("the voice helper stopped".to_string());
    }
}

// -------------------------------------------------------------------- engine

/// The elevated half. Blocks until the app's end of the pipe disappears, then
/// tears everything down on the way out.
#[cfg(windows)]
pub fn engine_main(pipe_name: &str) {
    engine::run(pipe_name)
}

#[cfg(not(windows))]
pub fn engine_main(_pipe_name: &str) {}

#[cfg(not(windows))]
pub async fn start(_app: &AppHandle) -> Result<(), String> {
    Err("the voice fix is Windows only".to_string())
}

/// Everything below runs elevated: the TUN adapter, the interface routes, the
/// sing-box child. Pure std - this process never builds the app.
#[cfg(windows)]
mod engine {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::Ipv4Addr;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use serde_json::json;

    const TUN_IF: &str = "DiscordProxyVoice";
    const TUN_ADDR: &str = "198.18.0.1/30";
    const TUN_GW: &str = "198.18.0.2";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn run(pipe_name: &str) {
        let file = match connect(pipe_name) {
            Some(f) => f,
            None => return,
        };
        // Two handles on the same end: one reads lines, one writes replies.
        let reader = match file.try_clone() {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut reader = BufReader::new(reader);
        let mut writer = file;
        let mut sing: Option<Child> = None;
        let mut routes: Vec<Ipv4Addr> = Vec::new();
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break, // the app is gone - EOF means clean up
                Ok(_) => {}
            }
            let text = line.trim().to_string();
            let answer = if let Some(rest) = text.strip_prefix("RELOAD ") {
                reload(rest, &mut sing, &mut routes)
            } else if let Some(rest) = text.strip_prefix("ADD ") {
                add(rest, &mut routes)
            } else {
                Err(format!(
                    "unknown command: {}",
                    text.split_whitespace().next().unwrap_or("")
                ))
            };
            let reply = match answer {
                Ok(()) => "OK\n".to_string(),
                Err(e) => format!("ERR {e}\n"),
            };
            if writer.write_all(reply.as_bytes()).is_err() || writer.flush().is_err() {
                break;
            }
        }
        teardown(&mut sing, &mut routes);
    }

    /// The app's end of the pipe appears a beat after we do (UAC may still be
    /// up), so look for it for the same 45s it was promised.
    fn connect(pipe_name: &str) -> Option<std::fs::File> {
        for _ in 0..450 {
            match fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(pipe_name)
            {
                Ok(f) => return Some(f),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => return None,
            }
        }
        None
    }

    /// New proxy in, engine out. Fail-safe in both directions: the old engine
    /// goes down before the new one is judged, and a bad config ends with
    /// nothing routed rather than the wrong thing routed.
    fn reload(
        payload: &str,
        sing: &mut Option<Child>,
        routes: &mut Vec<Ipv4Addr>,
    ) -> Result<(), String> {
        teardown(sing, routes);
        // No active proxy is "stop routing", not an error.
        if payload.trim() == "null" {
            return Ok(());
        }
        let cfg: serde_json::Value =
            serde_json::from_str(payload).map_err(|e| format!("bad RELOAD payload: {e}"))?;
        let host = cfg["host"]
            .as_str()
            .filter(|h| !h.is_empty())
            .ok_or("the proxy has no host")?;
        let port = cfg["port"]
            .as_u64()
            .filter(|p| (1..=65535).contains(p))
            .ok_or("the proxy has no port")? as u16;

        // A records only, gvisor, and deliberately no auto_route: the /32s
        // below are the whole routing table this adapter will ever get.
        let mut outbound = json!({
            "type": "socks",
            "tag": "proxy-out",
            "server": host,
            "server_port": port,
            "version": "5",
        });
        let user = cfg["username"].as_str().unwrap_or("");
        if !user.is_empty() {
            outbound["username"] = json!(user);
            outbound["password"] = json!(cfg["password"].as_str().unwrap_or(""));
        }
        let config = json!({
            "inbounds": [{
                "type": "tun",
                "tag": "tun-in",
                "interface_name": TUN_IF,
                "address": [TUN_ADDR],
                "mtu": 1500,
                "stack": "gvisor",
                "auto_route": false,
                "strict_route": false,
            }],
            "outbounds": [outbound],
            "route": {"final": "proxy-out"},
        });

        let path = std::env::temp_dir().join("discord-proxy-voice.json");
        let bytes = serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?;
        fs::write(&path, bytes).map_err(|e| format!("cannot write the engine config: {e}"))?;

        *sing = Some(spawn(&path)?);
        // Routes from the last proxy still point at the same adapter, which
        // just came back under a different upstream.
        for ip in routes.clone() {
            if let Err(e) = add_route(ip) {
                teardown(sing, routes);
                return Err(e);
            }
        }
        Ok(())
    }

    fn add(text: &str, routes: &mut Vec<Ipv4Addr>) -> Result<(), String> {
        let ip: Ipv4Addr = text
            .trim()
            .parse()
            .map_err(|_| format!("bad address: {text}"))?;
        if routes.contains(&ip) {
            return Ok(());
        }
        add_route(ip)?;
        routes.push(ip);
        Ok(())
    }

    /// One /32, nothing more. Exit code is the only trustworthy signal - the
    /// text netsh prints is localized.
    fn add_route(ip: Ipv4Addr) -> Result<(), String> {
        let mut last = String::new();
        // The adapter appears a beat after sing-box starts, so the first try
        // can legitimately race it.
        for _ in 0..8 {
            let out = Command::new("netsh")
                .args([
                    "interface",
                    "ipv4",
                    "add",
                    "route",
                    &format!("{ip}/32"),
                    TUN_IF,
                    TUN_GW,
                    "store=active",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            match out {
                Ok(o) if o.status.success() => return Ok(()),
                Ok(o) => last = o.status.to_string(),
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        Err(format!("could not route {ip} through {TUN_IF} ({last})"))
    }

    fn del_route(ip: Ipv4Addr) {
        let _ = Command::new("netsh")
            .args([
                "interface",
                "ipv4",
                "delete",
                "route",
                &format!("{ip}/32"),
                TUN_IF,
                TUN_GW,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }

    /// Kill the engine first: the adapter going away takes the interface with
    /// it, and the explicit deletes below are the belt to that pair of braces.
    fn teardown(sing: &mut Option<Child>, routes: &mut Vec<Ipv4Addr>) {
        if let Some(mut child) = sing.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for ip in routes.drain(..) {
            del_route(ip);
        }
    }

    fn spawn(config: &Path) -> Result<Child, String> {
        let exe = locate()?;
        let log_path = std::env::temp_dir().join("discord-proxy-voice.log");
        let mut last = "sing-box did not start".to_string();
        // A bad config dies in moments; so does a collide with an adapter that
        // is still tearing down - five tries with a fresh look at the log each.
        for _ in 0..5 {
            let log = fs::File::create(&log_path)
                .map_err(|e| format!("cannot open the engine log: {e}"))?;
            let err = log
                .try_clone()
                .map_err(|e| format!("cannot open the engine log: {e}"))?;
            let mut child = Command::new(&exe)
                .args(["run", "-c"])
                .arg(config)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(err))
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| format!("cannot start sing-box: {e}"))?;
            std::thread::sleep(Duration::from_millis(400));
            match child.try_wait() {
                Ok(None) => return Ok(child),
                Ok(Some(status)) => {
                    last = format!("sing-box exited with {status}: {}", log_tail(&log_path));
                }
                Err(e) => {
                    last = format!("cannot watch sing-box: {e}");
                    let _ = child.kill();
                }
            }
            std::thread::sleep(Duration::from_millis(400));
        }
        Err(last)
    }

    fn log_tail(path: &Path) -> String {
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(3);
        lines[start..].join(" | ")
    }

    /// Installed: beside the exe. Dev: cargo puts the exe in target/debug,
    /// two levels below src-tauri where the vendor folder lives.
    fn locate() -> Result<PathBuf, String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot locate sing-box: {e}"))?;
        let dir = exe.parent().ok_or("cannot locate sing-box")?;
        let here = dir.join("vendor").join("sing-box.exe");
        if here.is_file() {
            return Ok(here);
        }
        let dev = dir
            .join("..")
            .join("..")
            .join("vendor")
            .join("sing-box.exe");
        if dev.is_file() {
            return Ok(dev);
        }
        Err("sing-box.exe is missing (vendor/sing-box.exe next to the app)".to_string())
    }
}
