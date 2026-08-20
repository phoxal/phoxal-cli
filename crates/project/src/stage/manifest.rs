//! Writing the one persisted document a bundle carries.
//!
//! There is nothing to index and nothing to hash here. `manifest.json` is the
//! compiled robot under its schema tag, and everything a reader needs - the
//! process set, each process's configuration, the whole robot model - is inside
//! it. A binary is found in `bin/` by the id it was launched under, so no path
//! is recorded; integrity is the archive's job, so no digest is recorded.
//!
//! What this *does* still do is prove, once, that every executable the robot
//! will launch is actually in `bin/` and declares the identity the manifest
//! launches it under. That check has to live on the writing side: after
//! publication nobody re-reads a binary's embedded metadata, and a bundle whose
//! `bin/drive` is some other participant would only be discovered by the
//! failure it causes at runtime.

use std::path::Path;

use anyhow::{Context, Result};
use phoxal::bundle::RuntimeBundle;
use phoxal::model::manifest::ManifestDocument;

use crate::source::resolver::BundlePlan;

/// Write `manifest.json` into a staged candidate and open the result.
///
/// Every selected binary is judged against the framework the project selected
/// before the document is written. That is the project-side proof that the
/// finalized graph shares one compatibility line: it arrives with the project
/// as the stated authority and names the offending binary.
pub(crate) fn write_manifest_document(root: &Path, resolved: &BundlePlan) -> Result<RuntimeBundle> {
    // `cargo install --root <candidate>` leaves its bookkeeping beside `bin/`.
    // Those files carry absolute host paths and are not bundle content, so they
    // go before the candidate becomes a bundle.
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

    verify_staged_binaries(root, &resolved.compiled.robot, resolved.train.framework())?;

    let document = ManifestDocument::new(resolved.compiled.robot.clone());
    let bytes = serde_json::to_vec_pretty(&document)?;
    std::fs::write(root.join(phoxal::bundle::MANIFEST_FILE), bytes)
        .context("failed to write manifest.json")?;
    RuntimeBundle::open(root).context("failed to open the staged bundle candidate")
}

/// Prove every launchable runtime has its executable in `bin/`, built on the
/// project's framework line, declaring the identity it will be launched under.
///
/// One binary serves every instance of a component type, so the check is per
/// distinct `bin/` entry rather than per participant - and the identity it must
/// declare is the artifact identity (`ddsm115`), which for a service and the
/// brain happens to be the launch identity too.
fn verify_staged_binaries(
    root: &Path,
    robot: &phoxal::model::Robot,
    framework: phoxal::version::FrameworkVersion,
) -> Result<()> {
    for binary in crate::runtimes::robot_binaries(robot) {
        let path = root.join(phoxal::bundle::BIN_DIR).join(&binary);
        let contract =
            crate::check::participant_metadata::extract_participant_metadata_for_project(
                &path, framework,
            )
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        anyhow::ensure!(
            contract.id.as_str() == binary,
            "{} declares participant '{}', but the manifest launches it as '{binary}'",
            path.display(),
            contract.id,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use phoxal::model::builder::RobotBuilder;

    use crate::check::participant_metadata::{
        FIXTURE_FRAMEWORK, synthesize_host_participant_object,
    };

    fn robot() -> phoxal::model::Robot {
        RobotBuilder::new("rover")
            .service("drive", None)
            .build()
            .expect("a valid canonical robot")
    }

    fn stage_binary(bin: &Path, name: &str, declares: &str, kind: &str) {
        std::fs::create_dir_all(bin).expect("the bin directory");
        std::fs::write(
            bin.join(name),
            synthesize_host_participant_object(&crate::stage::test_metadata_payload(
                declares,
                kind,
                None,
                serde_json::json!({"type": "null"}),
            )),
        )
        .expect("write the fixture binary");
    }

    /// The bundle records no path and no identity for a binary: a launcher
    /// finds `bin/drive` because the manifest launches `drive`. That
    /// convention is only safe if it is proved once, while the bundle is
    /// still a candidate, so a mislabelled binary is refused here rather than
    /// by the failure it would cause at runtime.
    #[test]
    fn a_binary_declaring_another_identity_is_refused_at_its_bundle_name() {
        let staged = tempfile::tempdir().expect("a staging root");
        let bin = staged.path().join(phoxal::bundle::BIN_DIR);
        stage_binary(&bin, "brain", "brain", "brain");
        stage_binary(&bin, "drive", "odometry", "service");

        let error = super::verify_staged_binaries(staged.path(), &robot(), FIXTURE_FRAMEWORK)
            .expect_err("a mislabelled binary must not reach a published bundle");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("declares participant 'odometry'"),
            "{rendered}"
        );
        assert!(rendered.contains("launches it as 'drive'"), "{rendered}");

        stage_binary(&bin, "drive", "drive", "service");
        super::verify_staged_binaries(staged.path(), &robot(), FIXTURE_FRAMEWORK)
            .expect("every launched binary declares the identity it is launched under");
    }

    /// A binary the manifest launches but staging never produced is named,
    /// rather than discovered by a launcher that cannot exec it.
    #[test]
    fn a_missing_binary_is_named_while_the_bundle_is_still_a_candidate() {
        let staged = tempfile::tempdir().expect("a staging root");
        let bin = staged.path().join(phoxal::bundle::BIN_DIR);
        stage_binary(&bin, "brain", "brain", "brain");

        let error = super::verify_staged_binaries(staged.path(), &robot(), FIXTURE_FRAMEWORK)
            .expect_err("an absent binary must fail the candidate");
        assert!(format!("{error:#}").contains("drive"), "{error:#}");
    }
}
