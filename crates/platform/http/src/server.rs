use std::{fmt, io, time::Duration};

use actix_http::Request;
use actix_service::IntoServiceFactory;
use actix_web::{
    body::MessageBody,
    dev::{AppConfig, Response},
};

use crate::config::ServerConfig;

/// Build a *bound but not yet running* actix server. [`bind`] is the normal
/// entry point; this variant exposes `addrs()` so tests can listen on an
/// ephemeral port before starting the server.
pub fn bind_without_run<F, I, S, B>(
    config: &ServerConfig,
    factory: F,
) -> io::Result<actix_web::HttpServer<F, I, S, B>>
where
    F: Fn() -> I + Send + Clone + 'static,
    I: IntoServiceFactory<S, Request>,
    S: actix_web::dev::ServiceFactory<Request, Config = AppConfig> + 'static,
    S::Error: Into<actix_web::Error> + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
{
    config
        .validate()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;

    let workers = if config.workers == 0 {
        num_cpus()
    } else {
        config.workers
    };

    actix_web::HttpServer::new(factory)
        .workers(workers)
        .disable_signals()
        .shutdown_timeout(config.shutdown_timeout_secs)
        .client_request_timeout(Duration::from_secs(config.request_timeout_secs + 5))
        .bind((config.host.as_str(), config.port))
}

/// Build a listening actix server from configuration.
///
/// The caller provides the `App` factory (the composition root). Signal
/// handling is disabled so the binary controls shutdown explicitly via
/// [`shutdown_signal`]; worker count 0 means "one per CPU core".
///
/// The returned [`actix_web::dev::Server`] implements `Future` and exposes
/// `handle()` for graceful stop.
pub fn bind<F, I, S, B>(config: &ServerConfig, factory: F) -> io::Result<actix_web::dev::Server>
where
    F: Fn() -> I + Send + Clone + 'static,
    I: IntoServiceFactory<S, Request>,
    S: actix_web::dev::ServiceFactory<Request, Config = AppConfig> + 'static,
    S::Error: Into<actix_web::Error> + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
{
    Ok(bind_without_run(config, factory)?.run())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(2)
}

/// Wait for SIGTERM or SIGINT (the canonical graceful-shutdown trigger).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}

/// Shorthand used by binaries: run the server and stop gracefully on signal.
pub async fn run(server: actix_web::dev::Server) {
    let handle = server.handle();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("stopping http server");
        handle.stop(true).await;
    });

    if let Err(err) = server.await {
        tracing::error!(error = %err, "http server error");
    }
}

/// Marker import kept for doc references.
#[allow(unused_imports)]
use actix_web::dev::ServiceRequest as _ServiceRequestMarker;

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::App;

    #[test]
    fn worker_count_defaults_to_available_parallelism() {
        assert!(num_cpus() >= 1);
    }

    #[test]
    fn bind_rejects_invalid_config() {
        let config = ServerConfig {
            host: String::new(),
            ..ServerConfig::default()
        };
        let result = bind(&config, App::new);
        assert!(result.is_err());
    }
}
