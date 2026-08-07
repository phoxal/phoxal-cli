use std::fmt;

/// An exit whose user-facing context has already been rendered on stderr.
#[derive(Debug)]
pub struct ReportedExit(pub u8);

impl fmt::Display for ReportedExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "command exited with status {}", self.0)
    }
}

impl std::error::Error for ReportedExit {}
