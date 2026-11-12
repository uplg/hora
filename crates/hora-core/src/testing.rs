//! Test fixtures shared across modules: a webhook sink that records what the
//! notifiers send, a TLS server with a fixed certificate, and an HTTP CONNECT
//! proxy that counts the tunnels it opened.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// The `notAfter` of `testdata/localhost.cert.der` (2126-09-09 16:33:45 UTC),
/// a self-signed certificate for `localhost` and `127.0.0.1`.
pub(crate) const FIXTURE_NOT_AFTER: i64 = 4_944_645_225;

/// The JSON bodies a webhook channel posted, in order.
pub(crate) type Posts = Arc<Mutex<Vec<serde_json::Value>>>;

/// A webhook receiver on 127.0.0.1: returns its URL and the recorded posts.
pub(crate) async fn webhook_sink() -> (String, Posts) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts: Posts = Arc::default();
    let recorded = Arc::clone(&posts);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut socket = BufReader::new(socket);
                // Keep-alive: several requests may share the connection.
                while let Some(body) = read_request(&mut socket).await {
                    if let Ok(json) = serde_json::from_slice(&body) {
                        recorded.lock().unwrap().push(json);
                    }
                    let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                    if socket.get_mut().write_all(reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (format!("http://{addr}/hook"), posts)
}

/// A webhook receiver that refuses every post with a 400: a permanent failure,
/// so the channel reports it without retrying. Returns its URL.
pub(crate) async fn webhook_refusing() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut socket = BufReader::new(socket);
                while read_request(&mut socket).await.is_some() {
                    let reply = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                    if socket.get_mut().write_all(reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    format!("http://{addr}/hook")
}

/// One HTTP/1.1 request's body (`Content-Length` framed); `None` at EOF.
async fn read_request(socket: &mut BufReader<TcpStream>) -> Option<Vec<u8>> {
    let mut length = 0;
    loop {
        let mut line = String::new();
        if socket.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.ok()?;
    Some(body)
}

/// The events posted so far, by their `event` field.
pub(crate) fn events(posts: &Posts) -> Vec<String> {
    posts
        .lock()
        .unwrap()
        .iter()
        .map(|post| post["event"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A TLS server presenting the fixture certificate and answering every
/// request with an empty 200. Returns its port.
pub(crate) async fn tls_server() -> u16 {
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::crypto::aws_lc_rs::default_provider;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    let cert = CertificateDer::from(include_bytes!("../testdata/localhost.cert.der").to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("../testdata/localhost.key.der").to_vec(),
    ));
    let config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(socket).await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = tls.read(&mut buf).await;
                let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = tls.write_all(reply).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

/// A SOCKS5 proxy (CONNECT only), requiring `credentials` when given.
/// Returns its `socks5://host:port` URL.
pub(crate) async fn socks5_proxy(credentials: Option<(&'static str, &'static str)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = socks5_session(&mut client, credentials).await;
            });
        }
    });
    format!("socks5://{addr}")
}

async fn socks5_session(
    client: &mut TcpStream,
    credentials: Option<(&str, &str)>,
) -> std::io::Result<()> {
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    let mut methods = vec![0u8; usize::from(head[1])];
    client.read_exact(&mut methods).await?;
    if let Some((user, password)) = credentials {
        client.write_all(&[5, 2]).await?;
        let mut version = [0u8; 2];
        client.read_exact(&mut version).await?;
        let mut name = vec![0u8; usize::from(version[1])];
        client.read_exact(&mut name).await?;
        let mut pass = vec![0u8; usize::from(client.read_u8().await?)];
        client.read_exact(&mut pass).await?;
        let ok = name == user.as_bytes() && pass == password.as_bytes();
        client.write_all(&[1, u8::from(!ok)]).await?;
        if !ok {
            return Ok(());
        }
    } else {
        client.write_all(&[5, 0]).await?;
    }
    let mut request = [0u8; 4];
    client.read_exact(&mut request).await?;
    let host = match request[3] {
        1 => {
            let mut v4 = [0u8; 4];
            client.read_exact(&mut v4).await?;
            std::net::Ipv4Addr::from(v4).to_string()
        }
        3 => {
            let mut name = vec![0u8; usize::from(client.read_u8().await?)];
            client.read_exact(&mut name).await?;
            String::from_utf8_lossy(&name).into_owned()
        }
        _ => return Ok(()),
    };
    let port = client.read_u16().await?;
    let mut upstream = TcpStream::connect((host.as_str(), port)).await?;
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    tokio::io::copy_bidirectional(client, &mut upstream).await?;
    Ok(())
}

/// An HTTP proxy that only tunnels (`CONNECT`), counting the tunnels.
/// Returns its URL and the counter.
pub(crate) async fn connect_proxy() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tunnels = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&tunnels);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let mut client = BufReader::new(socket);
                let mut request = String::new();
                if client.read_line(&mut request).await.is_err() {
                    return;
                }
                // Skip the remaining request headers.
                loop {
                    let mut line = String::new();
                    match client.read_line(&mut line).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) if line.trim_end().is_empty() => break,
                        Ok(_) => {}
                    }
                }
                let Some(target) = request
                    .strip_prefix("CONNECT ")
                    .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
                else {
                    return;
                };
                let Ok(mut upstream) = TcpStream::connect(target).await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut client = client.into_inner();
                if client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .is_ok()
                {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
            });
        }
    });
    (format!("http://{addr}"), tunnels)
}
