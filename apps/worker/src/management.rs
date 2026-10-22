//! Private management listener and fail-fast supervision for worker tasks.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::{JoinHandle, JoinSet},
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ManagementConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ManagementConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 9091,
        }
    }
}

#[derive(Default)]
pub struct WorkerHealth {
    healthy: AtomicBool,
    ready: AtomicBool,
}

impl WorkerHealth {
    pub fn starting() -> Arc<Self> {
        Arc::new(Self {
            healthy: AtomicBool::new(true),
            ready: AtomicBool::new(false),
        })
    }
    fn stop(&self) {
        self.ready.store(false, Ordering::SeqCst);
        self.healthy.store(false, Ordering::SeqCst);
    }
}

pub async fn run(
    listener: TcpListener,
    metrics: Arc<sanvi_telemetry::MetricsRegistry>,
    health: Arc<WorkerHealth>,
    pool: sqlx::PgPool,
    mut redis: sanvi_messaging::RedisConfig,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    // Independent management connection: a blocked event command cannot
    // prevent readiness from detecting a dependency outage.
    redis.response_timeout_ms = redis.response_timeout_ms.min(1000);
    let mut redis = sanvi_messaging::connect_redis(&redis).await?;
    let mut probe = tokio::time::interval(Duration::from_secs(1));
    let mut connections = JoinSet::new();
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break; } }
            _ = probe.tick() => {
                let database = tokio::time::timeout(Duration::from_secs(1), sqlx::query("SELECT 1").execute(&pool));
                let ping = redis::cmd("PING");
                let cache = tokio::time::timeout(Duration::from_secs(1), ping.query_async::<String>(&mut redis));
                let (database, cache) = tokio::join!(database, cache);
                health.ready.store(matches!(database, Ok(Ok(_))) && matches!(cache, Ok(Ok(_))), Ordering::SeqCst);
            }
            accepted = listener.accept() => {
                let (socket, _) = accepted.context("worker management accept failed")?;
                if connections.len() < 128 {
                    let metrics = metrics.clone(); let health = health.clone();
                    connections.spawn(async move {
                        let _ = tokio::time::timeout(Duration::from_secs(3), serve(socket, metrics, health)).await;
                    });
                }
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    health.stop();
    connections.abort_all();
    Ok(())
}

async fn serve(
    mut socket: TcpStream,
    metrics: Arc<sanvi_telemetry::MetricsRegistry>,
    health: Arc<WorkerHealth>,
) -> Result<()> {
    let mut header = [0_u8; 4096];
    let mut used = 0;
    loop {
        if used == header.len() {
            return Ok(());
        }
        let read = socket.read(&mut header[used..]).await?;
        if read == 0 {
            return Ok(());
        }
        used += read;
        if header[..used]
            .windows(4)
            .any(|window| window == b"\r\n\r\n")
        {
            break;
        }
    }
    let line = std::str::from_utf8(&header[..used])?
        .lines()
        .next()
        .unwrap_or_default();
    let mut request = line.split_whitespace();
    let method = request.next().unwrap_or_default();
    let path = request.next().unwrap_or_default();
    let (status, content_type, body) = if method == "GET" {
        response(path, &metrics, &health)?
    } else {
        (405, "text/plain", "method not allowed\n".into())
    };
    let wire = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Unavailable" },
        body.len()
    );
    socket.write_all(wire.as_bytes()).await?;
    socket.shutdown().await?;
    Ok(())
}

fn response(
    path: &str,
    metrics: &sanvi_telemetry::MetricsRegistry,
    health: &WorkerHealth,
) -> Result<(u16, &'static str, String)> {
    let healthy = health.healthy.load(Ordering::SeqCst);
    match path {
        "/metrics" => Ok((200, "text/plain; version=0.0.4", metrics.render()?)),
        "/healthz" if healthy => Ok((200, "text/plain", "ok\n".into())),
        "/readyz" if healthy && health.ready.load(Ordering::SeqCst) => {
            Ok((200, "text/plain", "ready\n".into()))
        }
        "/healthz" | "/readyz" => Ok((503, "text/plain", "unavailable\n".into())),
        _ => Ok((404, "text/plain", "not found\n".into())),
    }
}

pub async fn supervise(
    tasks: Vec<(&'static str, JoinHandle<()>)>,
    shutdown: watch::Sender<bool>,
    health: Arc<WorkerHealth>,
) -> Result<()> {
    let mut pending = JoinSet::new();
    for (name, handle) in tasks {
        pending.spawn(async move { (name, handle.await) });
    }
    let mut failure = None;
    while let Some(result) = pending.join_next().await {
        let unexpected = !*shutdown.borrow();
        if unexpected || !result.as_ref().is_ok_and(|(_, outcome)| outcome.is_ok()) {
            failure = Some(format!("worker task stopped: {result:?}"));
            health.stop();
            let _ = shutdown.send(true);
            break;
        }
    }
    if *shutdown.borrow() {
        health.stop();
        if tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(result) = pending.join_next().await {
                if !result.as_ref().is_ok_and(|(_, outcome)| outcome.is_ok()) {
                    failure = Some(format!("worker task failed while draining: {result:?}"));
                }
            }
        })
        .await
        .is_err()
        {
            failure.get_or_insert_with(|| "worker drain timed out".into());
            pending.abort_all();
        }
    }
    match failure {
        Some(detail) => anyhow::bail!(detail),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_worker_metrics_are_exposed_and_readiness_tracks_health() {
        let metrics = sanvi_telemetry::MetricsRegistry::prometheus_only();
        let outbox = sanvi_messaging::OutboxMetrics::new();
        outbox.register(metrics.prometheus_registry()).unwrap();
        outbox.pending.set(7);
        let health = WorkerHealth::starting();
        assert!(
            response("/metrics", &metrics, &health)
                .unwrap()
                .2
                .contains("outbox_pending_gauge 7")
        );
        assert_eq!(response("/readyz", &metrics, &health).unwrap().0, 503);
        health.ready.store(true, Ordering::SeqCst);
        assert_eq!(response("/readyz", &metrics, &health).unwrap().0, 200);
        health.stop();
        assert_eq!(response("/healthz", &metrics, &health).unwrap().0, 503);
    }

    #[tokio::test]
    async fn registered_backlog_is_scrapable_over_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let metrics = sanvi_telemetry::MetricsRegistry::prometheus_only();
        let outbox = sanvi_messaging::OutboxMetrics::new();
        outbox.register(metrics.prometheus_registry()).unwrap();
        outbox.pending.set(17);
        let health = WorkerHealth::starting();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            serve(socket, metrics, health).await.unwrap();
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: worker\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut wire = String::new();
        client.read_to_string(&mut wire).await.unwrap();
        server.await.unwrap();
        assert!(wire.starts_with("HTTP/1.1 200"));
        assert!(wire.contains("outbox_pending_gauge 17"));
    }

    #[tokio::test]
    async fn a_panicked_consumer_signals_shutdown_and_fails_the_worker() {
        let (tx, rx) = watch::channel(false);
        let health = WorkerHealth::starting();
        let panic = tokio::spawn(async { panic!("consumer failed") });
        let mut drain = rx.clone();
        let other = tokio::spawn(async move {
            let _ = drain.changed().await;
        });
        assert!(
            supervise(
                vec![("consumer", panic), ("other", other)],
                tx,
                health.clone()
            )
            .await
            .is_err()
        );
        assert!(*rx.borrow());
        assert!(!health.healthy.load(Ordering::SeqCst));
    }
}
