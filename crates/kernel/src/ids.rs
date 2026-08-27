use std::{fmt, hash::Hash, marker::PhantomData, num::NonZeroI64, str::FromStr};

use id_forge::snowflake::{ClockSkew, Snowflake};
use serde::{
    Deserialize, Serialize,
    de::{self, Visitor},
};

use crate::error::{IdGenerationError, KernelError};

pub const SNOWFLAKE_EPOCH_MS: u64 = id_forge::snowflake::DEFAULT_EPOCH_MS;

/// Valid, positive, signed-64-bit Snowflake storage value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnowflakeId(NonZeroI64);

impl SnowflakeId {
    pub const fn as_i64(self) -> i64 {
        self.0.get()
    }

    pub fn node_id(self) -> u16 {
        Snowflake::parts(self.as_i64() as u64).1
    }
}

impl TryFrom<i64> for SnowflakeId {
    type Error = KernelError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        NonZeroI64::new(value)
            .filter(|value| value.get().is_positive())
            .map(Self)
            .ok_or_else(|| KernelError::InvalidId {
                value: value.to_string(),
            })
    }
}

impl fmt::Display for SnowflakeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_i64().fmt(f)
    }
}

impl FromStr for SnowflakeId {
    type Err = KernelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.starts_with(['+', '-'])
            || value.starts_with('0')
            || value.bytes().any(|byte| !byte.is_ascii_digit())
        {
            return Err(KernelError::InvalidId {
                value: value.to_string(),
            });
        }

        value
            .parse::<i64>()
            .map_err(|_| KernelError::InvalidId {
                value: value.to_string(),
            })
            .and_then(Self::try_from)
    }
}

impl Serialize for SnowflakeId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SnowflakeId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SnowflakeVisitor;

        impl Visitor<'_> for SnowflakeVisitor {
            type Value = SnowflakeId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical positive decimal Snowflake ID string")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(SnowflakeVisitor)
    }
}

/// Configured Snowflake node. Zero is reserved for stable seed IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeId(u16);

impl NodeId {
    pub fn new(value: u16) -> Result<Self, IdGenerationError> {
        if (1..=1023).contains(&value) {
            Ok(Self(value))
        } else {
            Err(IdGenerationError::InvalidNode(value))
        }
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Generates raw first-party IDs. Composition roots own its implementation.
pub trait RawIdGenerator: Send + Sync {
    fn next_id(&self) -> Result<SnowflakeId, IdGenerationError>;
}

/// `id-forge` backed production generator using Sanvi's fixed epoch.
#[derive(Debug)]
pub struct SnowflakeGenerator {
    inner: Snowflake,
}

impl SnowflakeGenerator {
    pub fn new(node_id: NodeId) -> Self {
        Self {
            inner: Snowflake::new(node_id.get()),
        }
    }
}

impl RawIdGenerator for SnowflakeGenerator {
    fn next_id(&self) -> Result<SnowflakeId, IdGenerationError> {
        let value = self
            .inner
            .try_next_id()
            .map_err(
                |ClockSkew { last_ms, now_ms }| IdGenerationError::ClockSkew { last_ms, now_ms },
            )?;
        let value = i64::try_from(value).map_err(|_| IdGenerationError::OutOfRange)?;

        SnowflakeId::try_from(value).map_err(|_| IdGenerationError::OutOfRange)
    }
}

/// Strongly typed first-party identifier.
///
/// `Tag` makes IDs of different kinds distinct types at compile time while
/// keeping their storage and wire representation uniform.
pub struct Id<Tag> {
    raw: SnowflakeId,
    _tag: PhantomData<fn() -> Tag>,
}

impl<Tag> Id<Tag> {
    pub const fn from_snowflake(raw: SnowflakeId) -> Self {
        Self {
            raw,
            _tag: PhantomData,
        }
    }

    pub const fn as_snowflake(self) -> SnowflakeId {
        self.raw
    }

    pub const fn as_i64(self) -> i64 {
        self.raw.as_i64()
    }
}

impl<Tag> TryFrom<i64> for Id<Tag> {
    type Error = KernelError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        SnowflakeId::try_from(value).map(Self::from_snowflake)
    }
}

impl<Tag> FromStr for Id<Tag> {
    type Err = KernelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<SnowflakeId>().map(Self::from_snowflake)
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
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de, Tag> Deserialize<'de> for Id<Tag> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor<Tag>(PhantomData<fn() -> Tag>);

        impl<'de, Tag> Visitor<'de> for IdVisitor<Tag> {
            type Value = Id<Tag>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical positive decimal Snowflake ID string")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(IdVisitor(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use super::*;

    enum Tag {}
    type TestId = Id<Tag>;

    #[test]
    fn snowflake_value_rejects_non_positive_values() {
        assert!(SnowflakeId::try_from(0_i64).is_err());
        assert!(SnowflakeId::try_from(-1_i64).is_err());
    }

    #[test]
    fn tagged_id_serde_is_a_decimal_string_only() {
        let id = TestId::try_from(873_698_342_314_721_281_i64).unwrap();

        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"873698342314721281\""
        );
        assert_eq!(
            serde_json::from_str::<TestId>("\"873698342314721281\"").unwrap(),
            id
        );
        assert!(serde_json::from_str::<TestId>("873698342314721281").is_err());
        assert!(serde_json::from_str::<TestId>("\"01\"").is_err());
    }

    #[test]
    fn generator_uses_requested_node_and_is_unique_under_concurrency() {
        let generator = Arc::new(SnowflakeGenerator::new(NodeId::new(17).unwrap()));
        let ids = (0..8)
            .map(|_| {
                let generator = Arc::clone(&generator);
                std::thread::spawn(move || {
                    (0..2_000)
                        .map(|_| generator.next_id().unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .flat_map(|thread| thread.join().unwrap())
            .collect::<HashSet<_>>();

        assert_eq!(ids.len(), 16_000);
        assert!(ids.iter().all(|id| id.node_id() == 17));
    }
}
