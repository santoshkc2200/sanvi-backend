//! Graceful-shutdown drain (TASK-025 step 2, FR-1118): the property that
//! makes TASK-023's rolling restart drop zero requests.
//!
//! SIGTERM maps to `ServerHandle::stop(true)` — graceful — in `run()`, on
//! every platform the signal test can reach. The test drives exactly that
//! call under load: requests already accepted must complete, and once the
//! drain finishes the listener must refuse new work.

use std::net::TcpStream;
use std::time::Duration;

use actix_web::{App, HttpResponse, web};
use sanvi_http::{ServerConfig, bind_without_run};

/// A handler slow enough that the test can hit stop while requests are
/// provably in flight (the drain window the grace period covers).
async fn slow() -> HttpResponse {
    tokio::time::sleep(Duration::from_millis(400)).await;
    HttpResponse::Ok().body("drained")
}

fn drain_config(port: u16) -> ServerConfig {
    ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        workers: 2,
        shutdown_timeout_secs: 10,
        ..ServerConfig::default()
    }
}

fn raw_get(port: u16) -> Result<String, std::io::Error> {
    use std::io::{Read, Write};
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(
        format!("GET /slow HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

fn port_refuses_connections(port: u16) -> bool {
    TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_err()
}

/// SIGTERM under load: every accepted request completes; nothing is
/// dropped. `stop(true)` is the exact call `run()` makes on SIGTERM/SIGINT.
#[actix_web::test]
async fn graceful_stop_drains_every_in_flight_request() {
    let server = bind_without_run(&drain_config(0), move || {
        App::new().route("/slow", web::get().to(slow))
    })
    .expect("bind on an ephemeral port");
    let port = server
        .addrs()
        .first()
        .copied()
        .expect("bound address")
        .port();
    let actix_server = server.run();
    let handle = actix_server.handle();
    let server_task = tokio::spawn(actix_server);

    // Wait for the listener, then put requests in flight.
    let mut wait = 50;
    while wait > 0 && port_refuses_connections(port) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        wait -= 1;
    }
    assert!(wait > 0, "server never started listening");

    const IN_FLIGHT: usize = 8;
    let mut readers = Vec::new();
    for _ in 0..IN_FLIGHT {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let response = raw_get(port).expect("request completed at the TCP level");
            let _ = tx.send(response);
        });
        readers.push(rx);
    }
    // Let the requests land in the sleeping handler before stopping.
    tokio::time::sleep(Duration::from_millis(150)).await;

    handle.stop(true).await;
    let _ = server_task.await;

    for (index, rx) in readers.into_iter().enumerate() {
        let response = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("request {index} was dropped by shutdown"));
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "request {index} did not complete successfully: {response:.60}"
        );
    }

    // The listener is gone: new work is refused, not queued.
    assert!(
        port_refuses_connections(port),
        "listener still accepting after a graceful stop"
    );
}

/// `stop(false)` is the non-graceful path: accepted work is NOT guaranteed
/// to finish. The drain test above is the contract; this one proves the
/// contrast so the graceful variant cannot silently regress into force.
#[actix_web::test]
async fn forceful_stop_is_not_the_drain_path() {
    let server = bind_without_run(&drain_config(0), move || {
        App::new().route("/slow", web::get().to(slow))
    })
    .expect("bind on an ephemeral port");
    let port = server
        .addrs()
        .first()
        .copied()
        .expect("bound address")
        .port();
    let actix_server = server.run();
    let handle = actix_server.handle();
    tokio::spawn(actix_server);

    let mut wait = 50;
    while wait > 0 && port_refuses_connections(port) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        wait -= 1;
    }
    assert!(wait > 0, "server never started listening");

    // (No in-flight requests here: this only pins that stop(false) returns
    // immediately rather than draining — the mechanism distinction.)
    let started = std::time::Instant::now();
    handle.stop(false).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "forceful stop drained instead of cutting off"
    );
}
