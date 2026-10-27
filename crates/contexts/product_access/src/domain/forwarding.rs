//! What crosses the proxy. Both directions use allow-lists: browser
//! credentials (cookies, `Authorization`) and tenant hints never reach a
//! product service, and service internals never reach the browser.

use super::error::ProductAccessError;

/// Largest request body forwarded to a product service.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Longest forwarded path, in bytes.
pub const MAX_PATH_BYTES: usize = 512;

/// Request headers a product service may see from the browser.
const REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "content-type",
    "idempotency-key",
];

/// Response headers the browser may see from a product service.
const RESPONSE_HEADERS: &[&str] = &["content-type", "idempotency-replayed"];

/// Whether a browser request header is forwarded.
pub fn forwards_request_header(name: &str) -> bool {
    REQUEST_HEADERS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// Whether an upstream response header is returned to the browser.
pub fn returns_response_header(name: &str) -> bool {
    RESPONSE_HEADERS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// A path below the product's `/v1/`. Segments are plain URL tokens: no
/// empty, `.` or `..` segments and no percent-encoding, so the forwarded URL
/// cannot be steered to another upstream route than the one requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductPath(String);

impl ProductPath {
    /// Validates the tail after the proxy prefix, e.g. `stores/42/products`.
    pub fn parse(tail: &str) -> Result<Self, ProductAccessError> {
        let valid_segment = |segment: &str| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
        };
        if tail.len() > MAX_PATH_BYTES || !tail.split('/').all(valid_segment) {
            return Err(ProductAccessError::BadRequest(
                "not a valid product path".to_owned(),
            ));
        }
        Ok(Self(tail.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A browser request after the allow-list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRequest {
    pub method: String,
    pub path: ProductPath,
    pub query: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Correlates logs across the platform and the product service.
    pub request_id: Option<String>,
}

impl ForwardRequest {
    /// Keeps only forwardable headers; refuses oversized bodies.
    pub fn new(
        method: &str,
        path: ProductPath,
        query: Option<&str>,
        headers: impl IntoIterator<Item = (String, String)>,
        body: Vec<u8>,
        request_id: Option<String>,
    ) -> Result<Self, ProductAccessError> {
        if body.len() > MAX_BODY_BYTES {
            return Err(ProductAccessError::BadRequest(format!(
                "request body exceeds {MAX_BODY_BYTES} bytes"
            )));
        }
        Ok(Self {
            method: method.to_owned(),
            path,
            query: query.filter(|q| !q.is_empty()).map(str::to_owned),
            headers: headers
                .into_iter()
                .filter(|(name, _)| forwards_request_header(name))
                .collect(),
            body,
            request_id,
        })
    }
}

/// A product service's answer after the allow-list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_paths_are_forwarded() {
        for ok in ["stores", "stores/42/products", "a-b_c.d~e"] {
            assert!(ProductPath::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/stores",
            "stores/",
            "stores//products",
            "../health",
            "stores/../../admin",
            "./stores",
            "stores/%2e%2e",
            "stores?x=1",
            "stores#x",
            "stores/ä",
            &"a".repeat(MAX_PATH_BYTES + 1),
        ] {
            assert!(ProductPath::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn credentials_and_tenant_hints_are_never_forwarded() {
        let request = ForwardRequest::new(
            "POST",
            ProductPath::parse("stores").unwrap(),
            Some("limit=2"),
            [
                ("Content-Type", "application/json"),
                ("Idempotency-Key", "k1"),
                ("Cookie", "ory_session=secret"),
                ("Authorization", "Bearer user-token"),
                ("X-Tenant-Id", "999"),
                ("X-Identity-Token", "forged"),
                ("X-Forwarded-For", "1.2.3.4"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned())),
            b"{}".to_vec(),
            Some("req-1".to_owned()),
        )
        .unwrap();
        let names: Vec<_> = request.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["Content-Type", "Idempotency-Key"]);
        assert_eq!(request.query.as_deref(), Some("limit=2"));
    }

    #[test]
    fn oversized_bodies_are_refused() {
        let error = ForwardRequest::new(
            "POST",
            ProductPath::parse("stores").unwrap(),
            None,
            [],
            vec![0; MAX_BODY_BYTES + 1],
            None,
        );
        assert!(matches!(error, Err(ProductAccessError::BadRequest(_))));
    }

    #[test]
    fn only_safe_response_headers_return() {
        assert!(returns_response_header("Content-Type"));
        assert!(returns_response_header("idempotency-replayed"));
        assert!(!returns_response_header("set-cookie"));
        assert!(!returns_response_header("server"));
    }
}
