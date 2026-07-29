//! Identifies the live runtime mode without carrying presentation policy.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    Run,
    Simulation,
}

impl SessionMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Simulation => "simulation",
        }
    }
}

impl std::fmt::Display for SessionMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}
