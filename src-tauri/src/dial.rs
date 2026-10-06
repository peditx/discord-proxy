use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::store::{ProxyEntry, ProxyKind};

/// Anything the relay can splice bytes through.
pub trait Streamish: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Streamish for T {}

pub type Stream = Box<dyn Streamish>;

pub async fn dial(proxy: Option<&ProxyEntry>, host: &str, port: u16) -> Result<Stream, String> {
    match proxy {
        None => Ok(Box::new(raw_connect(host, port).await?)),
        Some(p) if p.host.is_empty() || p.port == 0 => {
            Err(format!("proxy \"{}\" has no host/port", p.name))
        }
        Some(p) => match p.kind {
            ProxyKind::Http => http_connect(p, host, port).await,
            ProxyKind::Socks5 => socks5(p, host, port).await,
            ProxyKind::Socks4 => socks4(p, host, port).await,
        },
    }
}

async fn raw_connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let s = TcpStream::connect((host, port))
        .await
        .map_err(|e| format!("cannot reach {host}:{port}: {e}"))?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

async fn proxy_socket(p: &ProxyEntry) -> Result<TcpStream, String> {
    let s = TcpStream::connect((p.host.as_str(), p.port))
        .await
        .map_err(|e| format!("cannot reach proxy {}: {e}", p.endpoint()))?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

// ---------------------------------------------------------------- HTTP / HTTPS

async fn http_connect(p: &ProxyEntry, host: &str, port: u16) -> Result<Stream, String> {
    let mut s = proxy_socket(p).await?;
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if !p.username.is_empty() {
        req.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            basic64(&format!("{}:{}", p.username, p.password))
        ));
    }
    req.push_str("Proxy-Connection: keep-alive\r\n\r\n");

    s.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("proxy write failed: {e}"))?;

    let head = read_headers(&mut s).await?;
    let status = status_code(&head).ok_or("proxy sent an unreadable response")?;
    if status != 200 {
        return Err(format!("proxy refused the CONNECT ({status})"));
    }
    Ok(Box::new(s))
}

pub async fn read_headers(s: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(512);
    let mut one = [0u8; 1];
    while buf.len() < 16 * 1024 {
        let n = s
            .read(&mut one)
            .await
            .map_err(|e| format!("proxy handshake read failed: {e}"))?;
        if n == 0 {
            return Err("proxy closed the connection during the handshake".to_string());
        }
        buf.push(one[0]);
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(buf);
        }
    }
    Err("proxy sent an oversized handshake".to_string())
}

fn status_code(head: &[u8]) -> Option<u16> {
    let line = head.split(|b| *b == b'\n').next()?;
    let text = std::str::from_utf8(line).ok()?;
    text.split_whitespace().nth(1)?.parse().ok()
}

// ---------------------------------------------------------------------- SOCKS5

async fn socks5(p: &ProxyEntry, host: &str, port: u16) -> Result<Stream, String> {
    let mut s = proxy_socket(p).await?;
    let wants_auth = !p.username.is_empty();

    // VER, NMETHODS, methods... — the count byte must match the methods sent,
    // otherwise the proxy sits there waiting for a byte that never comes.
    let mut greet = vec![0x05];
    if wants_auth {
        greet.extend_from_slice(&[0x02, 0x02, 0x00]); // user/pass, then no-auth
    } else {
        greet.extend_from_slice(&[0x01, 0x00]); // no-auth only
    }
    s.write_all(&greet)
        .await
        .map_err(|e| format!("SOCKS5 write failed: {e}"))?;

    let mut chosen = [0u8; 2];
    s.read_exact(&mut chosen)
        .await
        .map_err(|e| format!("SOCKS5 read failed: {e}"))?;
    match chosen[1] {
        0x00 => {}
        0x02 => {
            if !wants_auth {
                return Err("SOCKS5 proxy wants a username/password this proxy has none".into());
            }
            let user = p.username.as_bytes();
            let pass = p.password.as_bytes();
            if user.len() > 255 || pass.len() > 255 {
                return Err("SOCKS5 credentials are longer than 255 bytes".into());
            }
            let mut auth = vec![0x01, user.len() as u8];
            auth.extend_from_slice(user);
            auth.push(pass.len() as u8);
            auth.extend_from_slice(pass);
            s.write_all(&auth)
                .await
                .map_err(|e| format!("SOCKS5 auth write failed: {e}"))?;
            let mut reply = [0u8; 2];
            s.read_exact(&mut reply)
                .await
                .map_err(|e| format!("SOCKS5 auth read failed: {e}"))?;
            if reply[1] != 0x00 {
                return Err("SOCKS5 rejected the username/password".to_string());
            }
        }
        0xFF => return Err("SOCKS5 proxy requires credentials that were not provided".into()),
        other => return Err(format!("SOCKS5 offered an unsupported auth method (0x{other:02x})")),
    }

    let mut req = vec![0x05, 0x01, 0x00];
    req.extend(encode_addr(host)?);
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req)
        .await
        .map_err(|e| format!("SOCKS5 connect write failed: {e}"))?;

    let mut head = [0u8; 4];
    s.read_exact(&mut head)
        .await
        .map_err(|e| format!("SOCKS5 connect read failed: {e}"))?;
    if head[1] != 0x00 {
        return Err(format!(
            "SOCKS5 could not open the connection ({})",
            socks_reply(head[1])
        ));
    }
    // Drain the bound address the proxy reports back.
    let skip = match head[3] {
        0x01 => 4 + 2,
        0x04 => 16 + 2,
        0x03 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len)
                .await
                .map_err(|e| format!("SOCKS5 address read failed: {e}"))?;
            len[0] as usize + 2
        }
        other => return Err(format!("SOCKS5 returned an unknown address type (0x{other:02x})")),
    };
    let mut drain = vec![0u8; skip];
    s.read_exact(&mut drain)
        .await
        .map_err(|e| format!("SOCKS5 address read failed: {e}"))?;
    Ok(Box::new(s))
}

fn encode_addr(host: &str) -> Result<Vec<u8>, String> {
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        let mut v = vec![0x01];
        v.extend_from_slice(&v4.octets());
        return Ok(v);
    }
    if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        let mut v = vec![0x04];
        v.extend_from_slice(&v6.octets());
        return Ok(v);
    }
    if host.len() > 255 {
        return Err("destination hostname is longer than 255 bytes".into());
    }
    let mut v = vec![0x03, host.len() as u8];
    v.extend_from_slice(host.as_bytes());
    Ok(v)
}

fn socks_reply(code: u8) -> &'static str {
    match code {
        0x01 => "general failure",
        0x02 => "connection not allowed",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

// ---------------------------------------------------------------------- SOCKS4

async fn socks4(p: &ProxyEntry, host: &str, port: u16) -> Result<Stream, String> {
    let mut s = proxy_socket(p).await?;

    let ip: [u8; 4] = match host.parse::<std::net::Ipv4Addr>() {
        Ok(v4) => v4.octets(),
        Err(_) => [0, 0, 0, 1], // SOCKS4a: 0.0.0.1 tells the proxy to read the hostname
    };

    let mut req = vec![0x04, 0x01];
    req.extend_from_slice(&port.to_be_bytes());
    req.extend_from_slice(&ip);
    req.extend_from_slice(p.username.as_bytes());
    req.push(0);
    if ip == [0, 0, 0, 1] {
        req.extend_from_slice(host.as_bytes());
        req.push(0);
    }

    s.write_all(&req)
        .await
        .map_err(|e| format!("SOCKS4 write failed: {e}"))?;

    let mut reply = [0u8; 8];
    s.read_exact(&mut reply)
        .await
        .map_err(|e| format!("SOCKS4 read failed: {e}"))?;
    if reply[1] != 0x5A {
        return Err(format!(
            "SOCKS4 could not open the connection (status 0x{:02x})",
            reply[1]
        ));
    }
    Ok(Box::new(s))
}

// -------------------------------------------------------------------- utilities

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `Basic ...` value to hand an HTTP upstream that needs credentials.
pub fn basic_auth_value(p: &ProxyEntry) -> Option<String> {
    if p.username.is_empty() {
        return None;
    }
    Some(format!(
        "Basic {}",
        basic64(&format!("{}:{}", p.username, p.password))
    ))
}

fn basic64(input: &str) -> String {
    let b = input.as_bytes();
    let mut out = String::with_capacity((b.len() + 2) / 3 * 4);
    for chunk in b.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | (chunk.get(2).copied().unwrap_or(0) as u32);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Open a tunnel through `proxy`, ask Discord itself for a page, and stop the
/// clock at its first answer. Stopping at the proxy handshake (what this did
/// before) only measures how fast the proxy replies to CONNECT - not how fast
/// Discord comes back through it, which is the number anyone cares about.
pub async fn probe(proxy: Option<&ProxyEntry>) -> Result<u128, String> {
    let started = std::time::Instant::now();
    let mut s = dial(proxy, "discord.com", 80).await?;
    s.write_all(
        b"GET / HTTP/1.1\r\nHost: discord.com\r\nUser-Agent: discord-proxy\r\nConnection: close\r\n\r\n",
    )
    .await
    .map_err(|e| format!("cannot reach discord.com: {e}"))?;
    // `HTTP/1.1 301` - Discord redirects plain http to the https site, and any
    // 2xx/3xx from it proves the whole path through the proxy carried a request
    // and brought the answer back. A proxy that answers with its own 4xx page
    // would otherwise be reported as a perfectly fast round trip.
    let mut status = [0u8; 12];
    s.read_exact(&mut status)
        .await
        .map_err(|e| format!("discord.com did not answer: {e}"))?;
    let code: u16 = std::str::from_utf8(&status[9..12])
        .ok()
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| "something in between answered instead of discord.com".to_string())?;
    if !(200..400).contains(&code) {
        return Err(format!("discord.com answered with status {code}"));
    }
    Ok(started.elapsed().as_millis())
}
