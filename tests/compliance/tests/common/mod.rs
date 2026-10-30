//! A local stand-in for the standalone Domains service's privacy endpoints.
//!
//! Domains personal data lives in its own service, so the platform's Domains
//! provider always calls out. The compliance suite runs without that service,
//! so it points the harness here instead: a minimal HTTP/1.1 server that
//! verifies the signed `privacy` delegation exactly as the service does and
//! answers as the service would for a subject with no Domains orders. The
//! operational CLI drill never uses this; it keeps the real configuration.
// Each test binary compiles this module and uses a different subset of it.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use sanvi_api::compliance::ComplianceHarness;
use sanvi_domains::adapters::remote::{
    DELEGATION_HEADER, Delegation, RequestBinding, ServiceConfig, TENANT_AUDIENCE,
};
use sanvi_kernel::RawIdGenerator;
use sanvi_privacy::domain::{DataBundle, EraseReport};
use sanvi_telemetry::Redacted;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &str = "compliance-domains-stand-in-secret-0123456789";
const ADVERTISING_SECRET: &str = "compliance-advertising-stand-in-secret-0123456789";
const PAYMENTS_SECRET: &str = "compliance-payments-stand-in-secret-0123456789";

/// Privacy paths the stand-in answered with a verified delegation, in order.
pub type DomainsCalls = Arc<Mutex<Vec<String>>>;
pub type AdvertisingCalls = Arc<Mutex<Vec<String>>>;
pub type PaymentsCalls = Arc<Mutex<Vec<String>>>;

/// Build the harness over fresh Domains and Payments stand-ins.
pub async fn harness(
    pool: sanvi_persistence::PgPool,
    ids: Arc<dyn RawIdGenerator>,
) -> (ComplianceHarness, DomainsCalls, PaymentsCalls) {
    let (domains_config, domains_calls) = spawn_domains_service().await;
    let (payments_config, payments_calls) = spawn_payments_service().await;
    let (advertising_config, _) = spawn_advertising_service().await;
    (
        ComplianceHarness::build_with_services(
            pool,
            ids,
            domains_config,
            payments_config,
            advertising_config,
        ),
        domains_calls,
        payments_calls,
    )
}

async fn spawn_payments_service() -> (
    sanvi_payments::adapters::remote::ServiceConfig,
    PaymentsCalls,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = PaymentsCalls::default();
    let recorded = calls.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let Some((method, target, headers, body)) = read_request(&mut socket).await else {
                continue;
            };
            let (status, body) = answer_payments(&method, &target, &headers, &body, &recorded);
            let reason = match status {
                200 => "OK",
                204 => "No Content",
                401 => "Unauthorized",
                _ => "Not Found",
            };
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    let config = sanvi_payments::adapters::remote::ServiceConfig {
        base_url: format!("http://{addr}"),
        signing_secret: Redacted::new(PAYMENTS_SECRET.to_string()),
        inbound_secret: Redacted::new("compliance-payments-inbound-secret-0123456789".to_string()),
    };
    (config, calls)
}

fn answer_payments(
    method: &str,
    target: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    calls: &PaymentsCalls,
) -> (u16, Vec<u8>) {
    use sanvi_payments::adapters::remote::{
        DELEGATION_HEADER as P_DELEGATION_HEADER, Delegation as PDelegation,
        RequestBinding as PRequestBinding, TENANT_AUDIENCE as P_TENANT_AUDIENCE,
    };
    let header = |name: &str| headers.get(name).map(String::as_str).unwrap_or("");
    let binding = PRequestBinding {
        method,
        target,
        body,
        content_type: header("content-type"),
        if_match: header("if-match"),
    };
    if PDelegation::verify(
        PAYMENTS_SECRET,
        header(P_DELEGATION_HEADER),
        P_TENANT_AUDIENCE,
        "privacy",
        &binding,
    )
    .is_err()
    {
        return (401, Vec::new());
    }
    let reply = match target {
        "/internal/privacy/collect" => (
            200,
            serde_json::to_vec(&DataBundle {
                context: "payments".into(),
                entries: Vec::new(),
            })
            .unwrap(),
        ),
        "/internal/privacy/erase" => (
            200,
            serde_json::to_vec(&EraseReport {
                context: "payments".into(),
                ..EraseReport::default()
            })
            .unwrap(),
        ),
        "/internal/privacy/rectify" => (204, Vec::new()),
        _ => return (404, Vec::new()),
    };
    calls.lock().unwrap().push(target.to_string());
    reply
}

async fn spawn_advertising_service() -> (
    sanvi_advertising::adapters::remote::ServiceConfig,
    AdvertisingCalls,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = AdvertisingCalls::default();
    let recorded = calls.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let Some((method, target, headers, body)) = read_request(&mut socket).await else {
                continue;
            };
            let (status, body) = answer_advertising(&method, &target, &headers, &body, &recorded);
            let reason = match status {
                200 => "OK",
                204 => "No Content",
                401 => "Unauthorized",
                _ => "Not Found",
            };
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    let config = sanvi_advertising::adapters::remote::ServiceConfig {
        base_url: format!("http://{addr}"),
        signing_secret: Redacted::new(ADVERTISING_SECRET.to_string()),
        inbound_secret: Redacted::new(
            "compliance-advertising-inbound-secret-0123456789".to_string(),
        ),
    };
    (config, calls)
}

fn answer_advertising(
    method: &str,
    target: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    calls: &AdvertisingCalls,
) -> (u16, Vec<u8>) {
    use sanvi_advertising::adapters::remote::{
        DELEGATION_HEADER as P_DELEGATION_HEADER, Delegation as PDelegation,
        RequestBinding as PRequestBinding, TENANT_AUDIENCE as P_TENANT_AUDIENCE,
    };
    let header = |name: &str| headers.get(name).map(String::as_str).unwrap_or("");
    let binding = PRequestBinding {
        method,
        target,
        body,
        content_type: header("content-type"),
        if_match: header("if-match"),
    };
    if PDelegation::verify(
        ADVERTISING_SECRET,
        header(P_DELEGATION_HEADER),
        P_TENANT_AUDIENCE,
        "privacy",
        &binding,
    )
    .is_err()
    {
        return (401, Vec::new());
    }
    let reply = match target {
        "/internal/privacy/collect" => (
            200,
            serde_json::to_vec(&DataBundle {
                context: "advertising".into(),
                entries: Vec::new(),
            })
            .unwrap(),
        ),
        "/internal/privacy/erase" => (
            200,
            serde_json::to_vec(&EraseReport {
                context: "advertising".into(),
                ..EraseReport::default()
            })
            .unwrap(),
        ),
        "/internal/privacy/rectify" => (204, Vec::new()),
        _ => return (404, Vec::new()),
    };
    calls.lock().unwrap().push(target.to_string());
    reply
}

async fn spawn_domains_service() -> (ServiceConfig, DomainsCalls) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = DomainsCalls::default();
    let recorded = calls.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let Some((method, target, headers, body)) = read_request(&mut socket).await else {
                continue;
            };
            let (status, body) = answer(&method, &target, &headers, &body, &recorded);
            let reason = match status {
                200 => "OK",
                204 => "No Content",
                401 => "Unauthorized",
                _ => "Not Found",
            };
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    let config = ServiceConfig {
        base_url: format!("http://{addr}"),
        signing_secret: Redacted::new(SECRET.to_string()),
        inbound_secret: Redacted::new("compliance-domains-inbound-secret-0123456789".to_string()),
    };
    (config, calls)
}

fn answer(
    method: &str,
    target: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    calls: &DomainsCalls,
) -> (u16, Vec<u8>) {
    let header = |name: &str| headers.get(name).map(String::as_str).unwrap_or("");
    let binding = RequestBinding {
        method,
        target,
        body,
        content_type: header("content-type"),
        if_match: header("if-match"),
    };
    if Delegation::verify(
        SECRET,
        header(DELEGATION_HEADER),
        TENANT_AUDIENCE,
        "privacy",
        &binding,
    )
    .is_err()
    {
        return (401, Vec::new());
    }
    let reply = match target {
        "/internal/privacy/collect" => (
            200,
            serde_json::to_vec(&DataBundle {
                context: "domains".into(),
                entries: Vec::new(),
            })
            .unwrap(),
        ),
        "/internal/privacy/erase" => (
            200,
            serde_json::to_vec(&EraseReport {
                context: "domains".into(),
                ..EraseReport::default()
            })
            .unwrap(),
        ),
        "/internal/privacy/rectify" => (204, Vec::new()),
        _ => return (404, Vec::new()),
    };
    calls.lock().unwrap().push(target.to_string());
    reply
}

type Request = (String, String, BTreeMap<String, String>, Vec<u8>);

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = socket.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf.split_off(head_end + 4);
    while body.len() < length {
        let n = socket.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Some((method, target, headers, body))
}
