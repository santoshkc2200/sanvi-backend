//! HTTP client for a product service: `{base_url}/v1/{path}` with the
//! platform's service key and the per-request identity token.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Method, Url};

use crate::domain::forwarding::returns_response_header;
use crate::domain::{
    ForwardRequest, ForwardResponse, Product, ProductAccessError, ProductUpstream,
    forwarding::MAX_BODY_BYTES,
};

/// Header carrying the delegation token.
pub const IDENTITY_HEADER: &str = "X-Identity-Token";
/// Upper bound on one forwarded request. Mutations are never retried.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Largest upstream response body passed back to the browser.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub struct HttpProductUpstream {
    product: &'static Product,
    client: Client,
    base_url: Option<Url>,
    service_key: String,
}

impl HttpProductUpstream {
    /// An empty `base_url` leaves the product unconfigured: every request
    /// then fails closed with 503.
    pub fn new(
        product: &'static Product,
        base_url: &str,
        service_key: &str,
    ) -> anyhow::Result<Self> {
        let base_url = if base_url.trim().is_empty() {
            None
        } else {
            let url = Url::parse(base_url.trim_end_matches('/'))?;
            if !matches!(url.scheme(), "http" | "https") {
                anyhow::bail!("{} base_url must be http(s)", product.key);
            }
            Some(url)
        };
        let client = Client::builder()
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            product,
            client,
            base_url,
            service_key: service_key.to_owned(),
        })
    }

    fn url(&self, request: &ForwardRequest) -> Result<Url, ProductAccessError> {
        let base = self
            .base_url
            .as_ref()
            .filter(|_| !self.service_key.is_empty());
        let base = base.ok_or_else(|| {
            ProductAccessError::Unavailable(format!("{} is not configured", self.product.key))
        })?;
        // The path is validated to plain segments, so appending cannot
        // escape `/v1/`.
        let mut url = base.clone();
        url.set_path(&format!(
            "{}/v1/{}",
            base.path().trim_end_matches('/'),
            request.path.as_str()
        ));
        url.set_query(request.query.as_deref());
        Ok(url)
    }
}

#[async_trait]
impl ProductUpstream for HttpProductUpstream {
    fn product(&self) -> &'static Product {
        self.product
    }

    async fn forward(
        &self,
        request: &ForwardRequest,
        identity_token: &str,
    ) -> Result<ForwardResponse, ProductAccessError> {
        let url = self.url(request)?;
        let method = Method::from_bytes(request.method.as_bytes())
            .map_err(|_| ProductAccessError::BadRequest("unsupported method".to_owned()))?;
        let mut builder = self
            .client
            .request(method, url)
            .bearer_auth(&self.service_key)
            .header(IDENTITY_HEADER, identity_token);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(request_id) = &request.request_id {
            builder = builder.header("X-Request-Id", request_id);
        }
        if !request.body.is_empty() {
            debug_assert!(request.body.len() <= MAX_BODY_BYTES);
            builder = builder.body(request.body.clone());
        }

        let response = builder.send().await.map_err(|err| {
            tracing::warn!(product = self.product.key, error = %err, "product service unreachable");
            ProductAccessError::Unavailable(format!("{} unreachable", self.product.key))
        })?;
        let status = response.status().as_u16();
        // The service rejecting the platform's own credentials is a
        // deployment fault; a browser must never see it as its own 401.
        if status == 401 {
            tracing::error!(
                product = self.product.key,
                "product service rejected the platform's credentials"
            );
            return Err(ProductAccessError::BadGateway(format!(
                "{} rejected the platform's credentials",
                self.product.key
            )));
        }
        let headers = response
            .headers()
            .iter()
            .filter(|(name, _)| returns_response_header(name.as_str()))
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ProductAccessError::BadGateway(format!(
                "{} response too large",
                self.product.key
            )));
        }
        let body = response.bytes().await.map_err(|err| {
            tracing::warn!(product = self.product.key, error = %err, "product service response failed");
            ProductAccessError::Unavailable(format!("{} response failed", self.product.key))
        })?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(ProductAccessError::BadGateway(format!(
                "{} response too large",
                self.product.key
            )));
        }
        Ok(ForwardResponse {
            status,
            headers,
            body: body.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ECOMMERCE, ProductPath};

    fn request(path: &str, query: Option<&str>) -> ForwardRequest {
        ForwardRequest::new(
            "GET",
            ProductPath::parse(path).unwrap(),
            query,
            [],
            Vec::new(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn builds_urls_under_v1_keeping_a_base_path() {
        let upstream =
            HttpProductUpstream::new(&ECOMMERCE, "http://ecommerce:8080/internal/", "key").unwrap();
        let url = upstream
            .url(&request("stores/1/products", Some("limit=2")))
            .unwrap();
        assert_eq!(
            url.as_str(),
            "http://ecommerce:8080/internal/v1/stores/1/products?limit=2"
        );
    }

    #[test]
    fn unconfigured_products_fail_closed() {
        for (base, key) in [("", "key"), ("http://ecommerce:8080", "")] {
            let upstream = HttpProductUpstream::new(&ECOMMERCE, base, key).unwrap();
            assert!(matches!(
                upstream.url(&request("stores", None)),
                Err(ProductAccessError::Unavailable(_))
            ));
        }
        assert!(HttpProductUpstream::new(&ECOMMERCE, "ftp://x", "key").is_err());
    }
}
