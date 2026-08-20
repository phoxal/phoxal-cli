//! The unified runtime-layout stager.
//!
//! One stager materializes a source project into the deployment release at
//! `.phoxal/release/`: the supervisor and the finalized bundle it runs.
//!
//! ```text
//! phoxal-supervisor  # the framework supervisor this release is run by
//! bundle/manifest.json
//! bundle/assets/     # frozen robot and component definitions and their meshes
//! bundle/bin/        # brain, <service-id>, <component-type>
//! ```
//!
//! Participant `cargo install --root <candidate>` calls target the SAME
//! candidate directory the manifest and assets are staged into, so their
//! `bin/` entries land beside their final path with only a rename left to do -
//! Cargo installs a package under its own name (`phoxal-service-drive`) and the
//! bundle names it by the id it is launched under (`drive`). The supervisor
//! shares the neutral Cargo materialization machinery but uses its own
//! release-profile, release-destination batch and is copied beside `bundle/`
//! only at finalization. Participant installs also leave
//! `.crates.toml`/`.crates2.json` bookkeeping dotfiles in the candidate root;
//! those are host-specific state, never bundle content, so publication removes
//! them rather than fighting `--no-track` (which would disable Cargo's own
//! concurrent-invocation protection for no real benefit).
//!
//! The live `.phoxal/release/` is never deleted before every install and
//! validation succeeds: staging always builds into a sibling candidate
//! directory first, validates it, and only then atomically renames it over
//! the previous complete release. A build failure halfway through must never
//! leave a robot with no runtime.
//!
//! This is why [`begin_runtime_layout`]/[`finalize_release`] are two functions,
//! not one: everything between them - `cargo install` for every official,
//! building every source/override binary, the source check, and the manifest
//! write - runs against [`candidate::StagedCandidate::path`], a path nobody
//! executes from yet. Only [`finalize_release`] ever touches the live path, and
//! it is always the last call - see `run::prepare::refresh_staging` and
//! `simulation::prepare_simulation`. It adds the supervisor before it
//! publishes, so a published release always has both halves and they always
//! match.

mod candidate;
mod manifest;
mod participants;
mod publish;

use std::path::Path;

use anyhow::{Context, Result};

use crate::check::participant_metadata::ExpectedTarget;
use crate::deployment::ReleaseLayout;

pub(crate) use candidate::{StagedCandidate, begin_runtime_layout, copy_tree_into};
pub(crate) use manifest::write_manifest_document;
pub(crate) use participants::{
    MaterializeSettings, materialize_candidate_store, stage_named_binary,
};

/// Complete a validated candidate into a deployment release and publish it.
///
/// The supervisor lands in the candidate first, so the atomic rename that
/// publishes it switches the supervisor and the bundle together.
pub(crate) fn finalize_release(
    candidate: StagedCandidate,
    supervisor_source: &Path,
    expected: &ExpectedTarget,
) -> Result<ReleaseLayout> {
    crate::deployment::materialize(candidate.release_path(), supervisor_source, expected)
        .context("failed to package the supervisor into the staged deployment release")?;
    let root = publish::publish_runtime_layout(candidate)?;
    Ok(ReleaseLayout {
        supervisor: root.join(crate::deployment::SUPERVISOR_FILE),
        bundle: root.join(crate::deployment::BUNDLE_DIR),
        root,
    })
}

#[cfg(test)]
pub(crate) use candidate::compile_test_bundle;

/// The exact embedded-metadata document a role macro writes, for tests that
/// synthesize a participant binary.
///
/// Built through the framework's own serialize twin rather than a JSON literal,
/// so a fixture can never claim a version spelling the parser would reject. The
/// train it declares is the fixture train, which is also the one
/// [`test_project_train`] locks - together they keep a synthesized binary on
/// the line its fixture project targets.
/// `connection` is the one kind `#[phoxal::driver(connection = ...)]` declares,
/// and is `None` for every service and brain - which is what the callers here
/// synthesize.
#[cfg(test)]
pub(crate) fn test_metadata_payload(
    id: &str,
    kind: &str,
    connection: Option<phoxal::model::connection::ConnectionKind>,
    config_schema: serde_json::Value,
) -> Vec<u8> {
    let kind = serde_json::from_value(serde_json::Value::String(kind.to_string()))
        .expect("the fixture names a participant kind this train has");
    serde_json::to_vec(
        &phoxal::participant::metadata::ParticipantMetadataRecord::V0 {
            contract: phoxal::participant::metadata::ParticipantContractRecord {
                framework: crate::check::participant_metadata::FIXTURE_FRAMEWORK,
                id,
                kind,
                connection,
                config_schema,
            },
        },
    )
    .expect("metadata serializes")
}

/// The framework target a fixture project selects: the train the binaries
/// [`test_metadata_payload`] describes were built from.
#[cfg(test)]
pub(crate) fn test_project_train() -> crate::source::train::LockedTrain {
    crate::source::train::LockedTrain::from_locked_version(
        &crate::check::participant_metadata::FIXTURE_FRAMEWORK.to_string(),
    )
    .expect("the fixture framework is a canonical version")
}
