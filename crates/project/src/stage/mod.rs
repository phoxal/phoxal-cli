//! The unified runtime-layout stager.
//!
//! One stager materializes a source project into the finalized bundle at
//! `.phoxal/bundle/`:
//!
//! ```text
//! robot.yaml  # the one persisted robot definition
//! assets/     # frozen robot and component definitions and their meshes
//! bin/        # flat participant-executable store
//! ```
//!
//! `cargo install --root <candidate>` targets the SAME candidate directory the
//! finalized document and assets are staged into, so its `bin/` entries land
//! directly at their final path with no separate harvest-then-link step. It
//! also leaves `.crates.toml`/`.crates2.json` bookkeeping dotfiles in the
//! candidate root; those are host-specific state, never bundle content, so
//! publication removes them rather than fighting `--no-track` (which would
//! disable Cargo's own concurrent-invocation protection for no real benefit).
//!
//! The live `.phoxal/bundle/` is never deleted before every install and
//! validation succeeds: staging always builds into a sibling candidate
//! directory first, validates it, and only then atomically renames it over
//! the previous complete layout. A build failure halfway through must never
//! leave a robot with no runtime.
//!
//! This is why [`begin_runtime_layout`]/[`publish_runtime_layout`] are two
//! functions, not one: everything between them - `cargo install` for every
//! official, building every source/override binary, the source check, and
//! the loader's own execution-time validation - runs against
//! [`candidate::StagedCandidate::path`], a path nobody executes from yet. Only
//! [`publish_runtime_layout`] ever touches the live path, and it is always
//! the last call - see `run::prepare::refresh_staging` and
//! `simulation::prepare_simulation`.

mod candidate;
mod finalize;
mod participants;
mod publish;

pub(crate) use candidate::{begin_runtime_layout, copy_tree_into};
pub(crate) use participants::{
    MaterializeSettings, materialize_candidate_store, stage_named_binary, stage_participant_binary,
};
pub(crate) use publish::publish_runtime_layout;

#[cfg(test)]
#[cfg(test)]
pub(crate) use candidate::{compile_test_bundle, write_test_bundle};

/// The exact embedded-metadata document a role macro writes, for tests that
/// synthesize a participant binary.
#[cfg(test)]
pub(crate) fn test_metadata_payload(
    id: &str,
    kind: &str,
    config_schema: serde_json::Value,
) -> Vec<u8> {
    let current = phoxal_cli_core::check::participant_metadata::CompatibilitySet::current();
    serde_json::to_vec(&serde_json::json!({
        "schema": phoxal_runtime_contract::PARTICIPANT_METADATA_SCHEMA,
        "api": current.api.as_str(),
        "schemas": {
            "bus": current.schemas.bus.as_str(),
            "launch": current.schemas.launch.as_str(),
            "robot": current.schemas.robot.as_str(),
            "component": current.schemas.component.as_str(),
            "simulation": current.schemas.simulation.as_str(),
        },
        "id": id,
        "kind": kind,
        "config_schema": config_schema,
    }))
    .expect("metadata serializes")
}
