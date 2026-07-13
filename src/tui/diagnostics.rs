//! Session-wide warning/error history for the interactive TUI.
//!
//! This is deliberately separate from participant logs: diagnostics describe
//! CLI/session problems, while logs belong to one runtime. Routine `Info`
//! events are already represented by startup phases and are not retained.

use std::collections::VecDeque;
use std::time::Duration;

use crate::session::event::{DiagnosticLevel, DiagnosticSource};

const DIAGNOSTICS_CAPACITY: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiagnosticEntry {
    pub elapsed: Duration,
    pub source: DiagnosticSource,
    pub level: DiagnosticLevel,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DiagnosticsStore {
    entries: VecDeque<DiagnosticEntry>,
    warning_count: usize,
    error_count: usize,
    dropped_count: usize,
}

impl DiagnosticsStore {
    pub(crate) fn record(
        &mut self,
        elapsed: Duration,
        source: DiagnosticSource,
        level: DiagnosticLevel,
        message: String,
    ) {
        if level == DiagnosticLevel::Info {
            return;
        }
        if self.entries.len() == DIAGNOSTICS_CAPACITY {
            self.entries.pop_front();
            self.dropped_count += 1;
        }
        match level {
            DiagnosticLevel::Warn => self.warning_count += 1,
            DiagnosticLevel::Error => self.error_count += 1,
            DiagnosticLevel::Info => unreachable!("info diagnostics are not retained"),
        }
        self.entries.push_back(DiagnosticEntry {
            elapsed,
            source,
            level,
            message,
        });
    }

    #[must_use]
    pub(crate) fn entries(&self) -> &VecDeque<DiagnosticEntry> {
        &self.entries
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub(crate) fn warning_count(&self) -> usize {
        self.warning_count
    }

    #[must_use]
    pub(crate) fn error_count(&self) -> usize {
        self.error_count
    }

    #[must_use]
    pub(crate) fn dropped_count(&self) -> usize {
        self.dropped_count
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DiagnosticsFilter {
    #[default]
    All,
    Warnings,
    Errors,
}

impl DiagnosticsFilter {
    #[must_use]
    pub(crate) const fn cycle(self) -> Self {
        match self {
            Self::All => Self::Warnings,
            Self::Warnings => Self::Errors,
            Self::Errors => Self::All,
        }
    }

    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Warnings => "warnings",
            Self::Errors => "errors",
        }
    }

    #[must_use]
    pub(crate) fn matches(self, level: DiagnosticLevel) -> bool {
        match self {
            Self::All => level != DiagnosticLevel::Info,
            Self::Warnings => level == DiagnosticLevel::Warn,
            Self::Errors => level == DiagnosticLevel::Error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_info_and_counts_retained_severities() {
        let mut store = DiagnosticsStore::default();
        store.record(
            Duration::ZERO,
            DiagnosticSource::Cli,
            DiagnosticLevel::Info,
            "staged world".to_string(),
        );
        store.record(
            Duration::from_secs(1),
            DiagnosticSource::Cli,
            DiagnosticLevel::Warn,
            "retrying".to_string(),
        );
        store.record(
            Duration::from_secs(2),
            DiagnosticSource::Dependency,
            DiagnosticLevel::Error,
            "failed".to_string(),
        );

        assert_eq!(store.len(), 2);
        assert_eq!(store.warning_count(), 1);
        assert_eq!(store.error_count(), 1);
    }

    #[test]
    fn ring_is_bounded_and_reports_dropped_entries() {
        let mut store = DiagnosticsStore::default();
        for index in 0..(DIAGNOSTICS_CAPACITY + 3) {
            store.record(
                Duration::from_secs(index as u64),
                DiagnosticSource::Cli,
                DiagnosticLevel::Warn,
                format!("warning {index}"),
            );
        }
        assert_eq!(store.len(), DIAGNOSTICS_CAPACITY);
        assert_eq!(store.dropped_count(), 3);
        assert_eq!(store.entries().front().unwrap().message, "warning 3");
    }

    #[test]
    fn severity_filter_cycles_and_matches() {
        assert_eq!(DiagnosticsFilter::All.cycle(), DiagnosticsFilter::Warnings);
        assert_eq!(
            DiagnosticsFilter::Warnings.cycle(),
            DiagnosticsFilter::Errors
        );
        assert!(DiagnosticsFilter::Warnings.matches(DiagnosticLevel::Warn));
        assert!(!DiagnosticsFilter::Warnings.matches(DiagnosticLevel::Error));
    }
}
