//! Bounded descriptor preparation followed by atomic activation.

use super::{
    ArtifactProgressReporter, ScopePublication, activate_packages_atomically,
    descriptor_scope_label, prepare_scope_candidate, prune_inactive_versions,
};
use crate::ui::Ui;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::artifacts::NativeArtifactDescriptor;
use phoxal_cli_core::session::human;

pub fn prepare_and_activate_descriptors(
    descriptors: &[NativeArtifactDescriptor],
    ui: Option<&Ui>,
) -> Result<()> {
    prepare_and_activate_descriptors_inner(descriptors, ui, None)
}

/// The update command's presentation-aware entry point. Only rendering is
/// delegated; the same bounded workers and atomic activation path are used.
pub(crate) fn prepare_and_activate_descriptors_with_progress(
    descriptors: &[NativeArtifactDescriptor],
    reporter: &dyn ArtifactProgressReporter,
) -> Result<()> {
    prepare_and_activate_descriptors_inner(descriptors, None, Some(reporter))
}

pub(crate) fn prepare_and_activate_descriptors_inner(
    descriptors: &[NativeArtifactDescriptor],
    ui: Option<&Ui>,
    reporter: Option<&dyn ArtifactProgressReporter>,
) -> Result<()> {
    const CONCURRENCY: usize = 4;
    let mut package_versions = std::collections::BTreeMap::new();
    for descriptor in descriptors {
        if let Some(existing) = package_versions.insert(&descriptor.package_id, &descriptor.version)
        {
            anyhow::ensure!(
                existing == &descriptor.version,
                "artifact package {} resolved multiple versions in one atomic update: {} and {}",
                descriptor.package_id,
                existing,
                descriptor.version
            );
        }
    }
    let total = descriptors.len() as u64;
    let mut completed = 0_u64;
    let mut prepared_scopes = Vec::new();
    for batch in descriptors.chunks(CONCURRENCY) {
        std::thread::scope(|scope| -> Result<()> {
            let handles = batch
                .iter()
                .map(|descriptor| {
                    scope.spawn(move || prepare_scope_candidate(descriptor, ui, reporter))
                })
                .collect::<Vec<_>>();
            for handle in handles {
                if let Some(prepared) = handle
                    .join()
                    .map_err(|_| anyhow!("artifact download worker panicked"))??
                {
                    prepared_scopes.push(prepared);
                }
            }
            Ok(())
        })?;
        // Finding C2: real per-batch progress against the "download" phase
        // `prepare_descriptors_with_preflight` brackets - the first genuine
        // producer of `SessionEvent::PhaseProgress` (see
        // `session::diagnostics::phase_progress`'s own docs).
        completed += batch.len() as u64;
        crate::session::diagnostics::phase_progress(
            phoxal_cli_core::session::event::PhaseId::new("download"),
            completed,
            total,
        );
        if reporter.is_none()
            && let Some(ui) = ui
        {
            for descriptor in batch {
                ui.info(format!(
                    "verified {} {} [{}] ({})",
                    descriptor.package_id,
                    descriptor.version,
                    descriptor_scope_label(descriptor),
                    human::bytes(descriptor.size)
                ));
            }
        }
    }
    let publication = ScopePublication::publish(prepared_scopes)?;
    activate_packages_atomically(&package_versions)?;
    publication.commit()?;
    prune_inactive_versions(&package_versions)?;
    Ok(())
}
