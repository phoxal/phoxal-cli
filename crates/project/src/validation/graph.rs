//! Graph responsibilities for check.

use super::{
    CheckGraphContext, CheckOutcome, PlatformArtifactRef, RawParticipantReport,
    ensure_brain_declares_unit_config, validate_artifact_identity, validate_component_driver_block,
    validate_source_artifact_identity, validate_user_service_config,
};
use crate::check as graph_check;
use crate::check::source::SourceParticipant;
use crate::check::source::SourceParticipantKind;
use anyhow::Context;
use anyhow::Result;

pub fn run_check_with_context(
    resolved_platform_artifact_refs: &[PlatformArtifactRef],
    source_participants: &[SourceParticipant],
    context: CheckGraphContext<'_>,
    mut fetch: impl FnMut(&str) -> Result<RawParticipantReport>,
    mut build: impl FnMut(&SourceParticipant) -> Result<RawParticipantReport>,
) -> Result<CheckOutcome> {
    let mut participants = Vec::new();
    let mut config_problems = Vec::new();

    for artifact in resolved_platform_artifact_refs {
        let image_ref = &artifact.binary_name;
        let raw = fetch(image_ref).with_context(|| {
            format!(
                "failed to obtain participant report for {} {} ({image_ref})",
                artifact.kind_label(),
                artifact.name
            )
        })?;
        validate_artifact_identity(
            artifact.kind_label(),
            &artifact.name,
            artifact.kind.as_str(),
            &raw,
        )?;
        let participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret participant report for {} {} ({image_ref})",
                    artifact.kind_label(),
                    artifact.name
                )
            })?;
        if artifact.instances.is_empty() {
            participants.push(participant);
        } else {
            // A registry-materialized component driver is installed once but
            // launched once per instance that declares it - key each
            // instance's graph membership by its own id, exactly like a
            // workspace-built driver source participant does (see
            // `SourceParticipantKind::ComponentDriver` below). The driver
            // block is per instance too, so it is checked here rather than
            // once for the shared binary.
            for instance in &artifact.instances {
                let mut instance_participant = participant.clone();
                instance_participant.participant_id = instance.clone();
                instance_participant.scope =
                    graph_check::ParticipantScope::ComponentInstance(instance.clone());
                config_problems.extend(validate_component_driver_block(
                    instance,
                    &instance_participant.artifact_id,
                    instance_participant.config_schema.as_ref(),
                    instance_participant.connection,
                    context.robot,
                )?);
                participants.push(instance_participant);
            }
        }
    }

    for participant in source_participants {
        let raw = build(participant).with_context(|| {
            format!(
                "failed to obtain participant report for {} {} ({})",
                participant.kind_label(),
                participant.name,
                participant.crate_dir.display()
            )
        })?;
        validate_source_artifact_identity(participant, &raw)?;
        let mut participant_apis = graph_check::ParticipantApis::try_from(raw.clone())
            .with_context(|| {
                format!(
                    "failed to interpret participant report for {} {} ({})",
                    participant.kind_label(),
                    participant.name,
                    participant.crate_dir.display()
                )
            })?;
        if participant.kind == SourceParticipantKind::Brain {
            // The brain has no config side channel, so a non-unit schema is a
            // malformed binary rather than a user config mistake: fail hard
            // here, before any supervisor startup.
            ensure_brain_declares_unit_config(participant, &raw)?;
        } else if participant.kind == SourceParticipantKind::ComponentDriver {
            // A component driver is launched once per component instance. Several
            // instances of the same driver share `artifact_id` (validated against
            // the emitted artifact identity), so key graph membership and
            // diagnostics by the concrete instance id instead.
            participant_apis.participant_id = participant.name.clone();
            participant_apis.scope =
                graph_check::ParticipantScope::ComponentInstance(participant.name.clone());
            config_problems.extend(validate_component_driver_block(
                &participant.name,
                &participant_apis.artifact_id,
                participant_apis.config_schema.as_ref(),
                participant_apis.connection,
                context.robot,
            )?);
        } else if participant.kind == SourceParticipantKind::UserService
            && let Some(problem) = validate_user_service_config(
                &participant.name,
                participant_apis.config_schema.as_ref(),
                context.robot,
            )
        {
            config_problems.push(problem);
        }
        participants.push(participant_apis);
    }

    // Construct the shared report vehicle and append this pass's
    // `InvalidConfig` findings directly.
    let mut report = graph_check::Report::default();
    report.problems.extend(config_problems);
    Ok(CheckOutcome {
        report,
        checked_participants: participants,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::RawArtifact;
    use anyhow::{Result, anyhow, bail};
    use phoxal_cli_catalog::ArtifactKind;
    use std::path::{Path, PathBuf};

    /// Converts fixture `(name, artifact_ref)` pairs into service artifact refs.
    fn platform_refs(images: &[(String, String)]) -> Vec<PlatformArtifactRef> {
        images
            .iter()
            .map(|(name, binary_name)| PlatformArtifactRef {
                name: name.clone(),
                kind: ArtifactKind::Service,
                binary_name: binary_name.clone(),
                instances: Vec::new(),
            })
            .collect()
    }

    fn raw(id: &str) -> RawParticipantReport {
        raw_kind("service", id)
    }

    fn raw_kind(kind: &str, id: &str) -> RawParticipantReport {
        RawParticipantReport {
            artifact: RawArtifact {
                kind: kind.to_string(),
                id: id.to_string(),
            },
            config_schema: None,
            connection: None,
        }
    }

    #[test]
    fn healthy_graph_passes_with_fake_participant_report() -> Result<()> {
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![SourceParticipant::user_service(
            "drive".to_string(),
            PathBuf::from("/fake/project/runtimes/drive"),
        )];

        let outcome = run_check_with_context(
            &platform_refs(&images),
            &sources,
            CheckGraphContext { robot: None },
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw("mission")),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/drive") {
                    Ok(raw("drive"))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn healthy_graph_passes_with_platform_and_component_driver_source() -> Result<()> {
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let outcome = run_check_with_context(
            &platform_refs(&images),
            &sources,
            CheckGraphContext { robot: None },
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw("mission")),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/components/ddsm115") {
                    Ok(raw_kind("driver", "ddsm115"))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn source_and_platform_participants_coexist_in_a_healthy_graph() -> Result<()> {
        // A platform participant and a source participant both check clean.
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![SourceParticipant::user_service(
            "drive".to_string(),
            PathBuf::from("/fake/project/runtimes/drive"),
        )];

        let outcome = run_check_with_context(
            &platform_refs(&images),
            &sources,
            CheckGraphContext { robot: None },
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw("mission")),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| Ok(raw("drive")),
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn user_service_artifact_id_must_match_manifest_key() {
        let sources = vec![SourceParticipant::user_service(
            "avoid".to_string(),
            PathBuf::from("/fake/project/runtimes/avoid"),
        )];

        let error = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |_| Ok(raw("surprise")),
        )
        .expect_err("mismatched user service artifact id should abort check");

        let message = error.to_string();
        assert!(
            message.contains("artifact.id 'surprise'")
                && message.contains("expected artifact id 'avoid'"),
            "{message}"
        );
    }

    #[test]
    fn official_service_artifact_identity_must_match_resolved_name() {
        let images = vec![("drive".to_string(), "drive:swapped".to_string())];

        let error = run_check_with_context(
            &platform_refs(&images),
            &[],
            CheckGraphContext { robot: None },
            |image_ref| match image_ref {
                "drive:swapped" => Ok(raw("mission")),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no source services should be built"),
        )
        .expect_err("swapped official service artifact should abort check");

        let message = error.to_string();
        assert!(
            message.contains("official service participant report artifact.id 'mission'")
                && message.contains("expected artifact id 'drive'"),
            "{message}"
        );
    }

    #[test]
    fn official_driver_artifact_identity_uses_driver_label() {
        let artifacts = vec![PlatformArtifactRef {
            name: "bno085".to_string(),
            kind: ArtifactKind::ComponentDriver,
            binary_name: "driver-bno085:swapped".to_string(),
            instances: vec!["imu".to_string()],
        }];

        let error = run_check_with_context(
            &artifacts,
            &[],
            CheckGraphContext { robot: None },
            |artifact_ref| match artifact_ref {
                "driver-bno085:swapped" => Ok(raw_kind("service", "bno085")),
                unexpected => bail!("unexpected artifact {unexpected}"),
            },
            |_| bail!("no source services should be built"),
        )
        .expect_err("wrong official driver kind should abort check");

        let message = error.to_string();
        assert!(
            message.contains("official driver participant report artifact.kind 'service'")
                && message.contains("expected kind 'driver'"),
            "{message}"
        );
    }

    #[test]
    fn component_driver_artifact_kind_true_kind_is_accepted() -> Result<()> {
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_motor",
            "ddsm115",
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let outcome = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |_| Ok(raw_kind("driver", "ddsm115")),
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn component_driver_artifact_kind_legacy_runtime_is_rejected() {
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_motor",
            "ddsm115",
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let error = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |_| Ok(raw_kind("runtime", "ddsm115")),
        )
        .expect_err("component driver reporting legacy runtime kind should abort check");

        let message = error.to_string();
        assert!(
                message.contains(
                    "component driver participant report artifact.kind 'runtime' does not match the expected kind 'driver'"
                ),
                "{message}"
            );
    }

    #[test]
    fn every_source_participant_always_builds_no_scoping_no_cache() -> Result<()> {
        // Every source participant is rebuilt live on every `run_check_with_context`
        // invocation. This proves
        // `run_check` invokes the build closure for every source participant,
        // not just a named one.
        let sources = vec![
            SourceParticipant::user_service(
                "bad".to_string(),
                PathBuf::from("/fake/project/runtimes/bad"),
            ),
            SourceParticipant::user_service(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive".to_string(),
                "ddsm115".to_string(),
                PathBuf::from("/fake/project/components/ddsm115"),
            ),
        ];

        let mut built = Vec::new();
        let outcome = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                built.push(dir.to_path_buf());
                if dir == Path::new("/fake/project/runtimes/bad") {
                    Ok(raw("bad"))
                } else if dir == Path::new("/fake/project/runtimes/other") {
                    Ok(raw("other"))
                } else if dir == Path::new("/fake/project/components/ddsm115") {
                    Ok(raw_kind("driver", "ddsm115"))
                } else {
                    bail!("unexpected source participant: {}", dir.display())
                }
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        assert_eq!(
            built,
            vec![
                PathBuf::from("/fake/project/runtimes/bad"),
                PathBuf::from("/fake/project/runtimes/other"),
                PathBuf::from("/fake/project/components/ddsm115"),
            ],
            "every source participant must build, every invocation - no scoping, no cache"
        );
        Ok(())
    }

    #[test]
    fn component_driver_with_no_producer_is_a_legal_graph() -> Result<()> {
        // A component driver subscribing to a contract with no producer in the
        // graph is legal under the relaxed graph check.
        let sources = vec![
            SourceParticipant::user_service(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive".to_string(),
                "ddsm115".to_string(),
                PathBuf::from("/fake/project/components/ddsm115"),
            ),
        ];

        let outcome = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/other") {
                    Ok(raw("other"))
                } else if dir == Path::new("/fake/project/components/ddsm115") {
                    Ok(raw_kind("driver", "ddsm115"))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(outcome.report.problems.is_empty());
        Ok(())
    }

    #[test]
    fn user_service_with_no_producer_is_a_legal_graph() -> Result<()> {
        let sources = vec![
            SourceParticipant::user_service(
                "bad".to_string(),
                PathBuf::from("/fake/project/runtimes/bad"),
            ),
            SourceParticipant::user_service(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
        ];

        let outcome = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/bad") {
                    Ok(raw("bad"))
                } else if dir == Path::new("/fake/project/runtimes/other") {
                    Ok(raw("other"))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(outcome.report.problems.is_empty());
        Ok(())
    }

    #[test]
    fn component_driver_and_platform_participants_coexist_in_a_healthy_graph() -> Result<()> {
        // A component driver and a platform publisher both check clean. The
        // driver still appears under its concrete instance id (`left_drive`),
        // not the shared driver artifact (`ddsm115`), so multiple instances
        // of one driver stay distinct.
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let outcome = run_check_with_context(
            &platform_refs(&images),
            &sources,
            CheckGraphContext { robot: None },
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw("mission")),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| Ok(raw_kind("driver", "ddsm115")),
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        let participant_ids = outcome
            .checked_participants
            .iter()
            .map(|participant| participant.participant_id.as_str())
            .collect::<Vec<_>>();
        assert!(participant_ids.contains(&"left_drive"));
        Ok(())
    }

    #[test]
    fn source_build_error_is_a_hard_error() {
        let sources = vec![SourceParticipant::user_service(
            "drive".to_string(),
            PathBuf::from("/fake/project/runtimes/drive"),
        )];

        let error = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |_| Err(anyhow!("source build failed")),
        )
        .expect_err("source build failures should abort check");

        let message = format!("{error:#}");
        assert!(
            message.contains("failed to obtain participant report for user service drive"),
            "{message}"
        );
        assert!(message.contains("source build failed"), "{message}");
    }

    #[test]
    fn component_driver_build_error_is_a_hard_error() {
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let error = run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |_| Err(anyhow!("component build failed")),
        )
        .expect_err("component driver build failures should abort check");

        let message = format!("{error:#}");
        assert!(
            message.contains("failed to obtain participant report for component driver left_drive"),
            "{message}"
        );
        assert!(message.contains("component build failed"), "{message}");
    }
}
