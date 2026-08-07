//! Bounded text carried by supervisor documents.
//!
//! Every free-text field on this contract is a rendered diagnostic, so it has
//! no closed set to be typed against but must still be bounded: an unbounded
//! `String` on a document the daemon publishes at will is a remote memory
//! amplifier for every attached client.
//!
//! The bound is part of the type, not a check a caller may forget.
//! Construction truncates on a character boundary and marks the cut;
//! deserialization *rejects* an oversized value rather than truncating it,
//! because silently shortening a peer's document would make two clients
//! disagree about the same revision.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Marker appended when [`Bounded::new`] had to cut a value short.
const TRUNCATION_MARK: char = '…';

/// A UTF-8 string of at most `MAX` bytes.
#[derive(Clone, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Bounded<const MAX: usize>(String);

impl<const MAX: usize> Bounded<MAX> {
    /// The byte bound this type enforces.
    pub const MAX_BYTES: usize = MAX;

    /// Build a bounded value, truncating on a character boundary and marking
    /// the cut when the input does not fit.
    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Self {
        let value = value.as_ref();
        if value.len() <= MAX {
            return Self(value.to_string());
        }
        let mut end = MAX.saturating_sub(TRUNCATION_MARK.len_utf8());
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        let mut bounded = value[..end].to_string();
        if MAX >= TRUNCATION_MARK.len_utf8() {
            bounded.push(TRUNCATION_MARK);
        }
        Self(bounded)
    }

    /// Build a bounded value, rejecting rather than truncating an oversized
    /// input. This is the path a peer's document takes.
    ///
    /// # Errors
    ///
    /// Returns [`TextTooLong`] when `value` exceeds `MAX` bytes.
    pub fn try_new(value: impl Into<String>) -> Result<Self, TextTooLong> {
        let value = value.into();
        if value.len() > MAX {
            return Err(TextTooLong {
                bytes: value.len(),
                limit: MAX,
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A value that does not fit the bound its field declares.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("bounded text is {bytes} bytes; the limit is {limit}")]
pub struct TextTooLong {
    pub bytes: usize,
    pub limit: usize,
}

impl<const MAX: usize> fmt::Debug for Bounded<MAX> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, formatter)
    }
}

impl<const MAX: usize> fmt::Display for Bounded<MAX> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<const MAX: usize> Serialize for Bounded<MAX> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for Bounded<MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::try_new(value).map_err(serde::de::Error::custom)
    }
}

/// One key segment sized identity: a participant id, a component instance, a
/// component type, a robot id.
pub type Name = Bounded<256>;

/// A rendered failure explanation - an error chain, a validation message.
pub type Detail = Bounded<4096>;

/// The retained tail of a failed process's standard error.
pub type StderrTail = Bounded<{ 32 * 1024 }>;

/// One structured log message or target.
pub type LogText = Bounded<4096>;

/// A bundle-relative path handed to `supervisor/bundle/get`.
pub type BundlePath = Bounded<1024>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_truncates_on_a_character_boundary_and_marks_the_cut() {
        let bounded = Bounded::<8>::new("ααααα");
        assert!(bounded.as_str().len() <= 8);
        assert!(bounded.as_str().ends_with(TRUNCATION_MARK));
        assert!(bounded.as_str().starts_with('α'));
    }

    #[test]
    fn a_peers_oversized_value_is_rejected_rather_than_silently_shortened() {
        let json = serde_json::to_string(&"x".repeat(300)).unwrap();
        let error = serde_json::from_str::<Name>(&json)
            .expect_err("an oversized name must not decode into a shortened one");
        assert!(error.to_string().contains("bounded text is 300 bytes"));

        let fits = serde_json::from_str::<Name>(&serde_json::to_string("brain").unwrap()).unwrap();
        assert_eq!(fits.as_str(), "brain");
    }
}
