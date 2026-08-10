use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use phoxal_bundle::{
    AssetIndex, BinaryReference, BinarySource, BundlePath, ParticipantClock, Runtime,
    RuntimeBundle, RuntimeDocument, RuntimeParticipant, RuntimeRouterConfig,
};
use phoxal_model::Clock;
use phoxal_runtime_contract::identity::{
    ComponentInstanceId, ParticipantArtifactId, ParticipantId,
};

use crate::source::requirements::{SimulationMembership, derive_runtime_requirements};
use crate::source::resolver::BundlePlan;
use phoxal_cli_catalog::Catalog;

/// Persist the only runtime authority after all selected binaries have landed.
///
/// Source manifests and catalog entries are consumed here and never copied into
/// the bundle. Opening the result proves every indexed asset and executable
/// before the candidate can be published.
pub(crate) fn write_runtime_document(root: &Path, resolved: &BundlePlan) -> Result<RuntimeBundle> {
    // `cargo install --root <candidate>` leaves its bookkeeping beside `bin/`.
    // Those files carry absolute host paths and are not bundle content; the
    // strict bundle layout rightly refuses them, so they go before the
    // candidate becomes a bundle.
    for name in crate::bundle::archive::CARGO_INSTALL_BOOKKEEPING {
        let path = root.join(name);
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error).with_context(|| {
                format!("failed to remove {} from the candidate", path.display())
            });
        }
    }

    let simulated = SimulationMembership::from_bundle_assets(
        &root.join(phoxal_bundle::ASSETS_DIR),
        &resolved.source_manifest.used_component_types(),
    );
    let requirements =
        derive_runtime_requirements(&resolved.source_manifest, &simulated, &Catalog::official())?;

    let clock = match resolved.compiled.robot.clock() {
        Clock::Real => ParticipantClock::Real,
        Clock::Simulated => ParticipantClock::Simulation,
    };
    let mut artifacts = BTreeMap::new();
    let mut participants = Vec::with_capacity(requirements.participants.len());

    for required in requirements.participants {
        let artifact_id = ParticipantArtifactId::new(required.artifact_id.clone())?;
        let participant_id = ParticipantId::new(required.participant_id.clone())?;
        let relative = BundlePath::new(format!(
            "{}/{}",
            phoxal_bundle::BIN_DIR,
            required.binary_name
        ))?;
        let binary_path = root.join(relative.as_str());
        let contract =
            crate::check::participant_metadata::extract_participant_metadata(&binary_path)
                .with_context(|| format!("failed to inspect {}", binary_path.display()))?;
        anyhow::ensure!(
            contract.id == artifact_id,
            "{} declares artifact {}, expected {}",
            binary_path.display(),
            contract.id,
            artifact_id
        );
        if !artifacts.contains_key(&artifact_id) {
            let source = BinarySource::open(&binary_path)?;
            let reference = BinaryReference::from_source(relative, contract, &source)?;
            artifacts.insert(artifact_id.clone(), reference);
        }
        let component = required
            .component_instance
            .map(ComponentInstanceId::new)
            .transpose()?;
        participants.push(RuntimeParticipant::new(
            participant_id,
            artifact_id,
            required.config,
            component,
            clock,
        ));
    }

    let assets = AssetIndex::from_bytes(&resolved.compiled.assets)?;
    let router = resolved
        .compiled
        .router
        .as_ref()
        .map(|router| {
            BundlePath::new(format!(
                "{}/{}",
                phoxal_bundle::ASSETS_DIR,
                router.asset.as_str()
            ))
            .map(RuntimeRouterConfig::new)
        })
        .transpose()?;
    let runtime = Runtime::new(
        resolved.compiled.robot.clone(),
        artifacts,
        participants,
        assets,
        router,
    )?;
    let document = RuntimeDocument::new(runtime);
    let bytes = serde_json::to_vec_pretty(&document)?;
    std::fs::write(root.join(phoxal_bundle::RUNTIME_FILE), bytes)
        .context("failed to write runtime.json")?;
    RuntimeBundle::open_verified(root).context("failed to verify the runtime bundle candidate")
}
