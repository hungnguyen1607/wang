//! Egress proxy: the jail has no network except loopback; a forwarder inside it carries
//! TCP from 127.0.0.1:3128 to this proxy over a bind-mounted Unix socket. The proxy speaks
//! HTTP CONNECT (HTTPS) and absolute-form HTTP, resolves DNS itself, and only connects to
//! hosts on the allow-list. Everything refused is recorded.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::task::JoinHandle;

use crate::policy::{domain_allowed, is_private_ip, private_allowed};

pub struct EgressProxy {
    pub socket: PathBuf,
    blocked: Arc<Mutex<Vec<String>>>,
    allowed: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl EgressProxy {
    pub fn start(socket: &Path, domains: Vec<String>) -> Result<Self> {
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket)?;
        let blocked = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(Mutex::new(Vec::new()));
        let domains = Arc::new(domains);
        let (b, a) = (blocked.clone(), allowed.clone());
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (d, b, a) = (domains.clone(), b.clone(), a.clone());
                tokio::spawn(async move {
                    let _ = handle(stream, &d, &b, &a).await;
                });
            }
        });
        Ok(Self { socket: socket.to_path_buf(), blocked, allowed, task })
    }

    /// Hosts refused so far.
    pub fn blocked(&self) -> Vec<String> {
        let mut v = self.blocked.lock().unwrap().clone();
        v.sort();
        v.dedup();
        v
    }

    pub fn stop(self) -> (Vec<String>, Vec<String>) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
        let dedup = |v: &Mutex<Vec<String>>| {
            let mut v = v.lock().unwrap().clone();
            v.sort();
            v.dedup();
            v
        };
        (dedup(&self.blocked), dedup(&self.allowed))
    }
}

/// Split "host:port" / "[v6]:port" with a default port.
pub fn split_host_port(s: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default_port,
        };
        return Some((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((s.to_string(), default_port)),
    }
}

enum Target {
    Connect { host: String, port: u16 },
    Http { host: String, port: u16, rewritten_head: Vec<u8> },
}

fn parse_head(head: &[u8]) -> Result<Target> {
    let text = std::str::from_utf8(head)?;
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(target, 443).ok_or_else(|| anyhow::anyhow!("bad CONNECT target"))?;
        return Ok(Target::Connect { host, port });
    }
    let Some(rest) = target.strip_prefix("http://") else { bail!("only CONNECT and absolute http:// requests are proxied") };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let authority = authority.rsplit_once('@').map(|(_, a)| a).unwrap_or(authority);
    let (host, port) = split_host_port(authority, 80).ok_or_else(|| anyhow::anyhow!("bad host"))?;
    let mut out = format!("{method} {path} {version}\r\n");
    for l in lines {
        let lower = l.to_ascii_lowercase();
        if lower.starts_with("proxy-connection:") || lower.starts_with("proxy-authorization:") {
            continue;
        }
        out.push_str(l);
        out.push_str("\r\n");
    }
    // `out` already ends with the blank line from the original head.
    Ok(Target::Http { host, port, rewritten_head: out.trim_end_matches("\r\n").to_string().into_bytes() })
}

async fn handle(mut client: UnixStream, domains: &[String], blocked: &Mutex<Vec<String>>, allowed: &Mutex<Vec<String>>) -> Result<()> {
    // Read the request head (bounded).
    let mut buf = Vec::with_capacity(2048);
    let head_end = loop {
        let mut chunk = [0u8; 2048];
        let n = tokio::time::timeout(Duration::from_secs(10), client.read(&mut chunk)).await??;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 16 * 1024 {
            client.write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\n\r\n").await?;
            return Ok(());
        }
    };
    let target = match parse_head(&buf[..head_end]) {
        Ok(t) => t,
        Err(e) => {
            let body = format!("wang-jail proxy: {e}\n");
            client
                .write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .await?;
            return Ok(());
        }
    };
    let (host, port) = match &target {
        Target::Connect { host, port } | Target::Http { host, port, .. } => (host.clone(), *port),
    };
    if !domain_allowed(domains, &host, port) {
        blocked.lock().unwrap().push(format!("{host}:{port}"));
        let body = format!("wang-jail: {host}:{port} is not in this tool's network allow-list\n");
        client
            .write_all(
                format!(
                    "HTTP/1.1 403 Forbidden\r\nX-Wang-Blocked: {host}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await?;
        return Ok(());
    }
    // Resolve here (the jail has no DNS) and refuse the user's own machine / network unless
    // that host is listed exactly: a wildcard must never reach localhost or the tailnet.
    let addrs: Vec<std::net::SocketAddr> =
        match tokio::time::timeout(Duration::from_secs(10), tokio::net::lookup_host((host.as_str(), port))).await {
            Ok(Ok(a)) => a.collect(),
            _ => vec![],
        };
    if addrs.is_empty() {
        client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    if addrs.iter().any(|a| is_private_ip(a.ip())) && !private_allowed(domains, &host) {
        blocked.lock().unwrap().push(format!("{host}:{port} (private address)"));
        let body = format!("wang-jail: {host} resolves to a private address; list it exactly to allow it\n");
        client
            .write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
            .await?;
        return Ok(());
    }
    let mut upstream = match tokio::time::timeout(Duration::from_secs(15), TcpStream::connect(&addrs[..])).await {
        Ok(Ok(s)) => s,
        _ => {
            client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            return Ok(());
        }
    };
    allowed.lock().unwrap().push(format!("{host}:{port}"));
    match target {
        Target::Connect { .. } => {
            client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
            upstream.write_all(&buf[head_end..]).await?;
        }
        Target::Http { rewritten_head, .. } => {
            upstream.write_all(&rewritten_head).await?;
            upstream.write_all(b"\r\n\r\n").await?;
            upstream.write_all(&buf[head_end..]).await?;
        }
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_ports() {
        assert_eq!(split_host_port("a.com:8443", 443), Some(("a.com".into(), 8443)));
        assert_eq!(split_host_port("a.com", 443), Some(("a.com".into(), 443)));
        assert_eq!(split_host_port("[::1]:80", 443), Some(("::1".into(), 80)));
        assert_eq!(split_host_port("a.com:x", 443), None);
    }

    #[test]
    fn heads() {
        match parse_head(b"CONNECT api.github.com:443 HTTP/1.1\r\nHost: api.github.com\r\n\r\n").unwrap() {
            Target::Connect { host, port } => assert_eq!((host.as_str(), port), ("api.github.com", 443)),
            _ => panic!(),
        }
        match parse_head(b"GET http://example.com/a?b=1 HTTP/1.1\r\nHost: example.com\r\nProxy-Connection: keep-alive\r\n\r\n").unwrap() {
            Target::Http { host, port, rewritten_head } => {
                assert_eq!((host.as_str(), port), ("example.com", 80));
                let h = String::from_utf8(rewritten_head).unwrap();
                assert!(h.starts_with("GET /a?b=1 HTTP/1.1\r\nHost: example.com"), "{h:?}");
                assert!(!h.to_lowercase().contains("proxy-connection"));
            }
            _ => panic!(),
        }
        assert!(parse_head(b"GET /relative HTTP/1.1\r\n\r\n").is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocks_unlisted_and_records() {
        let dir = std::env::temp_dir().join(format!("wang-proxy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("p.sock");
        let p = EgressProxy::start(&sock, vec!["allowed.example".into()]).unwrap();
        let mut c = UnixStream::connect(&sock).await.unwrap();
        c.write_all(b"CONNECT evil.example:443 HTTP/1.1\r\nHost: evil.example\r\n\r\n").await.unwrap();
        let mut resp = String::new();
        c.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        let (blocked, allowed) = p.stop();
        assert_eq!(blocked, vec!["evil.example:443".to_string()]);
        assert!(allowed.is_empty());

        // wildcard allow-list still can't reach the host itself
        let p = EgressProxy::start(&sock, vec!["*".into()]).unwrap();
        let mut c = UnixStream::connect(&sock).await.unwrap();
        c.write_all(b"CONNECT localhost:443 HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
        let mut resp = String::new();
        c.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        assert_eq!(p.stop().0, vec!["localhost:443 (private address)".to_string()]);
        std::fs::remove_dir_all(dir).ok();
    }
}
