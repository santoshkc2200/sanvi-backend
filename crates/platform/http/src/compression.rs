//! Response compression with a size floor (TASK-021 step 7).
//!
//! Why not `actix_web::middleware::Compress`: it compresses every byte it is
//! offered — no floor, so a 200-byte problem body pays framing plus CPU to
//! save nothing — and its type predicate still encodes `application/zip`,
//! audio and other already-compressed representations. [`SelectiveCompression`]
//! negotiates with the same `Accept-Encoding` rules (`AcceptEncoding::negotiate`,
//! brotli preferred over zstd over gzip over deflate at equal q) and encodes
//! with the same `Encoder`, but refuses responses below
//! [`CompressionConfig::min_bytes`] and responses whose type or existing
//! `Content-Encoding` says they are already compressed. `Vary:
//! Accept-Encoding` rides on every encoded response (the stock middleware
//! only sets it on its 406 path).

use actix_http::encoding::Encoder;
use actix_web::{
    HttpMessage as _,
    body::{BodySize, BoxBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    http::header::{self, AcceptEncoding, ContentEncoding, Encoding},
};

/// Response compression policy: one switch, one floor.
///
/// `enabled = false` is the rollback position — every response passes
/// through untouched. `min_bytes = 0` disables the floor (every eligible
/// representation encodes); empty bodies never encode regardless, the
/// `Encoder` declines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CompressionConfig {
    pub enabled: bool,
    pub min_bytes: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Below ~1 KiB the gzip/brotli framing plus CPU exceeds what the
            // transfer saves on any modern link — and our smallest responses
            // (problem bodies, 150–400 B) all sit under it. The floor's
            // honesty at this API's payload sizes is recorded in
            // `benchmarks/transport-2026-10-06.md`.
            min_bytes: 1024,
        }
    }
}

impl CompressionConfig {
    /// The pure eligibility decision — the unit-tested core of the slice.
    ///
    /// - `body_len`: `Some(n)` for a sized body, `None` for an unknown-length
    ///   stream (which cannot be proven small, so it stays eligible).
    /// - `content_type`: the response's `Content-Type`, if any. Unknown or
    ///   unparseable types fail closed (identity) — a new textual
    ///   representation opts in by joining the allow-list, not by review.
    /// - `content_encoding`: the response's `Content-Encoding`, if any. Any
    ///   value but `identity` means the bytes are already encoded and
    ///   encoding again would be double compression.
    pub fn should_compress(
        &self,
        body_len: Option<u64>,
        content_type: Option<&str>,
        content_encoding: Option<&str>,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(encoding) = content_encoding
            && !encoding.trim().eq_ignore_ascii_case("identity")
        {
            return false;
        }
        let Some(content_type) = content_type else {
            return false;
        };
        if !is_compressible_content_type(content_type) {
            return false;
        }
        match body_len {
            // Empty bodies never encode (the `Encoder` declines them too);
            // a zero floor means "everything non-empty", not "everything".
            Some(len) => len > 0 && len >= self.min_bytes as u64,
            None => true,
        }
    }
}

/// Textual representations only, matched case-insensitively with parameters
/// (`; charset=utf-8`) stripped. Everything else — images (save SVG),
/// video, audio, archives, pdf, fonts, octet-streams — is already
/// compressed or uncompressible, and encoding it burns CPU for zero or
/// negative savings.
pub fn is_compressible_content_type(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let (type_, subtype) = match mime.split_once('/') {
        Some(pair) => pair,
        None => return false,
    };
    if type_ == "text" {
        return true;
    }
    if subtype.ends_with("+json") || subtype.ends_with("+xml") {
        return true;
    }
    matches!(
        mime.as_str(),
        "application/json"
            | "application/xml"
            | "application/javascript"
            | "application/x-javascript"
            | "application/yaml"
            | "application/x-yaml"
    )
}

/// The encodings this server can emit, in `negotiate`'s input shape.
/// `identity` is included so a client that accepts nothing encoded still
/// gets a decision (identity) rather than a 406 — the 406 is reserved for
/// clients that refuse identity too.
static SUPPORTED_ENCODINGS: &[Encoding] = &[
    Encoding::identity(),
    Encoding::brotli(),
    Encoding::gzip(),
    Encoding::deflate(),
    Encoding::zstd(),
];

/// What the 406 body names when the client refuses every representation.
pub static SUPPORTED_ENCODINGS_LIST: &str = "br, gzip, deflate, zstd";

/// Compress eligible responses, pass everything else through byte-identical.
///
/// Negotiation happens up front (a pure function of the request headers, so
/// a refused-everything client gets its 406 without running the handler);
/// eligibility is decided on the response, where the size, type and prior
/// encoding are known. Runs *outside* [`crate::etag::ConditionalEtag`] in
/// the stack, so validators are always computed over the identity bytes.
#[derive(Clone, Copy)]
pub struct SelectiveCompression {
    config: CompressionConfig,
}

impl SelectiveCompression {
    pub fn new(config: CompressionConfig) -> Self {
        Self { config }
    }
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for SelectiveCompression
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = SelectiveCompressionService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(SelectiveCompressionService {
            service,
            config: self.config,
        }))
    }
}

pub struct SelectiveCompressionService<S> {
    service: S,
    config: CompressionConfig,
}

impl<S> actix_web::dev::Service<ServiceRequest> for SelectiveCompressionService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        // Missing *or unparseable* `Accept-Encoding` means identity: a
        // client that names no encoding gets the identity representation,
        // exactly like the stock middleware.
        let negotiated = match req.get_header::<AcceptEncoding>() {
            None => Some(Encoding::identity()),
            Some(accept) => accept.negotiate(SUPPORTED_ENCODINGS.iter()),
        };
        let config = self.config;
        match negotiated {
            // The client refuses identity and every encoding on offer.
            // Answer without running the handler, with the `Vary` the
            // decision depends on.
            None => Box::pin(async move {
                Ok(req.into_response(
                    actix_web::HttpResponse::NotAcceptable()
                        .insert_header((header::VARY, "Accept-Encoding"))
                        .body(SUPPORTED_ENCODINGS_LIST)
                        .map_into_boxed_body(),
                ))
            }),
            Some(Encoding::Known(ContentEncoding::Identity)) | Some(Encoding::Unknown(_)) => {
                Box::pin(self.service.call(req))
            }
            Some(Encoding::Known(encoding)) => {
                let fut = self.service.call(req);
                Box::pin(async move {
                    let res = fut.await?;
                    let eligible = {
                        let head = res.response();
                        let content_encoding = head
                            .headers()
                            .get(header::CONTENT_ENCODING)
                            .and_then(|value| value.to_str().ok());
                        let content_type = head
                            .headers()
                            .get(header::CONTENT_TYPE)
                            .and_then(|value| value.to_str().ok());
                        let body_len = match head.body().size() {
                            BodySize::Sized(len) => Some(len),
                            BodySize::None => Some(0),
                            BodySize::Stream => None,
                        };
                        config.should_compress(body_len, content_type, content_encoding)
                    };
                    if !eligible {
                        return Ok(res);
                    }
                    // `Encoder` declines the cases it owns (empty bodies,
                    // 101/204/206, a `Content-Encoding` that appeared
                    // downstream) and appends `Vary: Accept-Encoding` when
                    // it encodes — defence in depth under our own gates.
                    Ok(res.map_body(|head, body| Encoder::response(encoding, head, body).boxed()))
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(len: Option<u64>, content_type: Option<&str>, encoding: Option<&str>) -> bool {
        CompressionConfig::default().should_compress(len, content_type, encoding)
    }

    #[test]
    fn floor_applies_at_exactly_min_bytes() {
        // The boundary is `>=`: a body of exactly the floor still encodes.
        assert!(!decide(Some(1023), Some("application/json"), None));
        assert!(decide(Some(1024), Some("application/json"), None));
        assert!(decide(Some(4096), Some("application/json"), None));
        assert!(!decide(Some(0), Some("application/json"), None));
    }

    #[test]
    fn disabled_config_never_compresses() {
        let off = CompressionConfig {
            enabled: false,
            min_bytes: 0,
        };
        assert!(!off.should_compress(Some(1_000_000), Some("application/json"), None));
    }

    #[test]
    fn zero_floor_still_refuses_empty_and_ineligible_bodies() {
        let no_floor = CompressionConfig {
            enabled: true,
            min_bytes: 0,
        };
        assert!(no_floor.should_compress(Some(1), Some("application/json"), None));
        assert!(!no_floor.should_compress(Some(0), Some("application/json"), None));
        assert!(!no_floor.should_compress(Some(5000), Some("image/png"), None));
    }

    #[test]
    fn already_encoded_responses_pass_through() {
        for encoding in ["gzip", "br", "zstd", "deflate", "compress", "GZip"] {
            assert!(
                !decide(Some(8192), Some("application/json"), Some(encoding)),
                "{encoding} must not be encoded again"
            );
        }
        // `identity` is not an encoding — the representation is still raw.
        assert!(decide(
            Some(8192),
            Some("application/json"),
            Some("identity")
        ));
        assert!(decide(Some(8192), Some("application/json"), None));
    }

    #[test]
    fn textual_representations_encode() {
        for content_type in [
            "application/json",
            "application/problem+json",
            "application/vnd.api+json",
            "application/xml",
            "application/atom+xml",
            "application/javascript",
            "application/x-javascript",
            "text/plain",
            "text/html",
            "text/csv",
            "image/svg+xml",
            "application/json; charset=utf-8",
            "Application/JSON",
            "TEXT/HTML; charset=UTF-8",
        ] {
            assert!(
                decide(Some(8192), Some(content_type), None),
                "{content_type} should encode"
            );
        }
    }

    #[test]
    fn already_compressed_types_do_not_encode() {
        // The stock middleware encodes several of these; the floor test
        // below proves the difference on the wire.
        for content_type in [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/avif",
            "video/mp4",
            "audio/mpeg",
            "application/zip",
            "application/gzip",
            "application/zstd",
            "application/pdf",
            "application/octet-stream",
            "font/woff2",
            "multipart/form-data",
        ] {
            assert!(
                !decide(Some(1_000_000), Some(content_type), None),
                "{content_type} must not encode even far above the floor"
            );
        }
    }

    #[test]
    fn unknown_types_fail_closed() {
        assert!(!decide(Some(8192), None, None));
        assert!(!decide(Some(8192), Some(""), None));
        assert!(!decide(Some(8192), Some("not-a-mime"), None));
        assert!(!decide(Some(8192), Some("application/"), None));
    }

    #[test]
    fn unknown_length_streams_stay_eligible() {
        // A chunked stream cannot be proven small; refusing it would leave
        // the largest responses unencoded.
        assert!(decide(None, Some("application/json"), None));
        assert!(!decide(None, Some("image/png"), None));
    }
}
