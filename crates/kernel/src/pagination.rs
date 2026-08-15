use serde::{Deserialize, Serialize};

/// Default page size when the caller does not specify one.
pub const DEFAULT_PER_PAGE: u32 = 20;
/// Hard ceiling on page size; larger requests are clamped, not rejected.
pub const MAX_PER_PAGE: u32 = 100;

/// Normalised pagination request (1-based page numbers).
///
/// Values are clamped at construction so an API can never force the
/// database into pathological offsets or page sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRequest {
    page: u32,
    per_page: u32,
}

impl PageRequest {
    pub fn new(page: u32, per_page: u32) -> Self {
        Self {
            page: page.max(1),
            per_page: per_page.clamp(1, MAX_PER_PAGE),
        }
    }

    /// 1-based page number, always >= 1.
    pub fn page(&self) -> u32 {
        self.page
    }

    pub fn per_page(&self) -> u32 {
        self.per_page
    }

    /// Zero-based row offset.
    pub fn offset(&self) -> u64 {
        u64::from(self.page - 1) * u64::from(self.per_page)
    }
}

impl Default for PageRequest {
    fn default() -> Self {
        Self::new(1, DEFAULT_PER_PAGE)
    }
}

/// One page of results plus enough metadata to render pagination controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub per_page: u32,
    pub total: u64,
}

impl<T> Page<T> {
    pub fn has_more(&self) -> bool {
        u64::from(self.page) * u64::from(self.per_page) < self.total
    }

    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Page<U> {
        Page {
            items: self.items.into_iter().map(f).collect(),
            page: self.page,
            per_page: self.per_page,
            total: self.total,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn construction_never_yields_zero_or_oversized_pages(page: u32, per_page: u32) {
            let req = PageRequest::new(page, per_page);
            prop_assert!(req.page() >= 1);
            prop_assert!((1..=MAX_PER_PAGE).contains(&req.per_page()));
            prop_assert_eq!(req.offset(), u64::from(req.page() - 1) * u64::from(req.per_page()));
        }
    }

    #[test]
    fn clamps_oversized_page() {
        assert_eq!(
            PageRequest::new(0, 10_000),
            PageRequest::new(1, MAX_PER_PAGE)
        );
    }

    #[test]
    fn has_more_reflects_remaining_items() {
        let page = Page {
            items: vec![1, 2],
            page: 1,
            per_page: 2,
            total: 3,
        };
        assert!(page.has_more());

        let last = Page {
            items: vec![1],
            page: 2,
            per_page: 2,
            total: 3,
        };
        assert!(!last.has_more());
    }

    #[test]
    fn serde_roundtrip() {
        let req = PageRequest::new(2, 50);
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<PageRequest>(&json).unwrap(), req);
    }
}
