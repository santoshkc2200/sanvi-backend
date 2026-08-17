//! Brand asset validation: type sniffing (never extension trust),
//! dimension caps, EXIF stripping and content addressing.
//!
//! SVG uploads are rejected outright: without a full XML processor the
//! platform cannot prove the upload is script-free, and the plan allows
//! "sanitised or converted" — this release rejects instead (fail closed).

use sha2::{Digest, Sha256};

use crate::domain::error::ThemingError;
use crate::domain::tenant_theme::BrandAssetKind;

/// Size cap per upload (2 MiB covers retina logos and OG images).
pub const MAX_ASSET_BYTES: usize = 2 * 1024 * 1024;

/// Dimension cap (guards decompression bombs at the source).
pub const MAX_ASSET_DIMENSION: u32 = 4096;

/// A validated asset: sniffed format, cleaned bytes, dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAsset {
    pub mime: &'static str,
    pub ext: &'static str,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl ValidatedAsset {
    pub fn digest_hex(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(&self.bytes);
        hex::encode(hasher.finalize())
    }
}

/// Validate an upload by sniffing its magic bytes, extracting dimensions and
/// stripping metadata. Returns the cleaned bytes.
pub fn validate_asset(kind: BrandAssetKind, raw: &[u8]) -> Result<ValidatedAsset, ThemingError> {
    let reject = |msg: String| ThemingError::InvalidBrandAsset(msg);

    if raw.len() > MAX_ASSET_BYTES {
        return Err(reject(format!(
            "asset is {} bytes, over the {} byte cap",
            raw.len(),
            MAX_ASSET_BYTES
        )));
    }
    if raw.is_empty() {
        return Err(reject("asset is empty".to_string()));
    }

    // SVG is rejected before anything else: the tell-tale markers are in the
    // first bytes or a comment/CDATA prefix.
    if looks_like_svg(raw) {
        return Err(reject(
            "svg uploads are not supported (possible script execution); upload png, webp, jpeg or ico"
                .to_string(),
        ));
    }

    if is_png(raw) {
        let (width, height) =
            png_dimensions(raw).ok_or_else(|| reject("malformed png".to_string()))?;
        return finish("image/png", "png", raw.to_vec(), width, height, kind);
    }
    if is_webp(raw) {
        let (width, height) =
            webp_dimensions(raw).ok_or_else(|| reject("malformed webp".to_string()))?;
        return finish("image/webp", "webp", raw.to_vec(), width, height, kind);
    }
    if is_jpeg(raw) {
        let cleaned =
            strip_jpeg_metadata(raw).ok_or_else(|| reject("malformed jpeg".to_string()))?;
        let (width, height) =
            jpeg_dimensions(&cleaned).ok_or_else(|| reject("malformed jpeg".to_string()))?;
        return finish("image/jpeg", "jpg", cleaned, width, height, kind);
    }
    if matches!(kind, BrandAssetKind::Favicon) && is_ico(raw) {
        let (width, height) =
            ico_dimensions(raw).ok_or_else(|| reject("malformed ico".to_string()))?;
        return finish("image/x-icon", "ico", raw.to_vec(), width, height, kind);
    }

    Err(reject(
        "unsupported image format; upload png, webp or jpeg (favicon: ico)".to_string(),
    ))
}

fn finish(
    mime: &'static str,
    ext: &'static str,
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    kind: BrandAssetKind,
) -> Result<ValidatedAsset, ThemingError> {
    if width == 0 || height == 0 || width > MAX_ASSET_DIMENSION || height > MAX_ASSET_DIMENSION {
        return Err(ThemingError::InvalidBrandAsset(format!(
            "{} asset dimensions {width}x{height} exceed the {MAX_ASSET_DIMENSION}px cap",
            kind.as_str()
        )));
    }
    Ok(ValidatedAsset {
        mime,
        ext,
        bytes,
        width,
        height,
    })
}

fn looks_like_svg(raw: &[u8]) -> bool {
    // Whitespace/XML-declaration/comment-tolerant: any leading run of these
    // followed by `<svg` marks an SVG document.
    let mut index = 0;
    let mut i = 0;
    while i + 4 <= raw.len() {
        let window = &raw[i..];
        if window.starts_with(b"<svg") || window.starts_with(b"<!") || window.starts_with(b"<?") {
            return true;
        }
        if raw[i] == b' ' || raw[i] == b'\t' || raw[i] == b'\n' || raw[i] == b'\r' {
            i += 1;
            index += 1;
            if index > 512 {
                return false;
            }
        } else {
            break;
        }
    }
    false
}

fn is_png(raw: &[u8]) -> bool {
    raw.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
}

fn png_dimensions(raw: &[u8]) -> Option<(u32, u32)> {
    if raw.len() < 24 || !raw.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    // IHDR is the first chunk.
    if &raw[12..16] != b"IHDR" {
        return None;
    }
    Some((be_u32(raw, 16)?, be_u32(raw, 20)?))
}

fn is_webp(raw: &[u8]) -> bool {
    raw.len() >= 12 && &raw[0..4] == b"RIFF" && &raw[8..12] == b"WEBP"
}

fn webp_dimensions(raw: &[u8]) -> Option<(u32, u32)> {
    if !is_webp(raw) || raw.len() < 30 {
        return None;
    }
    match &raw[12..16] {
        b"VP8 " => Some((le_u16(raw, 26)? as u32, le_u16(raw, 28)? as u32)),
        b"VP8L" => {
            if raw[20] != 0x2F {
                return None;
            }
            let bits = u32::from_be_bytes([0, raw[21], raw[22], raw[23]]);
            let width = (bits & 0x3FFF) + 1;
            let height = ((bits >> 14) & 0x3FFF) + 1;
            Some((width, height))
        }
        b"VP8X" => {
            let width = le_u24(raw, 24)? + 1;
            let height = le_u24(raw, 27)? + 1;
            Some((width, height))
        }
        _ => None,
    }
}

fn is_jpeg(raw: &[u8]) -> bool {
    raw.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Walk JPEG segments and drop metadata-bearing APP1 (EXIF/XMP) segments.
/// APP0 (JFIF) and APP2 (ICC) are kept; any unknown APPn without a
/// well-formed length header fails closed.
fn strip_jpeg_metadata(raw: &[u8]) -> Option<Vec<u8>> {
    if !is_jpeg(raw) {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len());
    out.extend_from_slice(&raw[..2]);
    let mut i = 2;
    while i + 2 <= raw.len() {
        if raw[i] != 0xFF {
            return None;
        }
        let marker = raw[i + 1];
        // Standalone markers (no length): copy and continue.
        if matches!(marker, 0x01 | 0xD0..=0xD9) {
            out.push(0xFF);
            out.push(marker);
            i += 2;
            if marker == 0xD9 {
                // EOI: keep the remainder byte-exact.
                out.extend_from_slice(&raw[i..]);
                return Some(out);
            }
            continue;
        }
        // Length-bearing segment: 2 length bytes + payload.
        let length = be_u16(raw, i + 2)? as usize;
        if length < 2 || i + 2 + length > raw.len() {
            return None;
        }
        let payload = &raw[i + 4..i + 2 + length];
        let is_exif = marker == 0xE1 && payload.starts_with(b"Exif\x00\x00");
        let is_xmp = marker == 0xE1 && payload.starts_with(b"http://ns.adobe.com/xap/1.0/\x00");
        if !(is_exif || is_xmp) {
            out.extend_from_slice(&raw[i..i + 2 + length]);
        }
        i += 2 + length;
    }
    // Reached the end without EOI.
    None
}

fn jpeg_dimensions(raw: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 <= raw.len() {
        if raw[i] != 0xFF {
            return None;
        }
        let marker = raw[i + 1];
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            return Some((be_u16(raw, i + 7)? as u32, be_u16(raw, i + 5)? as u32));
        }
        if matches!(marker, 0x01 | 0xD0..=0xD9) {
            i += 2;
            continue;
        }
        let length = be_u16(raw, i + 2)? as usize;
        if length < 2 || i + 2 + length > raw.len() {
            return None;
        }
        i += 2 + length;
    }
    None
}

fn is_ico(raw: &[u8]) -> bool {
    raw.len() >= 6 && raw[0] == 0x00 && raw[1] == 0x00 && raw[2] == 0x01 && raw[3] == 0x00
}

fn ico_dimensions(raw: &[u8]) -> Option<(u32, u32)> {
    let count = be_u16(raw, 4)? as usize;
    if 6 + count * 16 > raw.len() {
        return None;
    }
    // The largest declared entry.
    let mut best = (0u32, 0u32);
    for entry in 0..count {
        let offset = 6 + entry * 16;
        let width = if raw[offset] == 0 {
            256
        } else {
            raw[offset] as u32
        };
        let height = if raw[offset + 1] == 0 {
            256
        } else {
            raw[offset + 1] as u32
        };
        if width * height > best.0 * best.1 {
            best = (width, height);
        }
    }
    Some(best)
}

fn be_u16(raw: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *raw.get(offset)?,
        *raw.get(offset + 1)?,
    ]))
}

fn be_u32(raw: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *raw.get(offset)?,
        *raw.get(offset + 1)?,
        *raw.get(offset + 2)?,
        *raw.get(offset + 3)?,
    ]))
}

fn le_u24(raw: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *raw.get(offset)?,
        *raw.get(offset + 1)?,
        *raw.get(offset + 2)?,
        0,
    ]))
}

fn le_u16(raw: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes([
        *raw.get(offset)?,
        *raw.get(offset + 1)?,
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal 2x2 PNG (transparent).
    const MINI_PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn accepts_a_minimal_png() {
        let asset = validate_asset(BrandAssetKind::Logo, MINI_PNG).unwrap();
        assert_eq!(asset.mime, "image/png");
        assert_eq!((asset.width, asset.height), (2, 2));
        assert_eq!(asset.digest_hex().len(), 64);
    }

    #[test]
    fn rejects_svg_even_with_script_markers() {
        for svg in [
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>".as_slice(),
            b"<?xml version=\"1.0\"?>\n<svg/>".as_slice(),
            b"  <!-- comment --> <svg/>".as_slice(),
            b"<svg onload=\"alert(1)\"/>".as_slice(),
        ] {
            let err = validate_asset(BrandAssetKind::Logo, svg).unwrap_err();
            assert!(matches!(err, ThemingError::InvalidBrandAsset(_)), "{svg:?}");
        }
    }

    #[test]
    fn rejects_unknown_magic_bytes() {
        let err = validate_asset(BrandAssetKind::Logo, b"#!/bin/sh\necho pwn").unwrap_err();
        assert!(matches!(err, ThemingError::InvalidBrandAsset(_)));
    }

    #[test]
    fn rejects_oversized_uploads() {
        let mut big = vec![0u8; MAX_ASSET_BYTES + 1];
        big[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let err = validate_asset(BrandAssetKind::Logo, &big).unwrap_err();
        assert!(matches!(err, ThemingError::InvalidBrandAsset(_)));
    }

    #[test]
    fn strips_exif_from_jpeg() {
        // Minimal JPEG: SOI + APP1(EXIF) + EOI (no SOF chunk; this fixture
        // exercises the metadata walk, not dimension parsing).
        let mut jpeg = vec![0xFF, 0xD8];
        let exif = b"Exif\x00\x00\x49\x49\x2a\x00";
        let mut segment = vec![0xFF, 0xE1];
        segment.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(exif);
        jpeg.extend_from_slice(&segment);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        let stripped = strip_jpeg_metadata(&jpeg).unwrap();
        assert!(!stripped.windows(5).any(|w| w == b"Exif\x00"));
        assert!(stripped.starts_with(&[0xFF, 0xD8]));
        assert!(stripped.ends_with(&[0xFF, 0xD9]));
    }

    #[test]
    fn webp_dimensions_parse() {
        // RIFF....WEBP + VP8X chunk with canvas size fields.
        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF\x00\x00\x00\x00WEBP");
        webp.extend_from_slice(b"VP8X");
        webp.extend_from_slice(&10u32.to_le_bytes()); // chunk size
        webp.push(0x10); // flags: has ICC? no — any flags ok
        webp.extend_from_slice(&[0, 0, 0]); // reserved
        webp.extend_from_slice(&[63, 0, 0]); // width - 1 (24-bit LE)
        webp.extend_from_slice(&[63, 0, 0]); // height - 1
        let (width, height) = webp_dimensions(&webp).unwrap();
        assert_eq!((width, height), (64, 64));
    }
}
