use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{BudgetType, CreativeText, Objective, TargetingDimension, ValidationViolation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleGranularity {
    Daily,
    Hourly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreativePlacement {
    pub key: String,
    pub asset_spec: CreativeAssetSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreativeAssetSpec {
    /// Placement-set-level requirement (whether at least one image asset is required for the placement set).
    /// Not enforced per asset.
    pub image_required: bool,
    pub video_allowed: bool,
    pub aspect_ratios: Vec<String>,
    #[serde(default)]
    pub min_width_px: Option<u32>,
    #[serde(default)]
    pub min_height_px: Option<u32>,
    #[serde(default)]
    pub max_file_size_bytes: Option<u64>,
    #[serde(default)]
    pub min_duration_seconds: Option<u32>,
    #[serde(default)]
    pub max_duration_seconds: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AssetMetadata {
    pub width_px: u32,
    pub height_px: u32,
    pub file_size_bytes: u64,
    pub is_video: bool,
    pub duration_seconds: Option<u32>,
    pub aspect_ratio: String,
}

fn parse_aspect_ratio(s: &str) -> Option<f64> {
    let (w_str, h_str) = s.split_once(':')?;
    let w: f64 = w_str.trim().parse().ok()?;
    let h: f64 = h_str.trim().parse().ok()?;
    if w <= 0.0 || h <= 0.0 || !w.is_finite() || !h.is_finite() {
        return None;
    }
    Some(w / h)
}

impl CreativeAssetSpec {
    pub fn validate_asset(&self, asset: &AssetMetadata) -> Vec<ValidationViolation> {
        let mut violations = Vec::new();

        if !self.video_allowed && asset.is_video {
            violations.push(ValidationViolation::unsupported(
                "is_video",
                "video assets are not allowed for this placement",
            ));
        }

        match parse_aspect_ratio(&asset.aspect_ratio) {
            None => {
                violations.push(ValidationViolation::invalid(
                    "aspect_ratio",
                    format!(
                        "aspect ratio '{}' is not a valid ratio format (expected W:H)",
                        asset.aspect_ratio
                    ),
                ));
            }
            Some(declared_ratio) => {
                if asset.height_px > 0 {
                    let actual_ratio = asset.width_px as f64 / asset.height_px as f64;
                    let rel_diff = (declared_ratio - actual_ratio).abs() / actual_ratio;
                    if rel_diff > 0.01 {
                        violations.push(ValidationViolation::invalid(
                            "aspect_ratio",
                            format!(
                                "declared aspect ratio '{}' does not match computed ratio {:.2} from dimensions {}x{}px",
                                asset.aspect_ratio, actual_ratio, asset.width_px, asset.height_px
                            ),
                        ));
                    }
                }

                if !self.aspect_ratios.is_empty() {
                    let matches_allowed = self.aspect_ratios.iter().any(|allowed| {
                        parse_aspect_ratio(allowed).is_some_and(|spec_ratio| {
                            (declared_ratio - spec_ratio).abs() / spec_ratio <= 0.01
                        })
                    });
                    if !matches_allowed {
                        violations.push(ValidationViolation::unsupported(
                            "aspect_ratio",
                            format!(
                                "aspect ratio '{}' is not supported; allowed aspect ratios: [{}]",
                                asset.aspect_ratio,
                                self.aspect_ratios.join(", ")
                            ),
                        ));
                    }
                }
            }
        }

        if let Some(min_width) = self.min_width_px.filter(|&min| asset.width_px < min) {
            violations.push(ValidationViolation::invalid(
                "width_px",
                format!(
                    "asset width ({}px) is below the minimum required width of {}px",
                    asset.width_px, min_width
                ),
            ));
        }

        if let Some(min_height) = self.min_height_px.filter(|&min| asset.height_px < min) {
            violations.push(ValidationViolation::invalid(
                "height_px",
                format!(
                    "asset height ({}px) is below the minimum required height of {}px",
                    asset.height_px, min_height
                ),
            ));
        }

        if let Some(max_file_size) = self
            .max_file_size_bytes
            .filter(|&max| asset.file_size_bytes > max)
        {
            violations.push(ValidationViolation::invalid(
                "file_size_bytes",
                format!(
                    "asset file size ({} bytes) exceeds the maximum allowed {} bytes",
                    asset.file_size_bytes, max_file_size
                ),
            ));
        }

        if asset.is_video {
            let has_duration_bounds =
                self.min_duration_seconds.is_some() || self.max_duration_seconds.is_some();
            match asset.duration_seconds {
                None if has_duration_bounds => {
                    violations.push(ValidationViolation::invalid(
                        "duration_seconds",
                        "video duration is required for this placement",
                    ));
                }
                Some(duration) => {
                    if let Some(min_duration) =
                        self.min_duration_seconds.filter(|&min| duration < min)
                    {
                        violations.push(ValidationViolation::invalid(
                            "duration_seconds",
                            format!(
                                "video duration ({}s) is below the minimum allowed {}s",
                                duration, min_duration
                            ),
                        ));
                    }

                    if let Some(max_duration) =
                        self.max_duration_seconds.filter(|&max| duration > max)
                    {
                        violations.push(ValidationViolation::invalid(
                            "duration_seconds",
                            format!(
                                "video duration ({}s) exceeds the maximum allowed {}s",
                                duration, max_duration
                            ),
                        ));
                    }
                }
                None => {}
            }
        }

        violations
    }
}

/// Data consumed by both the validator and the frontend form engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CapabilityMatrix {
    pub matrix_version: String,
    pub objectives: BTreeSet<Objective>,
    pub budget_types: BTreeSet<BudgetType>,
    pub minimum_budgets_minor: BTreeMap<String, BTreeMap<BudgetType, i64>>,
    pub schedule_granularity: ScheduleGranularity,
    pub targeting_dimensions: BTreeSet<TargetingDimension>,
    pub creative_placements: Vec<CreativePlacement>,
    /// Locale -> field name -> inclusive character limit.
    pub text_limits: BTreeMap<String, BTreeMap<String, usize>>,
}

impl CapabilityMatrix {
    pub fn text_limit(&self, locale: &str, field: &str) -> Option<usize> {
        self.text_limits
            .get(locale)
            .or_else(|| self.text_limits.get("default"))
            .and_then(|limits| limits.get(field))
            .copied()
    }

    pub fn validate_creative_text(&self, texts: &[CreativeText]) -> Vec<ValidationViolation> {
        let mut violations = Vec::new();

        for (text_index, text) in texts.iter().enumerate() {
            for (field, value) in [("headline", &text.headline), ("body", &text.body)] {
                let field_path = format!("texts[{text_index}].{field}");
                let char_count = value.chars().count();
                match self.text_limit(&text.locale, field) {
                    Some(limit) if char_count > limit => {
                        violations.push(ValidationViolation::invalid(
                            field_path,
                            format!(
                                "{field} exceeds the {limit}-character {} limit",
                                text.locale
                            ),
                        ));
                    }
                    None => {
                        violations.push(ValidationViolation::unsupported(
                            field_path,
                            format!("{field} has no limit for locale {}", text.locale),
                        ));
                    }
                    Some(_) => {}
                }
            }
        }

        violations
    }

    pub fn minimum_budget(&self, currency: &str, kind: BudgetType) -> Option<i64> {
        self.minimum_budgets_minor
            .get(currency)
            .and_then(|by_type| by_type.get(&kind))
            .copied()
    }

    pub fn placement(&self, key: &str) -> Option<&CreativePlacement> {
        self.creative_placements
            .iter()
            .find(|placement| placement.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> CreativeAssetSpec {
        CreativeAssetSpec {
            image_required: true,
            video_allowed: true,
            aspect_ratios: vec!["1:1".to_string(), "4:5".to_string()],
            min_width_px: Some(600),
            min_height_px: Some(600),
            max_file_size_bytes: Some(30_000_000),
            min_duration_seconds: Some(1),
            max_duration_seconds: Some(240),
        }
    }

    fn sample_image_asset() -> AssetMetadata {
        AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 2_000_000,
            is_video: false,
            duration_seconds: None,
            aspect_ratio: "1:1".to_string(),
        }
    }

    #[test]
    fn valid_asset_passes_with_empty_violations() {
        let spec = sample_spec();
        let asset = sample_image_asset();
        let violations = spec.validate_asset(&asset);
        assert!(violations.is_empty());
    }

    #[test]
    fn asset_with_unsupported_aspect_ratio_is_rejected() {
        let spec = sample_spec();
        let asset = AssetMetadata {
            width_px: 1920,
            height_px: 1080,
            file_size_bytes: 2_000_000,
            is_video: false,
            duration_seconds: None,
            aspect_ratio: "16:9".to_string(),
        };
        let violations = spec.validate_asset(&asset);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].field_path, "aspect_ratio");
        assert_eq!(violations[0].code, "capability_unsupported");
        assert!(violations[0].message.to_lowercase().contains("aspect ratio"));
    }

    #[test]
    fn aspect_ratio_mismatch_with_actual_dimensions_is_rejected() {
        let spec = sample_spec();
        let asset = AssetMetadata {
            width_px: 1080,
            height_px: 1920,
            file_size_bytes: 2_000_000,
            is_video: false,
            duration_seconds: None,
            aspect_ratio: "1:1".to_string(),
        };
        let violations = spec.validate_asset(&asset);
        assert!(
            violations.iter().any(|v| {
                v.field_path == "aspect_ratio"
                    && v.message.contains("1:1")
                    && (v.message.contains("computed") || v.message.contains("0.56"))
            }),
            "Expected mismatch violation naming declared and computed ratio: {violations:?}"
        );
    }

    #[test]
    fn equivalent_aspect_ratio_format_is_accepted_against_allowed_spec() {
        let spec = sample_spec();
        let asset = AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 2_000_000,
            is_video: false,
            duration_seconds: None,
            aspect_ratio: "1080:1080".to_string(),
        };
        let violations = spec.validate_asset(&asset);
        assert!(violations.is_empty(), "Expected 1080:1080 to match 1:1: {violations:?}");
    }

    #[test]
    fn unparseable_aspect_ratio_is_rejected() {
        let spec = sample_spec();
        let mut asset = sample_image_asset();
        asset.aspect_ratio = "not_a_ratio".to_string();
        let violations = spec.validate_asset(&asset);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].field_path, "aspect_ratio");
        assert_eq!(violations[0].code, "invalid");
        assert!(violations[0].message.contains("not_a_ratio"));
    }

    #[test]
    fn asset_over_max_file_size_is_rejected() {
        let spec = sample_spec();
        let mut asset = sample_image_asset();
        asset.file_size_bytes = 40_000_000;
        let violations = spec.validate_asset(&asset);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].field_path, "file_size_bytes");
        assert_eq!(violations[0].code, "invalid");
        assert!(violations[0].message.contains("file size"));
    }

    #[test]
    fn video_asset_with_duration_outside_window_is_rejected() {
        let too_short = AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 5_000_000,
            is_video: true,
            duration_seconds: Some(0),
            aspect_ratio: "1:1".to_string(),
        };

        let video_spec = CreativeAssetSpec {
            image_required: false,
            video_allowed: true,
            aspect_ratios: vec!["1:1".to_string()],
            min_width_px: Some(600),
            min_height_px: Some(600),
            max_file_size_bytes: Some(50_000_000),
            min_duration_seconds: Some(5),
            max_duration_seconds: Some(60),
        };
        let short_violations = video_spec.validate_asset(&too_short);
        assert!(
            short_violations
                .iter()
                .any(|v| v.field_path == "duration_seconds" && v.message.contains("duration") && v.message.contains("below"))
        );

        let too_long = AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 5_000_000,
            is_video: true,
            duration_seconds: Some(120),
            aspect_ratio: "1:1".to_string(),
        };
        let long_violations = video_spec.validate_asset(&too_long);
        assert!(
            long_violations
                .iter()
                .any(|v| v.field_path == "duration_seconds" && v.message.contains("duration") && v.message.contains("exceeds"))
        );
    }

    #[test]
    fn video_asset_with_missing_duration_when_bounds_declared_is_rejected() {
        let spec = CreativeAssetSpec {
            image_required: false,
            video_allowed: true,
            aspect_ratios: vec!["1:1".to_string()],
            min_width_px: Some(600),
            min_height_px: Some(600),
            max_file_size_bytes: Some(30_000_000),
            min_duration_seconds: Some(1),
            max_duration_seconds: Some(60),
        };
        let video_without_duration = AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 5_000_000,
            is_video: true,
            duration_seconds: None,
            aspect_ratio: "1:1".to_string(),
        };
        let violations = spec.validate_asset(&video_without_duration);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].field_path, "duration_seconds");
        assert_eq!(violations[0].code, "invalid");
        assert!(violations[0].message.contains("video duration is required for this placement"));
    }

    #[test]
    fn image_asset_with_duration_seconds_ignores_duration_bounds() {
        let spec = CreativeAssetSpec {
            image_required: true,
            video_allowed: true,
            aspect_ratios: vec!["1:1".to_string()],
            min_width_px: Some(600),
            min_height_px: Some(600),
            max_file_size_bytes: Some(30_000_000),
            min_duration_seconds: Some(5),
            max_duration_seconds: Some(60),
        };
        let image_with_duration = AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 2_000_000,
            is_video: false,
            duration_seconds: Some(0),
            aspect_ratio: "1:1".to_string(),
        };
        let violations = spec.validate_asset(&image_with_duration);
        assert!(violations.is_empty(), "Image asset should not be checked for duration: {violations:?}");
    }

    #[test]
    fn video_asset_passes_on_spec_with_image_required_and_video_allowed() {
        let spec = CreativeAssetSpec {
            image_required: true,
            video_allowed: true,
            aspect_ratios: vec!["1:1".to_string(), "4:5".to_string()],
            min_width_px: Some(600),
            min_height_px: Some(600),
            max_file_size_bytes: Some(31_457_280),
            min_duration_seconds: Some(1),
            max_duration_seconds: Some(14460),
        };
        let valid_video = AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 10_000_000,
            is_video: true,
            duration_seconds: Some(30),
            aspect_ratio: "1:1".to_string(),
        };
        let violations = spec.validate_asset(&valid_video);
        assert!(violations.is_empty(), "Valid video should pass: {violations:?}");
    }

    #[test]
    fn video_asset_duration_bounds_enforced_when_image_required_and_video_allowed() {
        let spec = CreativeAssetSpec {
            image_required: true,
            video_allowed: true,
            aspect_ratios: vec!["1:1".to_string(), "4:5".to_string()],
            min_width_px: Some(600),
            min_height_px: Some(600),
            max_file_size_bytes: Some(31_457_280),
            min_duration_seconds: Some(5),
            max_duration_seconds: Some(180),
        };
        let out_of_bounds_video = AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 10_000_000,
            is_video: true,
            duration_seconds: Some(300),
            aspect_ratio: "1:1".to_string(),
        };
        let violations = spec.validate_asset(&out_of_bounds_video);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].field_path, "duration_seconds");
        assert_eq!(violations[0].code, "invalid");
        assert!(violations[0].message.contains("exceeds"));
    }

    #[test]
    fn video_asset_rejected_when_video_not_allowed() {
        let image_only_spec = CreativeAssetSpec {
            image_required: true,
            video_allowed: false,
            aspect_ratios: vec!["1:1".to_string()],
            min_width_px: Some(600),
            min_height_px: Some(600),
            max_file_size_bytes: Some(10_000_000),
            min_duration_seconds: None,
            max_duration_seconds: None,
        };

        let video_asset = AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 5_000_000,
            is_video: true,
            duration_seconds: Some(15),
            aspect_ratio: "1:1".to_string(),
        };

        let violations = image_only_spec.validate_asset(&video_asset);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].field_path, "is_video");
        assert_eq!(violations[0].code, "capability_unsupported");
        assert!(violations[0].message.contains("not allowed"));
    }

    #[test]
    fn validating_same_asset_against_two_placements_scopes_rejection_per_placement() {
        let feed_placement = CreativePlacement {
            key: "feed".to_string(),
            asset_spec: CreativeAssetSpec {
                image_required: true,
                video_allowed: true,
                aspect_ratios: vec!["1:1".to_string(), "4:5".to_string()],
                min_width_px: Some(600),
                min_height_px: Some(600),
                max_file_size_bytes: Some(30_000_000),
                min_duration_seconds: None,
                max_duration_seconds: None,
            },
        };

        let story_placement = CreativePlacement {
            key: "story".to_string(),
            asset_spec: CreativeAssetSpec {
                image_required: true,
                video_allowed: true,
                aspect_ratios: vec!["9:16".to_string()],
                min_width_px: Some(1080),
                min_height_px: Some(1920),
                max_file_size_bytes: Some(30_000_000),
                min_duration_seconds: None,
                max_duration_seconds: None,
            },
        };

        let matrix = CapabilityMatrix {
            matrix_version: "2026-08-25".to_string(),
            objectives: BTreeSet::new(),
            budget_types: BTreeSet::new(),
            minimum_budgets_minor: BTreeMap::new(),
            schedule_granularity: ScheduleGranularity::Daily,
            targeting_dimensions: BTreeSet::new(),
            creative_placements: vec![feed_placement, story_placement],
            text_limits: BTreeMap::new(),
        };

        let asset = AssetMetadata {
            width_px: 1080,
            height_px: 1080,
            file_size_bytes: 2_000_000,
            is_video: false,
            duration_seconds: None,
            aspect_ratio: "1:1".to_string(),
        };

        let feed_spec = &matrix.placement("feed").unwrap().asset_spec;
        let story_spec = &matrix.placement("story").unwrap().asset_spec;

        let feed_violations = feed_spec.validate_asset(&asset);
        let story_violations = story_spec.validate_asset(&asset);

        assert!(
            feed_violations.is_empty(),
            "Asset should pass feed placement"
        );
        assert!(
            !story_violations.is_empty(),
            "Asset should fail story placement"
        );
        assert!(story_violations.iter().any(|v| v.message.contains("aspect ratio")));
    }

    #[test]
    fn asset_with_dimensions_below_minimum_is_rejected() {
        let spec = sample_spec();
        let mut asset = sample_image_asset();
        asset.width_px = 500;
        let violations = spec.validate_asset(&asset);
        assert!(violations.iter().any(|v| v.field_path == "width_px" && v.message.contains("width")));

        asset.width_px = 1080;
        asset.height_px = 400;
        let violations_h = spec.validate_asset(&asset);
        assert!(violations_h.iter().any(|v| v.field_path == "height_px" && v.message.contains("height")));
    }

    #[test]
    fn creative_asset_spec_deserializes_with_missing_optional_fields() {
        let json = r#"{
            "image_required": true,
            "video_allowed": false,
            "aspect_ratios": ["1:1"]
        }"#;
        let spec: CreativeAssetSpec = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(spec.min_width_px, None);
        assert_eq!(spec.min_height_px, None);
        assert_eq!(spec.max_file_size_bytes, None);
        assert_eq!(spec.min_duration_seconds, None);
        assert_eq!(spec.max_duration_seconds, None);
    }

    fn sample_text_limits_matrix() -> CapabilityMatrix {
        let mut en = BTreeMap::new();
        en.insert("headline".to_string(), 40usize);
        en.insert("body".to_string(), 90usize);
        let mut ja = BTreeMap::new();
        ja.insert("headline".to_string(), 25usize);
        ja.insert("body".to_string(), 45usize);
        let mut text_limits = BTreeMap::new();
        text_limits.insert("en".to_string(), en);
        text_limits.insert("ja".to_string(), ja);

        CapabilityMatrix {
            matrix_version: "2026-08-24".to_string(),
            objectives: BTreeSet::new(),
            budget_types: BTreeSet::new(),
            minimum_budgets_minor: BTreeMap::new(),
            schedule_granularity: ScheduleGranularity::Daily,
            targeting_dimensions: BTreeSet::new(),
            creative_placements: Vec::new(),
            text_limits,
        }
    }

    #[test]
    fn identical_short_copy_passes_for_english_and_japanese() {
        let matrix = sample_text_limits_matrix();
        let texts = vec![
            CreativeText {
                locale: "en".to_string(),
                headline: "Short headline here".to_string(),
                body: "Short body copy.".to_string(),
            },
            CreativeText {
                locale: "ja".to_string(),
                headline: "Short headline here".to_string(),
                body: "Short body copy.".to_string(),
            },
        ];
        assert!(matrix.validate_creative_text(&texts).is_empty());
    }

    #[test]
    fn identical_headline_passes_english_and_fails_shorter_japanese_limit() {
        let matrix = sample_text_limits_matrix();
        // 29 chars: fits English's 40-char limit, exceeds Japanese's 25-char limit.
        let headline = "This headline is thirty chars".to_string();
        assert_eq!(headline.chars().count(), 29);

        let texts = vec![
            CreativeText {
                locale: "en".to_string(),
                headline: headline.clone(),
                body: String::new(),
            },
            CreativeText {
                locale: "ja".to_string(),
                headline,
                body: String::new(),
            },
        ];

        let violations = matrix.validate_creative_text(&texts);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "invalid");
        assert_eq!(violations[0].field_path, "texts[1].headline");
        assert!(violations[0].message.contains("ja"));
        assert!(violations[0].message.contains("headline"));
    }

    #[test]
    fn headline_and_body_violations_are_both_reported_independently() {
        let matrix = sample_text_limits_matrix();
        let texts = vec![CreativeText {
            locale: "ja".to_string(),
            headline: "a".repeat(30),
            body: "b".repeat(50),
        }];

        let violations = matrix.validate_creative_text(&texts);
        assert_eq!(violations.len(), 2);
        assert!(violations.iter().all(|v| v.code == "invalid"));
        assert!(violations.iter().any(|v| v.message.contains("headline")));
        assert!(violations.iter().any(|v| v.message.contains("body")));
    }

    #[test]
    fn locale_with_no_configured_limit_is_rejected_as_unsupported() {
        let matrix = sample_text_limits_matrix();
        let texts = vec![CreativeText {
            locale: "de".to_string(),
            headline: "a".repeat(200),
            body: "b".repeat(200),
        }];
        let violations = matrix.validate_creative_text(&texts);
        assert_eq!(violations.len(), 2);
        assert!(violations.iter().all(|v| v.code == "capability_unsupported"));
        assert!(violations.iter().any(|v| v.message.contains("headline has no limit for locale de")));
        assert!(violations.iter().any(|v| v.message.contains("body has no limit for locale de")));
    }
}
