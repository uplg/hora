//! A raw TCP stream to `host:port` through a monitor's proxy (HTTP `CONNECT`,
//! SOCKS5), for the certificate watcher: it reads the leaf certificate with
//! its own TLS dial (trust not verified), which reqwest cannot hand back for a
//! tunneled connection - its `TlsInfo` is only filled in on direct ones.

use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// The largest `CONNECT` answer head read before giving up.
const MAX_CONNECT_HEAD: usize = 8 * 1024;

/// Open a stream to `host:port` through `proxy` (`http://`, `socks5://` or
/// `socks5h://`, with optional `user:password@`), the whole exchange bounded
/// by `timeout`. Errors read "proxy failed: ...", like the probe's.
///
/// # Errors
///
/// An unsupported proxy scheme, a proxy that cannot be reached, or one that
/// refuses the tunnel (`407`, SOCKS credentials or a SOCKS error reply).
pub(crate) async fn open(
    proxy: &str,
    host: &str,
    port: u16,
    timeout: Duration,
) -> anyhow::Result<TcpStream> {
    tokio::time::timeout(timeout, open_unbounded(proxy, host, port))
        .await
        .map_err(|_elapsed| anyhow::anyhow!("proxy failed: tunnel timed out"))?
        .map_err(|err| anyhow::anyhow!("proxy failed: {err:#}"))
}

async fn open_unbounded(proxy: &str, host: &str, port: u16) -> anyhow::Result<TcpStream> {
    let url = reqwest::Url::parse(proxy).context("invalid proxy URL")?;
    let proxy_host = url
        .host_str()
        .context("proxy URL without a host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let credentials = (!url.username().is_empty()).then(|| {
        (
            percent_decode(url.username()),
            percent_decode(url.password().unwrap_or("")),
        )
    });
    match url.scheme() {
        "http" => {
            let proxy_port = url.port().unwrap_or(80);
            let mut stream = TcpStream::connect((proxy_host.as_str(), proxy_port))
                .await
                .context("could not reach the proxy")?;
            http_connect(&mut stream, host, port, credentials.as_ref()).await?;
            Ok(stream)
        }
        scheme @ ("socks5" | "socks5h") => {
            let proxy_port = url.port().unwrap_or(1080);
            let mut stream = TcpStream::connect((proxy_host.as_str(), proxy_port))
                .await
                .context("could not reach the SOCKS proxy")?;
            // socks5h: the proxy resolves the name; socks5: we do.
            let target = if scheme == "socks5h" {
                SocksTarget::Name(host)
            } else {
                let addr = tokio::net::lookup_host((host, port))
                    .await?
                    .next()
                    .context("could not resolve host")?;
                SocksTarget::Ip(addr.ip())
            };
            socks5_connect(&mut stream, &target, port, credentials.as_ref()).await?;
            Ok(stream)
        }
        other => bail!("reading a certificate through a {other} proxy is not supported"),
    }
}

/// `CONNECT host:port` and require a 2xx answer.
async fn http_connect(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    credentials: Option<&(String, String)>,
) -> anyhow::Result<()> {
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let authorization = credentials.map_or_else(String::new, |(user, password)| {
        let token = base64(format!("{user}:{password}").as_bytes());
        format!("Proxy-Authorization: Basic {token}\r\n")
    });
    let request =
        format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{authorization}\r\n");
    stream.write_all(request.as_bytes()).await?;

    // Byte by byte up to the blank line: nothing past the head may be
    // consumed (the TLS handshake starts right after it).
    let mut head = Vec::with_capacity(256);
    while !head.ends_with(b"\r\n\r\n") {
        ensure!(head.len() < MAX_CONNECT_HEAD, "proxy answer too long");
        head.push(stream.read_u8().await.context("proxy closed the tunnel")?);
    }
    let status_line = String::from_utf8_lossy(&head);
    let status_line = status_line.lines().next().unwrap_or_default();
    let code = status_line.split_whitespace().nth(1).unwrap_or_default();
    if code.starts_with('2') {
        return Ok(());
    }
    if code == "407" {
        bail!("tunnel error: proxy authorization required (HTTP 407)");
    }
    bail!(
        "tunnel error: proxy answered {}",
        crate::bounded(status_line, 80)
    )
}

/// Where a SOCKS5 proxy should connect.
enum SocksTarget<'a> {
    Name(&'a str),
    Ip(std::net::IpAddr),
}

/// The SOCKS5 handshake (RFC 1928), with username/password authentication
/// (RFC 1929) when the proxy URL carries credentials.
async fn socks5_connect(
    stream: &mut TcpStream,
    target: &SocksTarget<'_>,
    port: u16,
    credentials: Option<&(String, String)>,
) -> anyhow::Result<()> {
    const NO_AUTH: u8 = 0x00;
    const USER_PASS: u8 = 0x02;
    let greeting: &[u8] = if credentials.is_some() {
        &[5, 2, NO_AUTH, USER_PASS]
    } else {
        &[5, 1, NO_AUTH]
    };
    stream.write_all(greeting).await?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    ensure!(choice[0] == 5, "SOCKS error: not a SOCKS5 proxy");
    match (choice[1], credentials) {
        (NO_AUTH, _) => {}
        (USER_PASS, Some((user, password))) => {
            let (user, password) = (user.as_bytes(), password.as_bytes());
            let mut auth = vec![1, u8::try_from(user.len()).context("username too long")?];
            auth.extend_from_slice(user);
            auth.push(u8::try_from(password.len()).context("password too long")?);
            auth.extend_from_slice(password);
            stream.write_all(&auth).await?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await?;
            ensure!(status[1] == 0, "SOCKS error: credentials not accepted");
        }
        _ => bail!("SOCKS error: no acceptable authentication method"),
    }

    let mut request = vec![5, 1, 0];
    match target {
        SocksTarget::Name(name) => {
            request.push(3);
            request.push(u8::try_from(name.len()).context("host name too long")?);
            request.extend_from_slice(name.as_bytes());
        }
        SocksTarget::Ip(std::net::IpAddr::V4(v4)) => {
            request.push(1);
            request.extend_from_slice(&v4.octets());
        }
        SocksTarget::Ip(std::net::IpAddr::V6(v6)) => {
            request.push(4);
            request.extend_from_slice(&v6.octets());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;

    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await?;
    if reply[1] != 0 {
        bail!("SOCKS error: {}", socks_reply(reply[1]));
    }
    // Skip the bound address the proxy reports.
    let address_len = match reply[3] {
        1 => 4,
        4 => 16,
        3 => usize::from(stream.read_u8().await?),
        other => bail!("SOCKS error: unknown address type {other}"),
    };
    let mut bound = vec![0u8; address_len + 2];
    stream.read_exact(&mut bound).await?;
    Ok(())
}

/// RFC 1928's reply codes, worded.
fn socks_reply(code: u8) -> &'static str {
    match code {
        1 => "general SOCKS server failure",
        2 => "connection not allowed by ruleset",
        3 => "network unreachable",
        4 => "host unreachable",
        5 => "connection refused",
        6 => "TTL expired",
        7 => "command not supported",
        8 => "address type not supported",
        _ => "unknown error",
    }
}

/// Standard, padded base64 - for one `Proxy-Authorization` header, not worth
/// a dependency.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = chunk.iter().enumerate().fold(0u32, |acc, (i, byte)| {
            acc | u32::from(*byte) << (16 - 8 * i)
        });
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(
                    ALPHABET[(triple >> (18 - 6 * i) & 0x3f) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode the `%XX` escapes of a URL's user info (lossy on invalid UTF-8).
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        if let Some(byte) = decoded {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The text helpers for the fuzz targets in `fuzz/` (built with
/// `--cfg fuzzing` only).
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzz {
    /// [`super::base64`].
    #[must_use]
    pub fn base64(bytes: &[u8]) -> String {
        super::base64(bytes)
    }

    /// [`super::percent_decode`].
    #[must_use]
    pub fn percent_decode(raw: &str) -> String {
        super::percent_decode(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("puser:ppass", "cHVzZXI6cHBhc3M="),
        ] {
            assert_eq!(base64(raw.as_bytes()), encoded, "{raw}");
        }
    }

    #[test]
    fn user_info_is_percent_decoded() {
        assert_eq!(percent_decode("p%40ss%3Aword"), "p@ss:word");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[tokio::test]
    async fn tunnels_through_http_connect_and_socks5() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket.write_all(b"hello").await;
            }
        });
        let read_hello = |mut stream: TcpStream| async move {
            let mut hello = [0u8; 5];
            stream.read_exact(&mut hello).await.unwrap();
            hello
        };

        let (http_proxy, tunnels) = crate::testing::connect_proxy().await;
        let stream = open(&http_proxy, "127.0.0.1", port, Duration::from_secs(5))
            .await
            .expect("CONNECT");
        assert_eq!(&read_hello(stream).await, b"hello");
        assert_eq!(tunnels.load(std::sync::atomic::Ordering::SeqCst), 1);

        let socks = crate::testing::socks5_proxy(Some(("puser", "p@ss"))).await;
        let proxy = format!(
            "socks5h://puser:p%40ss@{}",
            socks.trim_start_matches("socks5://")
        );
        let stream = open(&proxy, "localhost", port, Duration::from_secs(5))
            .await
            .expect("SOCKS5 with credentials");
        assert_eq!(&read_hello(stream).await, b"hello");

        let wrong = format!(
            "socks5://puser:nope@{}",
            socks.trim_start_matches("socks5://")
        );
        let err = open(&wrong, "127.0.0.1", port, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "proxy failed: SOCKS error: credentials not accepted"
        );
    }

    #[tokio::test]
    async fn a_proxy_refusing_the_tunnel_is_named() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                    .await;
            }
        });
        let err = open(
            &format!("http://{addr}"),
            "example.com",
            443,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "proxy failed: tunnel error: proxy authorization required (HTTP 407)"
        );
        let err = open(
            "https://proxy.example:3128",
            "example.com",
            443,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }
}
