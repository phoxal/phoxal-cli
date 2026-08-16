//! Source-built binary staging for runtime assembly.

use std::collections::BTreeSet;
use std::path::Path;

use crate::check::source::{SourceParticipant, SourceParticipantKind};
use anyhow::Result;

use crate::build::cargo::SourceArtifacts;

/// Add every selected source-built artifact to the candidate's flat `bin/`
/// store. Official packages and official source overrides are materialized by
/// the candidate-wide materializer; this pass owns robot source binaries.
pub(crate) fn stage_complete_bin_store(
    staged_root: &Path,
    source_participants: &[SourceParticipant],
    source_artifacts: &SourceArtifacts,
) -> Result<()> {
    let mut staged_names = BTreeSet::new();
    for participant in source_participants {
        // A bundle names a binary by the id it is launched under: the brain is
        // `brain`, a service is its service id, and a driver is its component
        // *type*, because one driver binary serves every instance of a type.
        let binary_name = match participant.kind {
            SourceParticipantKind::Brain | SourceParticipantKind::UserService => {
                phoxal_cli_catalog::bundle_binary_name(&participant.name)
            }
            SourceParticipantKind::ComponentDriver => {
                phoxal_cli_catalog::bundle_binary_name(&participant.expected_artifact_id)
            }
            SourceParticipantKind::OfficialService => continue,
        };
        if staged_names.insert(binary_name.clone()) {
            crate::stage::stage_named_binary(
                staged_root,
                &binary_name,
                source_artifacts.binary(participant)?,
            )?;
        }
    }
    Ok(())
}
