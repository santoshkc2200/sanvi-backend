//! `healthcheck`: dependency-free liveness probe for Docker `HEALTHCHECK`.
//!
//! The distroless runtime image ships no shell and no `curl`; this issues a
//! raw HTTP/1.1 GET over a plain `tokio::net::TcpStream` and checks for a
//! `200` status line, so the container needs nothing beyond the binary
//! itself.

use std::time::Duration;

use anyhow::{Context, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

pub async fn run(host: &str, port: u16, path: &str, timeout_secs: u64) -> anyhow::Result<()> {
    let budget = Duration::from_secs(timeout_secs);
    timeout(budget, check(host, port, path))
        .await
        .context("healthcheck timed out")?
}

async fn check(host: &str, port: u16, path: &str) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect((host, port))
        .await
        .context("failed to connect")?;

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .context("failed to send request")?;

    let mut response = Vec::with_capacity(256);
    stream
        .read_to_end(&mut response)
        .await
        .context("failed to read response")?;

    let status_line = response
        .split(|&b| b == b'\n')
        .next()
        .map(|line| String::from_utf8_lossy(line).trim().to_string())
        .unwrap_or_default();

    if status_line.starts_with("HTTP/1.1 200") || status_line.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        bail!("unhealthy response: {status_line}")
    }
}
