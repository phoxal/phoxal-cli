//! Project preparation phase identity.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PhaseId(Box<str>);

impl PhaseId {
    #[must_use]
    pub fn new(id: impl Into<Box<str>>) -> Self {
        Self(id.into())
    }
}

impl<T> From<T> for PhaseId
where
    T: Into<Box<str>>,
{
    fn from(id: T) -> Self {
        Self::new(id)
    }
}

impl std::fmt::Display for PhaseId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseOutcome {
    Succeeded,
    Failed { error: String },
}

#[cfg(test)]
mod tests {
    use super::PhaseId;
    use std::collections::HashSet;

    #[test]
    fn phase_id_equality_and_hashing() {
        let left = PhaseId::new("prepare");
        let right = PhaseId::from("prepare");
        assert_eq!(left, right);
        assert_eq!(HashSet::from([left]).get(&right), Some(&right));
    }

    #[test]
    fn phase_id_from_str_and_display() {
        assert_eq!(PhaseId::from("stage").to_string(), "stage");
    }
}
