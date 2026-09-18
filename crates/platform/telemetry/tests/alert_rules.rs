//! TASK-020 step 7 (slice 11.1e): alert rules as code, unit-tested against
//! recorded series.
//!
//! `observability/alerts/rules.json` is the registry: every alert the
//! platform declares, its PromQL expression for the monitoring stack, a
//! machine-checkable condition (`check`) the evaluator below understands,
//! and the runbook that answers it (`just runbook-xref` enforces the
//! pairing in both directions).
//!
//! The tests prove three things per rule:
//!
//! 1. the condition fires on a synthetic series shaped to breach it and
//!    stay breached past its `for_secs`;
//! 2. the same condition stays silent on a clean series (zeros, or busy
//!    traffic with no failures);
//! 3. the rule's expression names only metrics the platform actually
//!    exports (or declares as probe-only).
//!
//! The committed fixtures then prove the DoD demo end to end: a synthetic
//! ten-minute outage fires `ApiErrorRatioHigh`; a single failed request
//! among ten thousand does not.

use serde_json::Value;
use std::collections::BTreeMap;

const RULES: &str = include_str!("../../../../observability/alerts/rules.json");

fn rules() -> Vec<Value> {
    serde_json::from_str::<Value>(RULES)
        .expect("rules.json parses")
        .get("rules")
        .and_then(|rules| rules.as_array())
        .cloned()
        .expect("rules.json holds a rules array")
}

fn check<'a>(rule: &'a Value) -> &'a Value {
    rule.get("check").expect("every rule carries a check")
}

fn field<'a>(rule: &'a Value, name: &str) -> &'a Value {
    rule.get(name)
        .unwrap_or_else(|| panic!("rule {} missing {name}", rule["alert"]))
}

// ---------------------------------------------------------------------------
// The series evaluator. Fixture series record per-step increments (for
// counters) or the sampled value (for gauges), aligned to `step_secs`.
// ---------------------------------------------------------------------------

/// Label pairs required by a selector spec (`{"metric": …, "labels": {…}}`).
fn labels_of(spec: &Value) -> Vec<(String, String)> {
    spec["labels"]
        .as_object()
        .map(|labels| {
            labels
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

struct Series {
    step_secs: u64,
    values: BTreeMap<String, Vec<(Vec<(String, String)>, Vec<f64>)>>,
}

impl Series {
    fn from_fixture(fixture: &Value) -> Self {
        let step = fixture["step_secs"].as_u64().expect("step_secs");
        let mut values = BTreeMap::new();
        for series in fixture["series"].as_array().expect("series array") {
            let metric = series["metric"].as_str().expect("metric").to_string();
            let labels: Vec<(String, String)> = series["labels"]
                .as_object()
                .map(|labels| {
                    labels
                        .iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let values_list: Vec<f64> = series["values"]
                .as_array()
                .expect("values")
                .iter()
                .map(|v| v.as_f64().expect("numeric"))
                .collect();
            values.entry(metric).or_insert_with(Vec::new).push((labels, values_list));
        }
        Self { step_secs: step, values }
    }

    fn steps(&self) -> usize {
        self.values
            .values()
            .flat_map(|series| series.iter())
            .map(|(_, values)| values.len())
            .max()
            .unwrap_or(0)
    }

    /// Sum of every series matching `metric` + required labels, at step `t`.
    fn sample(&self, metric: &str, labels: &[(String, String)], t: usize) -> f64 {
        self.values
            .get(metric)
            .map(|series| {
                series
                    .iter()
                    .filter(|(series_labels, _)| {
                        labels.iter().all(|(k, v)| {
                            series_labels.iter().any(|(lk, lv)| lk == k && lv == v)
                        })
                    })
                    .map(|(_, values)| values.get(t).copied().unwrap_or(0.0))
                    .sum()
            })
            .unwrap_or(0.0)
    }

    fn window_sum(&self, metric: &str, labels: &[(String, String)], t: usize, window_secs: u64) -> f64 {
        let window_steps = (window_secs / self.step_secs).max(1) as usize;
        (t.saturating_sub(window_steps - 1)..=t)
            .map(|step| self.sample(metric, labels, step))
            .sum()
    }
}

/// Does the rule fire anywhere on the horizon? Fires at step `t` when the
/// condition holds, sustained for `for_secs`.
fn fires(series: &Series, check: &Value) -> bool {
    let kind = check["kind"].as_str().expect("check kind");
    let threshold = check["threshold"].as_f64().unwrap_or(0.0);
    let for_secs = check["for_secs"].as_u64().unwrap_or(0);
    let for_steps = (for_secs / series.step_secs).max(1) as usize;
    let metric_labels = |spec: &Value| -> Vec<(String, String)> {
        spec["labels"]
            .as_object()
            .map(|labels| {
                labels
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };

    match kind {
        "ratio" => {
            let numerator = &check["numerator"];
            let denominators = check["denominator"].as_array().expect("denominator array");
            for t in 0..series.steps() {
                let num = series.window_sum(
                    numerator["metric"].as_str().unwrap(),
                    &metric_labels(numerator),
                    t,
                    check["window_secs"].as_u64().unwrap(),
                );
                let den: f64 = denominators
                    .iter()
                    .map(|spec| {
                        series.window_sum(
                            spec["metric"].as_str().unwrap(),
                            &metric_labels(spec),
                            t,
                            check["window_secs"].as_u64().unwrap(),
                        )
                    })
                    .sum();
                let holds = den > 0.0 && num / den > threshold;
                if holds && (1..=for_steps).all(|back: usize| {
                    let u = t.saturating_sub(back);
                    let n = series.window_sum(
                        numerator["metric"].as_str().unwrap(),
                        &metric_labels(numerator),
                        u,
                        check["window_secs"].as_u64().unwrap(),
                    );
                    let d: f64 = denominators
                        .iter()
                        .map(|spec| {
                            series.window_sum(
                                spec["metric"].as_str().unwrap(),
                                &metric_labels(spec),
                                u,
                                check["window_secs"].as_u64().unwrap(),
                            )
                        })
                        .sum();
                    d > 0.0 && n / d >= threshold
                }) {
                    return true;
                }
            }
            false
        }
        "increase" => {
            let metric = check["metric"].as_str().unwrap();
            let labels = metric_labels(check);
            for t in 0..series.steps() {
                let sum = series.window_sum(
                    metric,
                    &labels,
                    t,
                    check["window_secs"].as_u64().unwrap(),
                );
                if sum > threshold {
                    return true;
                }
            }
            false
        }
        "gauge" | "p95_gauge" => {
            let metric = check["metric"].as_str().unwrap();
            let labels = metric_labels(check);
            let mut consecutive = 0;
            for t in 0..series.steps() {
                if series.sample(metric, &labels, t) > threshold {
                    consecutive += 1;
                    if consecutive >= for_steps {
                        return true;
                    }
                } else {
                    consecutive = 0;
                }
            }
            false
        }
        // Probe rules are database/audit predicates, not series-evaluable.
        "probe" => false,
        other => panic!("unknown check kind: {other}"),
    }
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

/// Every registered rule is well-formed and its runbook pairing is wired
/// (the file-existence half belongs to `just runbook-xref`).
#[test]
fn every_rule_is_well_formed() {
    for rule in rules() {
        assert!(field(&rule, "alert").is_string());
        assert!(field(&rule, "severity").is_string());
        assert!(field(&rule, "symptom").is_string());
        let runbook = field(&rule, "runbook").as_str().unwrap();
        assert!(runbook.starts_with("docs/runbooks/"), "{runbook}");
        let check = field(&rule, "check");
        let kind = check["kind"].as_str().unwrap();
        assert!(
            matches!(kind, "ratio" | "increase" | "gauge" | "p95_gauge" | "probe"),
            "{kind}"
        );
        if kind == "probe" {
            assert!(check["probe"].as_str().is_some(), "probe rules document their predicate");
        } else {
            assert!(check["threshold"].is_number());
            assert!(check["for_secs"].is_number());
        }
    }
}

/// Symptom rules only: no CPU, no queue depth that is not user-facing, no
/// cause-only alert. The registry is small enough to read; this test pins
/// the rule that nothing may alert on machine facts alone.
#[test]
fn rules_are_symptom_based_not_cause_based() {
    const FORBIDDEN: &[&str] = &["cpu", "memory", "disk", "fd_count", "load_average"];
    for rule in rules() {
        let text = serde_json::to_string(&rule).unwrap().to_lowercase();
        for word in FORBIDDEN {
            assert!(
                !text.contains(word),
                "{} alerts on a cause ({}), not a symptom",
                rule["alert"],
                word
            );
        }
    }
}

/// Generic evaluation: every series-evaluable rule fires on a synthetic
/// breach sustained past its `for_secs`, and stays silent on a clean
/// series.
#[test]
fn every_evaluable_rule_fires_on_a_breach_and_stays_silent_when_clean() {
    for rule in rules() {
        let check = field(&rule, "check");
        let kind = check["kind"].as_str().unwrap();
        if kind == "probe" {
            continue;
        }
        let step = 60_u64;
        let horizon = 120; // two hours of one-minute steps
        let threshold = check["threshold"].as_f64().unwrap_or(0.0);
        let series = Series {
            step_secs: step,
            values: BTreeMap::new(),
        };
        let _ = series; // constructed per case below

        let breach = match kind {
            "ratio" => {
                let mut errors: Vec<f64> = vec![0.0; horizon];
                for slot in errors.iter_mut().skip(30) {
                    *slot = (threshold * 200.0).ceil().max(1.0);
                }
                let numerator = &check["numerator"];
                let numerator_labels = labels_of(numerator);
                let numerator_metric = numerator["metric"].as_str().unwrap().to_string();
                let mut values = BTreeMap::new();
                values.insert(
                    numerator_metric.clone(),
                    vec![(numerator_labels, errors)],
                );
                for spec in check["denominator"].as_array().unwrap() {
                    let metric = spec["metric"].as_str().unwrap().to_string();
                    // A denominator that IS the numerator metric (failure /
                    // (success + failure)) is already in the map — the same
                    // series counts in both, exactly as in production.
                    if metric == numerator_metric {
                        continue;
                    }
                    // The success counter keeps a small share so the ratio
                    // actually breaches.
                    values.insert(metric, vec![(vec![], vec![10.0; horizon])]);
                }
                values
            }
            "increase" => {
                let mut values = BTreeMap::new();
                let mut increments = vec![0.0; horizon];
                increments[40] = (threshold + 1.0).max(1.0);
                values.insert(
                    check["metric"].as_str().unwrap().to_string(),
                    vec![(labels_of(check), increments)],
                );
                values
            }
            "gauge" | "p95_gauge" => {
                let mut values = BTreeMap::new();
                let mut samples = vec![0.0; horizon];
                for slot in samples.iter_mut().skip(30) {
                    *slot = threshold * 2.0 + 1.0;
                }
                values.insert(
                    check["metric"].as_str().unwrap().to_string(),
                    vec![(labels_of(check), samples)],
                );
                values
            }
            other => unreachable!("{other}"),
        };
        let breach_series = Series {
            step_secs: step,
            values: breach,
        };
        assert!(
            fires(&breach_series, check),
            "{} never fires on a synthetic breach",
            rule["alert"]
        );

        // Clean: busy but healthy.
        let clean = match kind {
            "ratio" => {
                let mut values = BTreeMap::new();
                let numerator = &check["numerator"];
                values.insert(
                    numerator["metric"].as_str().unwrap().to_string(),
                    vec![(labels_of(numerator), vec![0.0; horizon])],
                );
                let numerator_metric = numerator["metric"].as_str().unwrap().to_string();
                for spec in check["denominator"].as_array().unwrap() {
                    let metric = spec["metric"].as_str().unwrap().to_string();
                    if metric == numerator_metric {
                        continue; // numerator's zeros already cover it
                    }
                    values.insert(metric, vec![(vec![], vec![100.0; horizon])]);
                }
                values
            }
            "increase" => {
                let mut values = BTreeMap::new();
                values.insert(
                    check["metric"].as_str().unwrap().to_string(),
                    vec![(labels_of(check), vec![0.0; horizon])],
                );
                values
            }
            "gauge" | "p95_gauge" => {
                let mut values = BTreeMap::new();
                values.insert(
                    check["metric"].as_str().unwrap().to_string(),
                    vec![(labels_of(check), vec![0.0; horizon])],
                );
                values
            }
            other => unreachable!("{other}"),
        };
        let clean_series = Series {
            step_secs: step,
            values: clean,
        };
        assert!(
            !fires(&clean_series, check),
            "{} fires on a clean series",
            rule["alert"]
        );
        let _ = threshold;
    }
}

/// The DoD demo, against the committed fixtures: a synthetic ten-minute
/// outage fires the error-ratio alert; a single failed request among ten
/// thousand does not.
#[test]
fn a_ten_minute_outage_fires_and_a_single_failed_request_does_not() {
    let outage = Series::from_fixture(
        &serde_json::from_str::<Value>(include_str!(
            "../../../../observability/alerts/fixtures/ten_minute_outage.json"
        ))
        .unwrap(),
    );
    let single = Series::from_fixture(
        &serde_json::from_str::<Value>(include_str!(
            "../../../../observability/alerts/fixtures/single_failed_request.json"
        ))
        .unwrap(),
    );
    let rule = rules()
        .into_iter()
        .find(|rule| rule["alert"] == "ApiErrorRatioHigh")
        .expect("ApiErrorRatioHigh registered");
    assert!(fires(&outage, check(&rule)), "the outage must fire");
    assert!(
        !fires(&single, check(&rule)),
        "a single failed request must not fire"
    );
}

/// Expressions only name metrics the platform exports, or probe-declared
/// ones. `probe`-kind rules with `expr: null` are skipped; an expr that
/// names a metric nobody exports is a typo that would silently never fire.
#[test]
fn exprs_reference_declared_metrics_only() {
    let exported: &[&str] = &[
        // platform/http/src/metrics.rs (OTel → Prometheus naming)
        "http_server_requests_total",
        "http_server_errors_total",
        "http_server_duration_seconds",
        "http_server_duration_seconds_bucket",
        "tenant_resolution_duration",
        "tenant_resolution_source_total",
        // contexts/payments/src/adapters/metrics.rs
        "payments_platform_balance_movements_total",
        "payments_connection_status_gauge",
        "payments_webhook_events_total",
        "payments_connection_disconnects_total",
        "payments_webhook_unprocessed_gauge",
        "payments_webhook_dead_letter_gauge",
        "payments_webhook_lag_seconds",
        "payments_webhook_lag_seconds_bucket",
        "payments_webhook_unknown_account_total",
        "payments_payout_failures_total",
        "payments_payment_failure_total",
        "payments_payment_succeeded_total",
        "payments_dispute_open_gauge",
        // contexts/advertising/src/adapters/metrics.rs
        "conversion_upload_total",
        "conversion_suppressed_total",
        "conversion_retraction_total",
        "audience_sync_total",
        "conversion_dlq_depth",
        "conversion_upload_lag_seconds",
        "conversion_upload_lag_seconds_bucket",
        "metrics_ingestion_lag_hours",
        "ads_sync_failures_total",
        "ads_sync_duration_seconds",
        "ads_sync_duration_seconds_bucket",
        "budget_threshold_alerts_total",
        "budget_auto_pause_total",
    ];
    let expr_declared_only: &[&str] = &[];
    for rule in rules() {
        let Some(raw_expr) = rule["expr"].as_str() else {
            continue;
        };
        // Strip label selectors: `{status="onboarding"}` carries label
        // names, not metrics.
        let mut owned = raw_expr.to_string();
        while let Some(open) = owned.find('{') {
            let Some(close) = owned[open..].find('}') else {
                break;
            };
            owned.replace_range(open..=open + close, "");
        }
        let expr: &str = &owned;
        // Metric-shaped identifiers in the expression.
        let mut known = Vec::new();
        known.extend(exported.iter().copied());
        known.extend(expr_declared_only.iter().copied());
        const PROMQL_WORDS: &[&str] = &[
            "sum", "rate", "increase", "max_over_time", "min_over_time",
            "histogram_quantile", "by", "without", "le", "and", "or", "unless",
            "ignoring", "on", "group_left", "group_right", "offset", "bool",
        ];
        for candidate in expr.split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')) {
            if candidate.is_empty() || candidate.starts_with(|c: char| c.is_ascii_digit()) || PROMQL_WORDS.contains(&candidate) {
                continue;
            }
            let matches_known = known.iter().any(|m| candidate.starts_with(m));
            assert!(
                matches_known,
                "{}: expr names `{}` which no known metric starts with",
                rule["alert"],
                candidate
            );
        }
    }
}
