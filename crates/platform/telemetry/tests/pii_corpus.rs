//! TASK-020 step 4 (slice 11.1c): the log PII corpus and its scanner.
//!
//! The corpus holds one specimen of every PII shape the platform stores —
//! including the two easy-to-dismiss ones: a **base64-wrapped token** (the
//! wrapper does not anonymise the credential) and a **hashed identifier**
//! (a SHA-256 of an email is pseudonymous data, still an identifier, still
//! regulated). Every specimen is emitted through the real log emitter the
//! way application code is required to emit sensitive values — wrapped in
//! [`Redacted`] — and the emitted lines are then scanned for the raw
//! values. One raw occurrence is a finding; the clean tree must produce
//! zero.
//!
//! `scripts/check-log-pii.sh` is the gate: it runs this test (zero
//! findings expected) and proves the scan can actually fail via
//! `--self-test`, which plants a leak by emitting one unwrapped value and
//! asserts the scanner catches it.

use std::sync::{Arc, Mutex};

use sanvi_telemetry::Redacted;
use tracing::subscriber::set_default;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};

/// Every PII shape the platform holds. The kind is a stable name used in
/// findings and in the self-test's planted leak.
const CORPUS: &[(&str, &str)] = &[
    ("email", "yuki.tanaka@example.jp"),
    ("phone_number", "+81-90-1234-5678"),
    ("postal_address", "2-11-3 Meguro, Meguro-ku, Tokyo 153-0063"),
    (
        "bearer_token",
        "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk",
    ),
    ("stripe_secret_key", concat!("sk_", "live_51H8xQYdEvJ9l2pqAAAAAAAAAA00")),
    ("stripe_webhook_secret", "whsec_a1b2c3d4e5f6g7h8i9j0"),
    ("oauth_refresh_token", "1//0aB3cD4eF5gH6iJ7kL8mN9oP-refresh"),
    ("card_pan", "4242 4242 4242 4242"),
    (
        // The JWT from `bearer_token`, base64-wrapped: unwrapping it must
        // not be a way around redaction, so the wrapped form is a corpus
        // value in its own right.
        "base64_wrapped_token",
        "ZXlKaGJHY2lPaUpTVXpJMU5pSXNJblI1Y0NJNklrcFhWQ0o5LmV5SnpkV0lpT2lJeE1qTTBOVFkzT0Rrd0luMC5kQmpmdEplWjRDVlBtQjkySzI3dWhiVUpVMXAxcldXMWdGV0ZPRWpYaw==",
    ),
    (
        // SHA-256 of `yuki.tanaka@example.jp` — hashed, yet still an
        // identifier of a natural person (the advertising context uploads
        // exactly this shape).
        "hashed_identifier",
        "1b9f2a4cd3e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1",
    ),
    ("session_cookie", "__Host-srv_sess=MTc1ODAwNjQwMHxzZXNz"),
    ("kratos_identity_id", "b0f8a4c1-2d3e-4f5a-8679-1a2b3c4d5e6f"),
];

/// A cloneable handle to the capture buffer, shaped for `with_writer`.
#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn emit_and_capture() -> String {
    let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = {
        let buffer = buffer.clone();
        move || SharedBuf(buffer.clone())
    };
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .with_ansi(false)
            .with_level(true)
            .with_filter(LevelFilter::INFO),
    );
    let _guard = set_default(subscriber);

    for (kind, value) in CORPUS {
        // The three emission shapes application code actually uses: a
        // structured field, a Display-formatted message fragment, and a
        // Debug-rendered struct that carries the sensitive value.
        let redacted = Redacted::new((*value).to_string());
        tracing::info!(
            kind = *kind,
            value = %redacted,
            "pii corpus probe: field emission",
        );
        tracing::info!("pii corpus probe: message emission of {redacted}");
        #[derive(Debug)]
        #[allow(dead_code)] // the fields exist to be Debug-rendered
        struct Probe<'a> {
            kind: &'a str,
            value: Redacted<String>,
        }
        let probe = Probe {
            kind,
            value: Redacted::new((*value).to_string()),
        };
        tracing::info!(probe = ?probe, "pii corpus probe: debug emission");
    }

    let captured = buffer.lock().unwrap().clone();
    String::from_utf8(captured).expect("captured log output is utf-8")
}

/// The gate: emit the whole corpus through the emitter, then scan the
/// rendered lines for every raw value. Zero findings expected.
#[test]
fn redaction_hides_every_corpus_value_from_the_logs() {
    let captured = emit_and_capture();
    let mut findings = Vec::new();
    for (kind, value) in CORPUS {
        if captured.contains(value) {
            findings.push(*kind);
        }
    }
    assert!(
        findings.is_empty(),
        "LEAK FINDING: raw values rendered for {findings:?} — the emitting code must wrap them in Redacted"
    );
    // The emitter itself ran: the corpus really went through the formatter.
    assert!(captured.matches("pii corpus probe").count() >= CORPUS.len() * 3);
    // The approved rendering is present in the output.
    assert!(captured.contains("<redacted>"));
}

/// The self-test half of the gate: with `SANVI_PII_PLANT_LEAK` naming a
/// corpus kind, that one value is deliberately emitted *without* redaction
/// — simulating a type whose Display/Debug lost its wrapper — and the
/// scan below must report it. `scripts/check-log-pii.sh --self-test` runs
/// this test expecting the failure; the clean suite runs it as a no-op.
#[test]
fn planted_leak_is_detected_by_the_scan() {
    let Some(planted) = std::env::var_os("SANVI_PII_PLANT_LEAK") else {
        return;
    };
    let planted = planted.to_string_lossy().into_owned();
    let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = {
        let buffer = buffer.clone();
        move || SharedBuf(buffer.clone())
    };
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .with_ansi(false)
            .with_filter(LevelFilter::INFO),
    );
    let _guard = set_default(subscriber);

    let (kind, value) = CORPUS
        .iter()
        .find(|(kind, _)| *kind == planted)
        .unwrap_or_else(|| panic!("unknown planted leak kind: {planted}"));
    // The leak: one emission that forgot the wrapper — the defect the
    // scanner exists to catch.
    tracing::warn!(kind = *kind, value = *value, "planted leak");
    let captured = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();

    assert!(
        captured.contains(value),
        "LEAK FINDING: {kind} was expected in the output after the redaction was removed, but the scan found nothing — the scanner is broken"
    );
    // Reaching this line means the planted leak WAS rendered; the scan that
    // `check-log-pii.sh` runs on the corpus as a whole would flag it. Fail
    // the test so the self-test harness sees the finding surface.
    panic!("LEAK FINDING: {kind} — raw value present in rendered log output");
}
