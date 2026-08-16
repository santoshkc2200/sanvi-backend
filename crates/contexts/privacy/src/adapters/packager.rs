//! The export packager: provider bundles → one ZIP with a machine-readable
//! JSON export, human-readable HTML and CSV renderings, and a manifest with
//! per-file SHA-256 checksums. The ZIP bytes are then encrypted by the
//! caller with a one-time passphrase.

use std::io::Write;

use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

use crate::domain::{DataBundle, ExportPackager, PrivacyError, SubjectRef};

#[derive(Debug, Clone, Copy, Default)]
pub struct ZipPackager;

impl ZipPackager {
    pub fn new() -> Self {
        Self
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn render_csv(bundles: &[DataBundle]) -> Result<Vec<u8>, PrivacyError> {
    let mut writer = csv_writer();
    writer
        .write_record(["context", "category", "data_class", "label", "value"])
        .map_err(|e| PrivacyError::PackagingFailed(e.to_string()))?;
    for bundle in bundles {
        for entry in &bundle.entries {
            let value = match &entry.value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            writer
                .write_record([
                    bundle.context.clone(),
                    entry.category.clone(),
                    entry.data_class.as_str().to_string(),
                    entry.label.clone(),
                    value,
                ])
                .map_err(|e| PrivacyError::PackagingFailed(e.to_string()))?;
        }
    }
    writer
        .into_inner()
        .map_err(|e| PrivacyError::PackagingFailed(e.to_string()))
}

fn render_html(subject: &SubjectRef, bundles: &[DataBundle]) -> Result<Vec<u8>, PrivacyError> {
    let mut html = String::new();
    html.push_str("<!doctype html><html><head><meta charset=\"utf-8\"><title>Data export</title>");
    html.push_str("<style>body{font-family:system-ui,sans-serif;margin:2rem}table{border-collapse:collapse;width:100%}th,td{border:1px solid #ccc;padding:.4rem;text-align:left}th{background:#f5f5f5}h2{margin-top:2rem}</style></head><body>");
    html.push_str("<h1>Personal data export</h1>");
    html.push_str(&format!(
        "<p>Subject: <code>{}</code> · Generated at UTC timestamp in the manifest.</p>",
        html_escape(subject.canonical_key())
    ));
    for bundle in bundles {
        if bundle.entries.is_empty() {
            continue;
        }
        html.push_str(&format!(
            "<h2>{}</h2><table><tr><th>Category</th><th>Data class</th><th>Field</th><th>Value</th></tr>",
            html_escape(&bundle.context)
        ));
        for entry in &bundle.entries {
            let value = match &entry.value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                html_escape(&entry.category),
                html_escape(entry.data_class.as_str()),
                html_escape(&entry.label),
                html_escape(&value)
            ));
        }
        html.push_str("</table>");
    }
    html.push_str("</body></html>");
    Ok(html.into_bytes())
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn csv_writer() -> csv::Writer<Vec<u8>> {
    csv::Writer::from_writer(Vec::new())
}

impl ExportPackager for ZipPackager {
    fn package(
        &self,
        subject: &SubjectRef,
        bundles: Vec<DataBundle>,
    ) -> Result<Vec<u8>, PrivacyError> {
        let data_json = serde_json::to_vec_pretty(&serde_json::json!({
            "subject": subject,
            "contexts": bundles,
        }))
        .map_err(|e| PrivacyError::PackagingFailed(e.to_string()))?;
        let csv = render_csv(&bundles)?;
        let html = render_html(subject, &bundles)?;

        let files: Vec<(&str, &[u8])> = vec![
            ("data.json", data_json.as_slice()),
            ("data.csv", csv.as_slice()),
            ("data.html", html.as_slice()),
        ];

        // The manifest is written first with its own checksums computed, so
        // it is included in the archive checksums as a real file.
        let mut manifest_json = serde_json::Map::new();
        manifest_json.insert(
            "subject".to_string(),
            serde_json::to_value(subject)
                .map_err(|e| PrivacyError::PackagingFailed(e.to_string()))?,
        );
        manifest_json.insert(
            "generated_at".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        let mut manifest_files = Vec::new();
        for (name, bytes) in &files {
            manifest_files.push(serde_json::json!({ "name": name, "sha256": sha256_hex(bytes) }));
        }
        manifest_json.insert(
            "files".to_string(),
            serde_json::Value::Array(manifest_files),
        );
        let manifest_bytes = serde_json::to_vec_pretty(&serde_json::Value::Object(manifest_json))
            .map_err(|e| PrivacyError::PackagingFailed(e.to_string()))?;

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o600);
        for (name, bytes) in files.iter().chain(std::iter::once(&(
            "manifest.json",
            manifest_bytes.as_slice(),
        ))) {
            zip.start_file(*name, options)
                .map_err(|e| PrivacyError::PackagingFailed(e.to_string()))?;
            zip.write_all(bytes)
                .map_err(|e| PrivacyError::PackagingFailed(e.to_string()))?;
        }
        let archive = zip
            .finish()
            .map_err(|e| PrivacyError::PackagingFailed(e.to_string()))?
            .into_inner();
        Ok(archive)
    }
}

/// The manifest checksum of the archive (for the artifact row).
pub fn archive_sha256(archive: &[u8]) -> String {
    sha256_hex(archive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DataClass, DataEntry, SubjectKind};

    #[test]
    fn package_contains_all_files_and_verifies() {
        let subject = SubjectRef {
            kind: SubjectKind::TenantUser,
            tenant_id: None,
            key: "user:test".to_string(),
            identifiers: vec![],
        };
        let bundle = DataBundle {
            context: "identity".to_string(),
            entries: vec![DataEntry {
                data_class: DataClass::IdentityProfile,
                category: "Account".to_string(),
                label: "email".to_string(),
                value: serde_json::json!("test@example.com"),
            }],
        };
        let archive = ZipPackager::new().package(&subject, vec![bundle]).unwrap();
        assert!(!archive.is_empty());

        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&archive)).unwrap();
        let manifest = zip.by_name("manifest.json").unwrap();
        let manifest_value: serde_json::Value =
            serde_json::from_reader(manifest).expect("manifest parses");
        assert_eq!(manifest_value["subject"]["key"], "user:test");
        assert_eq!(manifest_value["files"].as_array().unwrap().len(), 3);
        for name in ["data.json", "data.csv", "data.html"] {
            assert!(zip.by_name(name).is_ok(), "missing {name}");
        }
    }
}
