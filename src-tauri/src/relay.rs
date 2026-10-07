use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::dial;
use crate::store::ProxyEntry;

pub struct Relay {
    pub port: u16,
    pub handle: Option<JoinHandle<()>>,
    pub conns: Arc<AtomicUsize>,
    pub upstream: Arc<RwLock<Option<ProxyEntry>>>,
    /// True while every live tunnel should close itself - flipped by `stop`,
    /// flipped back when a new relay is installed.
    cut: Arc<watch::Sender<bool>>,
    /// CONNECT targets worth showing the voice module. It filters; the relay
    /// just hands over what it parsed.
    voice: mpsc::Sender<String>,
}

impl Relay {
    pub fn new(upstream: Arc<RwLock<Option<ProxyEntry>>>, voice: mpsc::Sender<String>) -> Self {
        let (cut, _) = watch::channel(false);
        Self {
            port: 0,
            handle: None,
            conns: Arc::new(AtomicUsize::new(0)),
            upstream,
            cut: Arc::new(cut),
            voice,
        }
    }

    pub fn running(&self) -> bool {
        self.handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    pub fn stop(&mut self) {
        // Live tunnels go down with the listener. Without this the button only
        // stops *new* connections and keeps routing whatever is already open,
        // which reads exactly like the relay never went down.
        let _ = self.cut.send_replace(true);
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        // Deliberately not zeroing `conns`: live connections outlive the accept
        // loop and will subtract from it, which would wrap the counter to a
        // huge number. It drains to 0 on its own as they close.
    }

    /// Take the listener over and start accepting on it.
    pub fn install(&mut self, listener: TcpListener, port: u16) {
        self.stop();
        let _ = self.cut.send_replace(false);
        let conns = self.conns.clone();
        let upstream = self.upstream.clone();
        let cut = self.cut.clone();
        let voice = self.voice.clone();
        self.port = port;
        self.handle = Some(tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    // Back off instead of spinning — accept() errors here are
                    // transient (EMFILE under a connection storm, etc.).
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let upstream = upstream.clone();
                let conns = conns.clone();
                let cut = cut.clone();
                let voice = voice.clone();
                tokio::spawn(async move {
                    conns.fetch_add(1, Ordering::SeqCst);
                    let _ = handle_conn(sock, upstream, cut.subscribe(), voice).await;
                    conns.fetch_sub(1, Ordering::SeqCst);
                });
            }
        }));
    }
}

/// Bind before taking the state lock, so no lock is held across an await.
pub async fn bind(port: u16) -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))
}

struct Request {
    host: String,
    port: u16,
    connect: bool,
    forward: Vec<u8>,
}

/// Tell the client why we gave up, then hand the error back to the caller.
async fn fail(client: &mut TcpStream, status: &str, why: &str) -> Result<(), String> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{why}\n",
        why.len() + 1
    );
    let _ = client.write_all(resp.as_bytes()).await;
    Err(why.to_string())
}

/// Handshake budget: a client that connects and says nothing, or an upstream
/// that accepts and then goes quiet, would otherwise hold this task - and the
/// connection count with it - until the OS gave up on its own (~21s).
const HANDSHAKE: std::time::Duration = std::time::Duration::from_secs(10);

async fn handle_conn(
    mut client: TcpStream,
    upstream: Arc<RwLock<Option<ProxyEntry>>>,
    mut cut: watch::Receiver<bool>,
    voice: mpsc::Sender<String>,
) -> Result<(), String> {
    // A stop that lands between accept() and this task starting would slip
    // past `changed()`, which only reports values sent after we subscribe.
    let stopping = *cut.borrow();
    if stopping {
        return Err("relay stopped".to_string());
    }
    // Nagle on this leg holds small writes back for a delayed-ACK round (up to
    // ~200ms on Windows) and Discord is nothing but small writes. The upstream
    // leg is already nodelay'd in `dial`.
    let _ = client.set_nodelay(true);
    let head = match tokio::time::timeout(HANDSHAKE, dial::read_headers(&mut client)).await {
        Ok(r) => r?,
        Err(_) => return Err("client sent no request within 10s".to_string()),
    };
    let entry = upstream.read().await.clone();
    let auth = entry.as_ref().and_then(dial::basic_auth_value);
    // Without a reply the client just sits there until it times out.
    let req = match parse_head(&head, auth) {
        Ok(r) => r,
        Err(e) => return fail(&mut client, "400 Bad Request", &e).await,
    };
    // Discord's voice WebSocket is the only handle on where voice UDP will
    // land, and it is being CONNECTed right here. Hand the host over; what
    // counts as a voice server is decided over there.
    if req.connect && crate::voice::is_voice_host(&req.host) {
        let _ = voice.try_send(req.host.clone());
    }

    let dialed =
        tokio::time::timeout(HANDSHAKE, dial::dial(entry.as_ref(), &req.host, req.port)).await;
    let mut up = match dialed {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return fail(&mut client, "502 Bad Gateway", &e).await,
        Err(_) => {
            return fail(
                &mut client,
                "504 Gateway Timeout",
                "the upstream proxy did not answer within 10s",
            )
            .await
        }
    };

    if req.connect {
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;
    } else {
        up.write_all(&req.forward)
            .await
            .map_err(|e| e.to_string())?;
    }

    // Stop relay closes the live tunnels too - without this select the copy
    // would happily keep relaying long after the button was pressed.
    tokio::select! {
        _ = tokio::io::copy_bidirectional(&mut client, &mut up) => {}
        _ = cut.changed() => {}
    }
    Ok(())
}

fn parse_head(head: &[u8], auth: Option<String>) -> Result<Request, String> {
    let text = String::from_utf8_lossy(head).into_owned();
    let mut lines = text.split("\r\n");
    let first = lines.next().ok_or("empty request")?.to_string();
    let headers: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_string).collect();

    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().ok_or("malformed request line")?.to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(&target, 443)?;
        return Ok(Request {
            host,
            port,
            connect: true,
            forward: Vec::new(),
        });
    }

    // absolute-form (what a proxy is supposed to get) or origin-form (fall back to Host:)
    let (authority, path) = match target.split_once("://") {
        Some((_scheme, rest)) => {
            let cut = rest
                .find(|c| c == '/' || c == '?' || c == '#')
                .unwrap_or(rest.len());
            (rest[..cut].to_string(), rest[cut..].to_string())
        }
        None => {
            let host = header_value(&headers, "host")
                .ok_or("request has no absolute URI and no Host header")?;
            (host, target.clone())
        }
    };
    let authority = match authority.rsplit_once('@') {
        Some((_user, a)) => a.to_string(),
        None => authority,
    };
    let (host, port) = split_host_port(&authority, 80)?;
    // `GET http://example.com HTTP/1.1` has no path; origin-form needs one.
    let path: &str = if path.is_empty() { "/" } else { path.as_str() };

    // ponytail: everything below is sent origin-form through a tunnel, because
    // `dial` always CONNECTs (or opens a raw socket). A plain-HTTP destination
    // therefore needs the upstream HTTP proxy to accept CONNECT on port 80,
    // which strict ones refuse. Upgrade path: pass absolute-form through
    // untouched when the upstream is HTTP.
    let mut out = format!("{method} {path} {version}\r\n");
    for line in &headers {
        let name = line.split(':').next().unwrap_or("");
        if name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("proxy-authorization")
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if let Some(auth) = auth {
        out.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    out.push_str("\r\n");

    Ok(Request {
        host,
        port,
        connect: false,
        forward: out.into_bytes(),
    })
}

fn header_value(headers: &[String], wanted: &str) -> Option<String> {
    headers.iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(wanted)
            .then(|| value.trim().to_string())
    })
}

/// `host:port`, `[v6]:port`, or a bare host (default port when the port is missing).
fn split_host_port(raw: &str, default_port: u16) -> Result<(String, u16), String> {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or("unterminated IPv6 literal")?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| format!("bad port in {raw}"))?,
            None => default_port,
        };
        return Ok((host.to_string(), port));
    }
    match raw.rsplit_once(':') {
        // ponytail: a bare (unbracketed) IPv6 literal would split wrongly here;
        // proxies always send either a name, an IPv4, or `[v6]:port`.
        Some((host, port)) => {
            let port: u16 = port.parse().map_err(|_| format!("bad port in {raw}"))?;
            Ok((host.to_string(), port))
        }
        None => Ok((raw.to_string(), default_port)),
    }
}
