//! Source-built binary staging for runtime assembly.

use std::collections::BTreeSet;
use std::path::Path;

use crate::check::source::{SourceParticipant, SourceParticipantKind};
use crate::source::resolver::official_binary_name;
use anyhow::Result;
use phoxal_cli_catalog::ArtifactKind;

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
        let binary_name = match participant.kind {
            SourceParticipantKind::Brain | SourceParticipantKind::UserService => {
                participant.name.clone()
            }
            SourceParticipantKind::ComponentDriver => official_binary_name(
                ArtifactKind::ComponentDriver,
                &participant.expected_artifact_id,
            ),
            SourceParticipantKind::OfficialService | SourceParticipantKind::Simulator => continue,
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
