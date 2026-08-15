use std::{fmt, hash::Hash, marker::PhantomData, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Strongly-typed identifier.
///
/// Wraps a UUIDv7 so identifiers are globally unique *and* roughly sortable
/// by creation time. The `Tag` parameter makes ids of different kinds
/// (tenant vs. user vs. event) distinct types at compile time.
///
/// ```ignore
/// pub enum TenantTag {}
/// pub type TenantId = Id<TenantTag>;
/// ```
pub struct Id<Tag> {
    raw: Uuid,
    _tag: PhantomData<fn() -> Tag>,
}

impl<Tag> Id<Tag> {
    /// Generate a new id (UUIDv7).
    pub fn new() -> Self {
        Self {
            raw: Uuid::now_v7(),
            _tag: PhantomData,
        }
    }

    /// Wrap an existing UUID without interpretation.
    pub fn from_uuid(raw: Uuid) -> Self {
        Self {
            raw,
            _tag: PhantomData,
        }
    }

    pub fn as_uuid(&self) -> Uuid {
        self.raw
    }

    pub fn is_nil(&self) -> bool {
        self.raw.is_nil()
    }
}

impl<Tag> Default for Id<Tag> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Tag> FromStr for Id<Tag> {
    type Err = crate::error::KernelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw = Uuid::parse_str(s).map_err(|_| crate::error::KernelError::InvalidId {
            value: s.to_string(),
        })?;
        Ok(Self::from_uuid(raw))
    }
}

impl<Tag> fmt::Display for Id<Tag> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.raw.fmt(f)
    }
}

impl<Tag> fmt::Debug for Id<Tag> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({self})")
    }
}

impl<Tag> Clone for Id<Tag> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Tag> Copy for Id<Tag> {}

impl<Tag> PartialEq for Id<Tag> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<Tag> Eq for Id<Tag> {}

impl<Tag> PartialOrd for Id<Tag> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<Tag> Ord for Id<Tag> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<Tag> Hash for Id<Tag> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<Tag> Serialize for Id<Tag> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.raw.serialize(serializer)
    }
}

impl<'de, Tag> Deserialize<'de> for Id<Tag> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Uuid::deserialize(deserializer)?;
        Ok(Self::from_uuid(raw))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    enum Tag {}
    type TestId = Id<Tag>;

    #[test]
    fn new_ids_are_unique_and_parse_roundtrips() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(TestId::new()));
        }

        let id = TestId::new();
        let s = id.to_string();
        assert_eq!(s.parse::<TestId>().unwrap(), id);
    }

    #[test]
    fn ids_are_uuid_v7() {
        let id = TestId::new();
        assert_eq!(id.as_uuid().get_version_num(), 7);
        assert!(!id.is_nil());
    }

    #[test]
    fn invalid_id_is_rejected() {
        assert!(matches!(
            "not-a-uuid".parse::<TestId>(),
            Err(crate::error::KernelError::InvalidId { .. })
        ));
    }

    #[test]
    fn serde_roundtrip_is_transparent() {
        let id = TestId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        let back: TestId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn ids_generated_later_sort_after_earlier_ones() {
        let a = TestId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = TestId::new();
        assert!(a < b);
    }
}
