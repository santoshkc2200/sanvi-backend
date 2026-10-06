//! Content validators for conditional requests (TASK-021 step 7).
//!
//! [`etag_for_bytes`] derives a strong validator over a response's identity
//! bytes; [`if_none_match_matches`] evaluates an inbound `If-None-Match`
//! with the RFC 9110 weak comparison (`*`, lists, `W/` prefixes). The
//! [`ConditionalEtag`] middleware applies both to every successful GET/HEAD
//! response that does not already carry an `ETag` (the theming and
//! localization storefront handlers manage their own validators and are
//! left untouched).

use actix_web::{
    body::{BodySize, BoxBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    http::{
        StatusCode,
        header::{self, HeaderValue},
    },
};

/// A strong ETag over the response's identity bytes: `"<32 hex>"` (the
/// first 128 bits of SHA-256 — a change detector, not a fingerprint).
pub fn etag_for_bytes(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body);
    let digest = hasher.finalize();
    format!("\"{}\"", hex::encode(&digest[..16]))
}

/// True when an inbound `If-None-Match` precludes sending the representation
/// whose current validator is `etag`: `*` matches anything, otherwise any
/// listed tag matching under the weak comparison (RFC 9110 §13.1.2 — `W/`
/// prefixes ignored on both sides) matches. `None` (no precondition) never
/// matches.
pub fn if_none_match_matches(inbound: Option<&HeaderValue>, etag: &str) -> bool {
    let Some(inbound) = inbound else {
        return false;
    };
    let Ok(inbound) = inbound.to_str() else {
        return false;
    };
    let inbound = inbound.trim();
    if inbound == "*" {
        return true;
    }
    let ours = unquoted(etag.trim());
    inbound.split(',').any(|item| unquoted(item.trim()) == ours)
}

/// Strip the weakness indicator and the surrounding quotes, so `"abc"` and
/// `W/"abc"` compare equal. Unquoted garbage compares literally — a match
/// still requires exact tag equality, which is the security property.
fn unquoted(tag: &str) -> &str {
    let tag = tag.strip_prefix("W/").unwrap_or(tag);
    tag.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(tag)
}

/// Stamp a strong `ETag` on successful GET/HEAD responses and answer `304`
/// when the client's validator is current.
///
/// Scope, all deliberate:
/// - GET/HEAD only. `If-None-Match: *` on unsafe methods means "fail if the
///   resource exists" (RFC 9110 §13.1.2) — a different semantic this
///   middleware must not answer.
/// - 2xx only. Error bodies carry per-request correlation ids, so a
///   validator over them could never match; stamping one would be noise.
/// - Responses that already carry an `ETag` pass through untouched — the
///   storefront handlers that manage their own validators stay authoritative.
/// - Bodies whose length is unknown (streaming) or zero pass through: there
///   are no streaming GETs in this API today, and hashing a stream would
///   mean buffering it.
/// - `try_into_bytes` on `BoxBody` is zero-copy for the full bodies every
///   handler produces; a body that refuses still passes through unchanged.
#[derive(Clone, Copy)]
pub struct ConditionalEtag;

impl<S> actix_web::dev::Transform<S, ServiceRequest> for ConditionalEtag
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = ConditionalEtagService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(ConditionalEtagService { service }))
    }
}

pub struct ConditionalEtagService<S> {
    service: S,
}

impl<S> actix_web::dev::Service<ServiceRequest> for ConditionalEtagService<S>
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
        let conditional = matches!(
            *req.method(),
            actix_web::http::Method::GET | actix_web::http::Method::HEAD
        );
        let precondition = req.headers().get(header::IF_NONE_MATCH).cloned();
        let fut = self.service.call(req);
        Box::pin(async move {
            let res = fut.await?;
            if !conditional || !res.status().is_success() {
                return Ok(res);
            }
            if res.headers().contains_key(header::ETAG) {
                return Ok(res);
            }
            if !matches!(res.response().body().size(), BodySize::Sized(n) if n > 0) {
                return Ok(res);
            }
            Ok(res.map_body(|head, body| {
                let bytes = match body.try_into_bytes() {
                    Ok(bytes) => bytes,
                    Err(body) => return body,
                };
                let etag = etag_for_bytes(&bytes);
                if if_none_match_matches(precondition.as_ref(), &etag) {
                    head.status = StatusCode::NOT_MODIFIED;
                    // A 304 carries no representation: drop the headers that
                    // describe one. The validator is the only thing a cache
                    // needs to revalidate its stored copy.
                    head.headers.remove(header::CONTENT_TYPE);
                    head.headers.remove(header::CONTENT_LENGTH);
                    head.headers.remove(header::CONTENT_ENCODING);
                    head.headers.insert(
                        header::ETAG,
                        HeaderValue::from_str(&etag)
                            .unwrap_or_else(|_| HeaderValue::from_static("\"invalid\"")),
                    );
                    BoxBody::new(actix_web::web::Bytes::new())
                } else {
                    head.headers.insert(
                        header::ETAG,
                        HeaderValue::from_str(&etag)
                            .unwrap_or_else(|_| HeaderValue::from_static("\"invalid\"")),
                    );
                    BoxBody::new(bytes)
                }
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_is_a_stable_quoted_tag() {
        let first = etag_for_bytes(b"hello");
        let second = etag_for_bytes(b"hello");
        assert_eq!(first, second);
        assert!(first.starts_with('"') && first.ends_with('"'));
        // `"` + 32 hex chars + `"`.
        assert_eq!(first.len(), 34);
        assert_ne!(first, etag_for_bytes(b"hello!"));
        assert_ne!(first, etag_for_bytes(b""));
    }

    #[test]
    fn absent_precondition_never_matches() {
        assert!(!if_none_match_matches(None, "\"abc\""));
    }

    #[test]
    fn star_matches_anything() {
        let star = HeaderValue::from_static("*");
        assert!(if_none_match_matches(Some(&star), "\"abc\""));
    }

    #[test]
    fn exact_and_listed_tags_match() {
        let exact = HeaderValue::from_static("\"abc\"");
        assert!(if_none_match_matches(Some(&exact), "\"abc\""));
        assert!(!if_none_match_matches(Some(&exact), "\"abd\""));

        let list = HeaderValue::from_static("\"zzz\", \"abc\", \"mmm\"");
        assert!(if_none_match_matches(Some(&list), "\"abc\""));
        assert!(!if_none_match_matches(Some(&list), "\"abd\""));
    }

    #[test]
    fn weak_comparison_ignores_the_w_prefix_on_either_side() {
        // A recipient MUST use the weak comparison for If-None-Match
        // (RFC 9110 §13.1.2): `W/"abc"` validates against `"abc"`.
        let weak = HeaderValue::from_static("W/\"abc\"");
        assert!(if_none_match_matches(Some(&weak), "\"abc\""));
        let strong = HeaderValue::from_static("\"abc\"");
        assert!(if_none_match_matches(Some(&strong), "W/\"abc\""));
    }

    #[test]
    fn garbage_never_matches() {
        for raw in ["", "not-a-tag", "\"unclosed", "W/", "*,\"abc"] {
            let value = HeaderValue::from_str(raw).unwrap();
            if raw == "*,\"abc" {
                // `*` is only a match as the whole field value.
                assert!(!if_none_match_matches(Some(&value), "\"abc\""), "{raw}");
            } else {
                assert!(!if_none_match_matches(Some(&value), "\"abc\""), "{raw}");
            }
        }
    }

    #[test]
    fn non_ascii_precondition_never_matches() {
        let value = HeaderValue::from_bytes(b"\"\xff\"").unwrap();
        assert!(!if_none_match_matches(Some(&value), "\"abc\""));
    }
}
