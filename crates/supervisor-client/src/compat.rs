//! Connect-time compatibility, which is now a decode rather than a check.
//!
//! Every version on the connect reply is a serde enum, so a supervisor on a
//! different train produces a reply this client **cannot deserialize**. There
//! is nothing to compare: the decoder is the gate, it cannot be forgotten, and
//! serde's own error already names the version it found and the set it
//! expected.
//!
//! All this module does is turn that decode failure into a diagnostic an
//! operator can act on, keeping serde's naming verbatim rather than restating
//! it - a restatement is exactly where a second source of truth would creep
//! back in.
//!
//! Framework SemVer is provenance, not compatibility: a newer `phoxal` attaches
//! to an older running `phoxald` whenever the reply decodes, because the
//! exact-pair rule governs what is installed on disk, not what may attach.

use phoxal_bus::QueryError;

use crate::error::AttachError;

/// Classify a failure from the connect query.
///
/// A decode failure means the two ends disagree about a version; anything else
/// is an ordinary transport or availability failure and is passed through
/// unchanged.
#[must_use]
pub fn classify_connect_failure(endpoint: &str, error: QueryError) -> AttachError {
    match error {
        QueryError::Decode(detail) => AttachError::Incompatible {
            endpoint: endpoint.to_string(),
            detail,
        },
        other => AttachError::Query(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_supervisor_api::{SnapshotSchema, SupervisorSchemas, supervisor};

    const ENDPOINT: &str = "unix/.phoxal/run/supervisor.sock";

    /// The end-to-end shape of a mismatch: a supervisor whose descriptor names
    /// a version this client does not know fails to decode, and the resulting
    /// diagnostic carries serde's naming of both sides plus the fix.
    #[test]
    fn a_foreign_version_becomes_an_actionable_diagnostic() {
        let mut reply = serde_json::to_value(supervisor::connect::Reply::V0 {
            robot: phoxal_supervisor_api::RobotIdentity {
                id: phoxal_supervisor_api::Name::new("rover"),
                namespace: phoxal_supervisor_api::Name::new("demo"),
            },
            api: phoxal_supervisor_api::RobotApi::CURRENT,
            schemas: SupervisorSchemas::current(),
            mode: phoxal_supervisor_api::ExecutionMode::Real,
        })
        .unwrap();
        reply["schemas"]["snapshot"] = serde_json::json!("phoxal/supervisor-snapshot/v1");

        let decode_error = serde_json::from_value::<supervisor::connect::Reply>(reply)
            .expect_err("a foreign snapshot version must not decode");

        let error =
            classify_connect_failure(ENDPOINT, QueryError::Decode(decode_error.to_string()));
        let rendered = error.to_string();

        assert!(
            matches!(error, AttachError::Incompatible { .. }),
            "{rendered}"
        );
        // Serde named the foreign version and the expected one; the diagnostic
        // keeps that naming rather than restating it.
        assert!(
            rendered.contains("phoxal/supervisor-snapshot/v1"),
            "{rendered}"
        );
        assert!(
            rendered.contains(SnapshotSchema::CURRENT.as_str()),
            "{rendered}"
        );
        assert!(rendered.contains(ENDPOINT), "{rendered}");
        assert!(rendered.contains("same train"), "{rendered}");
    }

    /// A matching supervisor's reply simply decodes - there is no separate
    /// "passes validation" state to be in.
    #[test]
    fn a_matching_supervisor_just_decodes() {
        let reply = supervisor::connect::Reply::V0 {
            robot: phoxal_supervisor_api::RobotIdentity {
                id: phoxal_supervisor_api::Name::new("rover"),
                namespace: phoxal_supervisor_api::Name::new("demo"),
            },
            api: phoxal_supervisor_api::RobotApi::CURRENT,
            schemas: SupervisorSchemas::current(),
            mode: phoxal_supervisor_api::ExecutionMode::Real,
        };
        let bytes = serde_json::to_vec(&reply).unwrap();
        assert_eq!(
            serde_json::from_slice::<supervisor::connect::Reply>(&bytes).unwrap(),
            reply
        );
    }

    /// An unreachable supervisor is not an incompatible one, and must not be
    /// reported as one.
    #[test]
    fn an_unavailable_supervisor_is_not_reported_as_incompatible() {
        let error = classify_connect_failure(ENDPOINT, QueryError::Unavailable);
        assert!(matches!(error, AttachError::Query(QueryError::Unavailable)));
    }
}
