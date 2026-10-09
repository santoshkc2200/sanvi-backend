use std::process::Command;

#[test]
fn rehearsal_proves_new_access_and_old_rejection_without_rendering_credentials() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let report = std::env::temp_dir().join(format!("sanvi-rotation-{}.json", uuid::Uuid::now_v7()));
    let output = Command::new(env!("CARGO_BIN_EXE_sanvi-cli"))
        .current_dir(&root)
        .args(["rotate-credentials", "--rehearse", "--report"])
        .arg(&report)
        .output()
        .expect("CLI launched");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(&report).expect("measured report");
    std::fs::remove_file(&report).unwrap();
    let evidence: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        evidence["scope"],
        "local rehearsal; provider revocations parked"
    );
    for name in [
        "postgres",
        "redis",
        "tenant_signing",
        "tracking_signing",
        "oauth_state_signing",
        "preview_signing",
        "billing_webhook",
        "connect_webhook",
        "privacy_kek",
        "advertising_kek",
    ] {
        let row = evidence["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["credential"] == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(row["new_accepted"], true, "{name}");
        assert_eq!(row["old_rejected"], true, "{name}");
        assert!(row["duration_ms"].as_u64().is_some(), "{name}");
        assert!(root.join(row["runbook"].as_str().unwrap()).is_file());
    }
    assert!(!evidence["parked"].as_array().unwrap().is_empty());
    for text in [&bytes[..], &output.stdout[..], &output.stderr[..]] {
        let text = String::from_utf8_lossy(text);
        assert!(!text.contains("rotation-old-"));
        assert!(!text.contains("rotation-new-"));
    }
}

#[test]
fn rotation_requires_explicit_rehearsal_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_sanvi-cli"))
        .arg("rotate-credentials")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--rehearse"));
}
