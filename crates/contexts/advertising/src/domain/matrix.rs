use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{BudgetType, CreativeText, Objective, TargetingDimension};

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
    pub image_required: bool,
    pub video_allowed: bool,
    pub aspect_ratios: Vec<String>,
    pub min_width_px: Option<u32>,
    pub min_height_px: Option<u32>,
    pub max_file_size_bytes: Option<u64>,
    pub min_duration_seconds: Option<u32>,
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

impl CreativeAssetSpec {
    pub fn validate_asset(&self, asset: &AssetMetadata) -> Vec<String> {
        let mut violations = Vec::new();

        if !self.video_allowed && asset.is_video {
            violations.push("video assets are not allowed for this placement".to_string());
        }

        if self.image_required && asset.is_video {
            violations.push(
                "image asset is required for this placement, but a video was provided".to_string(),
            );
        }

        if !self.aspect_ratios.is_empty() && !self.aspect_ratios.contains(&asset.aspect_ratio) {
            violations.push(format!(
                "aspect ratio '{}' is not supported; allowed aspect ratios: [{}]",
                asset.aspect_ratio,
                self.aspect_ratios.join(", ")
            ));
        }

        if let Some(min_width) = self.min_width_px.filter(|&min| asset.width_px < min) {
            violations.push(format!(
                "asset width ({}px) is below the minimum required width of {}px",
                asset.width_px, min_width
            ));
        }

        if let Some(min_height) = self.min_height_px.filter(|&min| asset.height_px < min) {
            violations.push(format!(
                "asset height ({}px) is below the minimum required height of {}px",
                asset.height_px, min_height
            ));
        }

        if let Some(max_file_size) = self
            .max_file_size_bytes
            .filter(|&max| asset.file_size_bytes > max)
        {
            violations.push(format!(
                "asset file size ({} bytes) exceeds the maximum allowed {} bytes",
                asset.file_size_bytes, max_file_size
            ));
        }

        if let Some(duration) = asset.duration_seconds {
            if let Some(min_duration) = self.min_duration_seconds.filter(|&min| duration < min) {
                violations.push(format!(
                    "video duration ({}s) is below the minimum allowed {}s",
                    duration, min_duration
                ));
            }

            if let Some(max_duration) = self.max_duration_seconds.filter(|&max| duration > max) {
                violations.push(format!(
                    "video duration ({}s) exceeds the maximum allowed {}s",
                    duration, max_duration
                ));
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

    pub fn validate_creative_text(&self, texts: &[CreativeText]) -> Vec<String> {
        let mut violations = Vec::new();

        for text in texts {
            for (field, value) in [("headline", &text.headline), ("body", &text.body)] {
                let char_count = value.chars().count();
                if let Some(limit) = self
                    .text_limit(&text.locale, field)
                    .filter(|&limit| char_count > limit)
                {
                    violations.push(format!(
                        "{field} for locale '{}' is {char_count} characters, exceeding the limit of {limit}",
                        text.locale
                    ));
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
    fn asset_with_wrong_aspect_ratio_is_rejected_naming_aspect_ratio() {
        let spec = sample_spec();
        let mut asset = sample_image_asset();
        asset.aspect_ratio = "16:9".to_string();
        let violations = spec.validate_asset(&asset);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].to_lowercase().contains("aspect ratio"));
    }

    #[test]
    fn asset_over_max_file_size_is_rejected() {
        let spec = sample_spec();
        let mut asset = sample_image_asset();
        asset.file_size_bytes = 40_000_000;
        let violations = spec.validate_asset(&asset);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("file size"));
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
                .any(|v| v.contains("duration") && v.contains("below"))
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
                .any(|v| v.contains("duration") && v.contains("exceeds"))
        );
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
        assert!(story_violations.iter().any(|v| v.contains("aspect ratio")));
    }

    #[test]
    fn asset_with_dimensions_below_minimum_is_rejected() {
        let spec = sample_spec();
        let mut asset = sample_image_asset();
        asset.width_px = 500;
        let violations = spec.validate_asset(&asset);
        assert!(violations.iter().any(|v| v.contains("width")));

        asset.width_px = 1080;
        asset.height_px = 400;
        let violations_h = spec.validate_asset(&asset);
        assert!(violations_h.iter().any(|v| v.contains("height")));
    }

    fn meta_text_limits_matrix() -> CapabilityMatrix {
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
        let matrix = meta_text_limits_matrix();
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
        let matrix = meta_text_limits_matrix();
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
        assert!(violations[0].contains("ja"));
        assert!(violations[0].contains("headline"));
    }

    #[test]
    fn headline_and_body_violations_are_both_reported_independently() {
        let matrix = meta_text_limits_matrix();
        let texts = vec![CreativeText {
            locale: "ja".to_string(),
            headline: "a".repeat(30),
            body: "b".repeat(50),
        }];

        let violations = matrix.validate_creative_text(&texts);
        assert_eq!(violations.len(), 2);
        assert!(violations.iter().any(|v| v.contains("headline")));
        assert!(violations.iter().any(|v| v.contains("body")));
    }

    #[test]
    fn locale_with_no_configured_limit_is_skipped() {
        let matrix = meta_text_limits_matrix();
        let texts = vec![CreativeText {
            locale: "de".to_string(),
            headline: "a".repeat(200),
            body: "b".repeat(200),
        }];
        assert!(matrix.validate_creative_text(&texts).is_empty());
    }

    #[test]
    fn video_asset_rejected_when_video_not_allowed_or_image_required() {
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
        assert!(violations.iter().any(|v| v.contains("not allowed")));
        assert!(
            violations
                .iter()
                .any(|v| v.contains("image asset is required"))
        );
    }
}
