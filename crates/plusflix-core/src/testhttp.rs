//! A tiny fake HTTP server for tests: serves canned JSON bodies per request
//! path, so addon-protocol requests can be exercised without a network.
#![allow(dead_code)]

use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Routes map the raw (percent-encoded) request path (e.g.
/// `/catalog/movie/top/genre=Action&skip=0.json`) to a JSON body. A missing
/// route answers 404, which the addon client treats as a failed request.
pub struct FakeServer {
    /// Base URL (`http://127.0.0.1:{port}`) to use as an addon transport URL.
    pub url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    /// Starts the server on an OS-assigned loopback port. Keeps serving until
    /// dropped (the task is aborted).
    pub async fn start(routes: HashMap<String, String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let routes = routes.clone();
                tokio::spawn(serve(stream, routes));
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            handle,
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(mut stream: TcpStream, routes: HashMap<String, String>) {
    // Minimal HTTP/1.1: read the request head (GETs have no body) and answer.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let (status, body) = match routes.get(&path) {
        Some(json) => ("200 OK", json.clone()),
        None => ("404 Not Found", r#"{"metas":[]}"#.to_string()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}
