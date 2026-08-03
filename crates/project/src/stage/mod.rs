//! The unified runtime-layout stager.
//!
//! One stager materializes a source project into the runtime layout at
//! `.phoxal/bundle/` (organization#951 WS4 - no per-triple nesting, since one
//! robot targets one platform at a time):
//!
//! ```text
//! robot.json  # canonical compiler output
//! assets/     # compiler-owned model assets plus CLI runtime metadata
//! bin/        # flat binary lookup store
//! ```
//!
//! The canonical `robot.json` + assets swap is atomic per refresh (stage into a
//! sibling temp dir, then rename), so a crashed pass never leaves a
//! half-written layout. `bin/` is (re)populated from official packages and
//! resolved binaries every refresh so it can never go stale. `cargo install
//! --root <candidate>` targets the SAME candidate directory the canonical model
//! and assets are staged into, so its `bin/` entries land directly at their
//! final path with no separate harvest-then-link step; `cargo install` also
//! leaves `.crates.toml`/`.crates2.json` dotfiles in the candidate root,
//! which the archiver (`phoxal build`) excludes rather than fighting
//! `--no-track` (organization#951 WS4: `--no-track` disables Cargo's own
//! concurrent-invocation protection for no real benefit here).
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
//! the last call. The test-only `stage_runtime_layout` helper collapses the
//! two into one for a caller that never populates `bin/`; a caller that does
//! MUST use the split functions - see `run::prepare::refresh_staging` and
//! `simulation::prepare_simulation`.

mod candidate;
mod participants;
mod publish;

pub(crate) use candidate::{begin_runtime_layout, copy_tree_into};
pub(crate) use participants::{
    MaterializeSettings, materialize_candidate_store, stage_named_binary, stage_participant_binary,
};
pub(crate) use publish::publish_runtime_layout;

#[cfg(test)]
pub(crate) use candidate::{compile_test_bundle, write_test_layout};
#[cfg(test)]
use participants::{canonical_binary_name, copy_binary};
#[cfg(test)]
use publish::layout_path;

/// Stage and immediately publish a bare manifest/assets layout in tests.
#[cfg(test)]
pub(crate) fn stage_runtime_layout(
    project_root: &std::path::Path,
    resolved: &phoxal_cli_core::project::resolver::BundlePlan,
) -> anyhow::Result<std::path::PathBuf> {
    let candidate = begin_runtime_layout(project_root, resolved)?;
    publish_runtime_layout(candidate, resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::host::test_support::ScratchPhoxalHome;
    use crate::resolve::project::host_target_triple;
    use anyhow::{Context, Result, ensure};
    use phoxal_cli_core::identity::{ExecutionId, ProducerId};
    use phoxal_cli_core::project::catalog::ArtifactKind;
    use phoxal_cli_core::project::launch_plan::{ParticipantExecution, ParticipantLaunchRecord};
    use phoxal_cli_core::project::resolver::{
        BundlePlan, ResolvedComponent, ResolvedPlatformRuntime, ResolvedUserRuntime,
        official_binary_name,
    };
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    fn bundle_plan() -> Result<BundlePlan> {
        let yaml = r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: model/structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;
        let source_manifest = phoxal_manifest::source::robot::parse_from_string(yaml)?;
        let compiled = compile_test_bundle(&source_manifest)?;
        Ok(BundlePlan {
            source_manifest,
            compiled,
            train: "0.36.0".to_string(),
            target: host_target_triple(),
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            undeclared_runtimes: Vec::new(),
            components: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    fn launch_record(
        participant_id: &str,
        artifact_id: &str,
        execution: ParticipantExecution,
        component_instance: Option<&str>,
    ) -> ParticipantLaunchRecord {
        use phoxal_runtime_contract::{
            BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
        };
        ParticipantLaunchRecord {
            artifact_id: artifact_id.to_string(),
            execution,
            launch: ParticipantLaunch {
                participant_id: participant_id.to_string(),
                execution: ExecutionId::mint(),
                producer: ProducerId::mint(),
                execution_origin: None,
                namespace: "dev".to_string(),
                robot_id: "testbot".to_string(),
                bus: BusProfile {
                    connect_endpoints: Vec::new(),
                },
                clock: ClockMode::Real,
                config: None,
                bundle_root: None,
                component_instance: component_instance.map(str::to_string),
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
            startup_requirement: phoxal_cli_core::runtime::StartupRequirement::Required,
            runtime_failure: phoxal_cli_core::runtime::RuntimeFailurePolicy::StopProject,
        }
    }

    #[test]
    fn stages_the_full_layout_without_cargo_or_source_or_dotphoxal() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model/meshes"))?;
        fs::create_dir_all(project.path().join("behaviors"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        fs::write(project.path().join("model/meshes/chassis.dae"), "mesh")?;
        fs::write(
            project.path().join("behaviors/default.yaml"),
            "behavior: []",
        )?;
        // Source-project noise that must never appear in the staged layout.
        fs::write(project.path().join("Cargo.toml"), "[workspace]\n")?;
        fs::write(project.path().join("lib.rs"), "fn main() {}\n")?;

        let resolved = bundle_plan()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;
        assert_eq!(staged, layout_path(project.path(), &resolved));
        assert!(staged.starts_with(project.path().join(".phoxal/bundle")));

        assert!(staged.join("robot.json").is_file());
        assert!(staged.join("assets/participants.json").is_file());
        assert!(staged.join("assets/runtime.json").is_file());
        assert!(staged.join("bin").is_dir());

        // No authored manifests, model sources, Cargo inputs, or nested
        // `.phoxal` enter the runtime bundle.
        assert!(!staged.join("robot.yaml").exists());
        assert!(!staged.join("model").exists());
        assert!(!staged.join("behaviors").exists());
        assert!(!staged.join("Cargo.toml").exists());
        assert!(!staged.join("lib.rs").exists());
        assert!(!staged.join(".phoxal").exists());

        // Re-stage replaces the previous generation atomically (no leftovers).
        fs::write(staged.join("stale"), "old")?;
        stage_runtime_layout(project.path(), &resolved)?;
        assert!(!staged.join("stale").exists());
        assert!(!project.path().join(".phoxal/bundle.previous").exists());
        Ok(())
    }

    #[test]
    fn compiled_manifest_carries_the_authored_declarations_verbatim() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;

        let mut resolved = bundle_plan()?;
        // The declaration model (#950): the authored map is complete - one
        // declared service (selected) and one discovered crate that is NOT
        // declared and therefore never enters the compiled document.
        resolved.source_manifest.services.insert(
            "mission".to_string(),
            phoxal_manifest::source::robot::v0::UserService {
                config: Some(serde_json::json!({"speed": 1})),
            },
        );
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("services/mission"),
            source_hash: "hash".to_string(),
        });
        resolved
            .undeclared_runtimes
            .push(phoxal_cli_core::project::resolver::UndeclaredRuntime {
                name: "telemetry".to_string(),
                family: "services",
            });
        resolved.compiled.participants = vec![phoxal_manifest::Participant {
            id: "mission".to_string(),
            kind: phoxal_manifest::ParticipantKind::Service,
            component_instance: None,
            config: Some(serde_json::json!({"speed": 1})),
        }];

        let staged = stage_runtime_layout(project.path(), &resolved)?;
        let compiled = phoxal_cli_core::project::layout::decode_participants(
            &staged.join("assets/participants.json"),
        )?;
        // Compiler-owned declarations preserve authored configuration; the
        // undeclared discovered crate is absent and nothing is injected.
        assert_eq!(
            compiled
                .iter()
                .map(|participant| participant.id.as_str())
                .collect::<Vec<_>>(),
            vec!["mission"]
        );
        assert_eq!(compiled[0].config, Some(serde_json::json!({"speed": 1})));
        Ok(())
    }

    #[test]
    fn compiler_assets_cannot_overwrite_cli_owned_bundle_metadata() -> Result<()> {
        for reserved in [
            phoxal_cli_core::project::layout::PARTICIPANTS_ASSET,
            phoxal_cli_core::project::layout::ROUTER_CONFIG_ASSET,
            phoxal_cli_core::project::layout::RUNTIME_HEADER_ASSET,
        ] {
            let project = tempfile::tempdir()?;
            let mut resolved = bundle_plan()?;
            resolved.compiled.assets.insert(
                phoxal_manifest::AssetId::new(reserved.to_string())?,
                b"compiler-owned collision".to_vec(),
            );
            let error = match begin_runtime_layout(project.path(), &resolved) {
                Ok(_) => panic!("compiler assets must not overwrite CLI metadata"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains(reserved), "{error}");
            assert!(error.contains("collides"), "{error}");
        }
        Ok(())
    }

    /// The authored router config is copied into its fixed CLI-owned asset path
    /// and remains available after source-free extraction.
    #[test]
    fn router_config_is_staged_into_the_layout_and_resolves_after_extraction() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::create_dir_all(project.path().join("config"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        fs::write(
            project.path().join("config/zenoh.json5"),
            "{ mode: \"router\" }",
        )?;

        let mut resolved = bundle_plan()?;
        resolved.source_manifest.router.config = Some(PathBuf::from("config/zenoh.json5"));
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        let staged_router = crate::run::prepare::resolve_layout_router_config(&staged)?
            .context("staged router config must resolve")?;
        assert_eq!(
            staged_router,
            staged.join(phoxal_cli_core::project::layout::ROUTER_CONFIG_PATH)
        );
        assert_eq!(fs::read_to_string(staged_router)?, "{ mode: \"router\" }");

        let extracted = tempfile::tempdir()?;
        let extracted_root = extracted.path().join("layout");
        copy_tree(&staged, &extracted_root)?;
        let extracted_router = crate::run::prepare::resolve_layout_router_config(&extracted_root)?
            .context("extracted router config must resolve")?;
        assert_eq!(
            extracted_router,
            extracted_root.join(phoxal_cli_core::project::layout::ROUTER_CONFIG_PATH)
        );
        assert_eq!(
            fs::read_to_string(extracted_router)?,
            "{ mode: \"router\" }"
        );
        Ok(())
    }

    /// A `router.config` that escapes its source root is rejected at staging.
    #[test]
    fn router_config_cannot_escape_the_runtime_layout() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = bundle_plan()?;
        resolved.source_manifest.router.config = Some(PathBuf::from("../outside.json5"));
        let error = stage_runtime_layout(project.path(), &resolved)
            .unwrap_err()
            .to_string();
        assert!(error.contains("router.config"), "{error}");
        Ok(())
    }

    /// A small recursive copy for the extraction simulation above.
    fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let target = dest.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_tree(&entry.path(), &target)?;
            } else {
                fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
    }

    #[test]
    fn authored_robot_structure_path_never_controls_the_runtime_layout() -> Result<()> {
        let project = tempfile::tempdir()?;
        for structure in [
            PathBuf::from("../outside.urdf"),
            PathBuf::from("/tmp/outside.urdf"),
        ] {
            let mut resolved = bundle_plan()?;
            resolved.source_manifest.robot.structure = structure.clone();
            let staged = stage_runtime_layout(project.path(), &resolved)?;
            assert!(staged.join("robot.json").is_file());
            assert!(!staged.join("outside.urdf").exists());
            assert!(!staged.join("model").exists());
        }
        Ok(())
    }

    #[test]
    fn failed_candidate_preserves_previous_layout() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "first")?;
        let mut resolved = bundle_plan()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;
        let previous_robot = fs::read(staged.join("robot.json"))?;

        resolved.compiled.robot = b"invalid canonical model".to_vec();
        assert!(stage_runtime_layout(project.path(), &resolved).is_err());
        assert_eq!(fs::read(staged.join("robot.json"))?, previous_robot);
        Ok(())
    }

    /// The atomicity promise the module docs make, exercised past the point
    /// [`failed_candidate_preserves_previous_layout`] stops at: a failure
    /// during binary MATERIALIZATION (`materialize_candidate_store`, after
    /// `begin_runtime_layout` already succeeded) must never touch the
    /// previously published, running bundle. Uses the `officials_source`
    /// hard-error path (organization#951 WS4 review, blocker 2) as the
    /// deterministic, offline-safe failure trigger: a container-shaped
    /// directory whose `bin/` is missing the expected official.
    ///
    /// "...and_runnable" is proven literally (organization#951 WS4 review,
    /// round 2 nitpick): the marker is a real executable shell script, run
    /// via `std::process::Command` both before and after the failed second
    /// refresh, not just read as bytes - a test named "runnable" that only
    /// ever read the file would claim more than it proved.
    #[test]
    fn materialization_failure_leaves_previous_bundle_intact_and_runnable() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = bundle_plan()?;
        resolved.platform_runtimes = vec![platform_runtime("mission", ArtifactKind::Service)];
        let binary_name = official_binary_name(ArtifactKind::Service, "mission");

        // First refresh: a complete `officials_source` (the shape the
        // container builder hands to host-side staging) materializes and
        // publishes cleanly. The marker is a real, executable shell script -
        // not just bytes - so the "runnable" half of this test's name is
        // literally exercised below, not merely asserted by content.
        let complete_officials = tempfile::tempdir()?;
        fs::create_dir_all(complete_officials.path().join("bin"))?;
        let marker_script = complete_officials.path().join("bin").join(&binary_name);
        fs::write(&marker_script, "#!/bin/sh\necho first-runnable-bundle\n")?;
        fs::set_permissions(&marker_script, fs::Permissions::from_mode(0o755))?;
        let candidate = begin_runtime_layout(project.path(), &resolved)?;
        materialize_candidate_store(
            candidate.path(),
            &resolved,
            &[],
            true,
            Some(complete_officials.path()),
            &MaterializeSettings::default(),
            &crate::SilentReporter,
            |_, _| unreachable!("no path-overridden official in this fixture"),
        )?;
        let staged_root = publish_runtime_layout(candidate, &resolved)?;
        let published_binary = staged_root.join("bin").join(&binary_name);
        assert_eq!(run_marker(&published_binary)?, "first-runnable-bundle\n");

        // Second refresh: `officials_source` is present (as a container run
        // would be) but incomplete - `bin/` is missing `mission` entirely.
        // `materialize_candidate_store` must hard error rather than silently
        // falling back to a host-side `cargo install`.
        let incomplete_officials = tempfile::tempdir()?;
        fs::create_dir_all(incomplete_officials.path().join("bin"))?;
        let next_candidate = begin_runtime_layout(project.path(), &resolved)?;
        let candidate_dir = next_candidate.path().to_path_buf();
        let error = materialize_candidate_store(
            next_candidate.path(),
            &resolved,
            &[],
            true,
            Some(incomplete_officials.path()),
            &MaterializeSettings::default(),
            &crate::SilentReporter,
            |_, _| unreachable!("no path-overridden official in this fixture"),
        )
        .expect_err("an officials_source missing an expected official must hard error");
        assert!(
            error.to_string().contains("mission"),
            "error should name the missing official: {error}"
        );

        // The caller never publishes on this error path (the same `?`
        // early-return every real caller uses) - dropping the candidate here
        // reproduces that, and its `TempDir` cleans itself up.
        drop(next_candidate);
        assert!(
            !candidate_dir.exists(),
            "a discarded candidate must not leak its temp directory"
        );

        // The previously published bundle is completely untouched: same
        // path, same bytes, and genuinely still executes successfully.
        assert!(staged_root.is_dir());
        assert_eq!(run_marker(&published_binary)?, "first-runnable-bundle\n");
        Ok(())
    }

    /// Execute the marker script at `path` and return its captured stdout,
    /// failing loudly if it did not exit successfully - the literal proof
    /// half of [`materialization_failure_leaves_previous_bundle_intact_and_runnable`].
    fn run_marker(path: &Path) -> Result<String> {
        let output = std::process::Command::new(path)
            .output()
            .with_context(|| format!("failed to execute marker binary {}", path.display()))?;
        ensure!(
            output.status.success(),
            "marker binary {} exited with {}: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .with_context(|| format!("marker binary {} wrote non-UTF8 stdout", path.display()))
    }

    #[test]
    fn stages_a_participant_binary_into_a_flat_bin_store_by_hardlink_identity() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let resolved = bundle_plan()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        // A "built workspace artifact" living in a cargo-style target dir.
        let target_dir = project.path().join("target/debug");
        fs::create_dir_all(&target_dir)?;
        let built = target_dir.join("mission");
        fs::write(&built, "ELF")?;

        let record = launch_record(
            "mission",
            "mission",
            ParticipantExecution::UserService {
                binary_name: "mission".to_string(),
            },
            None,
        );
        let staged_bin = stage_participant_binary(&staged, &record, &built)?;

        // The returned path is the flat store entry, not the cargo target path.
        assert_eq!(staged_bin, staged.join("bin/mission"));
        assert!(staged_bin.is_file());
        // Hardlink identity: same inode as the built artifact.
        assert_eq!(
            fs::metadata(&built)?.ino(),
            fs::metadata(&staged_bin)?.ino(),
            "bin/ entry must be a hardlink to the built artifact"
        );

        // A refresh after the artifact changes relinks - bin/ never goes stale.
        fs::remove_file(&built)?;
        fs::write(&built, "ELF-v2")?;
        let refreshed = stage_participant_binary(&staged, &record, &built)?;
        assert_eq!(fs::read_to_string(&refreshed)?, "ELF-v2");
        assert_eq!(fs::metadata(&built)?.ino(), fs::metadata(&refreshed)?.ino());
        Ok(())
    }

    #[test]
    fn copy_fallback_reproduces_bytes_and_mode_with_a_distinct_inode() -> Result<()> {
        // The cross-filesystem fallback path `link_or_copy` takes when
        // `hard_link` fails: identical bytes and executable bits, distinct inode.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("src");
        fs::write(&source, "ELF")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;
        let dest = dir.path().join("bin/dest");
        fs::create_dir_all(dest.parent().unwrap())?;

        copy_binary(&source, &dest)?;
        assert_eq!(fs::read_to_string(&dest)?, "ELF");
        assert_ne!(fs::metadata(&source)?.ino(), fs::metadata(&dest)?.ino());
        assert_eq!(fs::metadata(&dest)?.permissions().mode() & 0o111, 0o111);
        Ok(())
    }

    /// Every participant, on every host - the darwin path included - gets a flat
    /// `bin/` entry; there is no `.app`-bundle carve-out (#936, finding D). A
    /// robot participant is always a plain executable (a cargo-built binary),
    /// so even one whose source lives under a `.app`-shaped path is flattened
    /// into `bin/` under its canonical identity, and the loader resolves it
    /// there with no host-specific tolerance.
    #[test]
    fn a_participant_is_always_flattened_into_bin_even_on_darwin() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let resolved = bundle_plan()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        // A plain source executable, as a cargo-built binary always is -
        // never a `.app` bundle.
        let source_dir = project.path().join("target/debug");
        fs::create_dir_all(&source_dir)?;
        let source = source_dir.join("phoxal-service-drive");
        fs::write(&source, "MACHO")?;

        let record = launch_record(
            "drive",
            "drive",
            ParticipantExecution::OfficialArtifact {
                binary_name: "phoxal-service-drive".to_string(),
            },
            None,
        );
        let executable = stage_participant_binary(&staged, &record, &source)?;
        // The flat `bin/` entry is created and returned - strict everywhere.
        assert_eq!(executable, staged.join("bin/phoxal-service-drive"));
        assert!(executable.is_file());
        assert_eq!(fs::read_to_string(&executable)?, "MACHO");
        Ok(())
    }

    #[test]
    fn canonical_binary_names_are_identity_keyed_across_sources() {
        // The source-free plan (#936) names each participant's `bin/` binary on
        // its execution, so `canonical_binary_name` is that name and the loader
        // resolves the identical one from compiler-owned declarations.
        assert_eq!(
            canonical_binary_name(&launch_record(
                "drive",
                "drive",
                ParticipantExecution::OfficialArtifact {
                    binary_name: "phoxal-service-drive".to_string()
                },
                None,
            )),
            "phoxal-service-drive"
        );
        // A user service is stored under its own identity.
        assert_eq!(
            canonical_binary_name(&launch_record(
                "mission",
                "mission",
                ParticipantExecution::UserService {
                    binary_name: "mission".to_string(),
                },
                None,
            )),
            "mission"
        );
        // A component driver is named by its component id and shared across
        // every instance - whether built from source or resolved from the
        // registry.
        for instance in ["left_drive", "right_drive"] {
            assert_eq!(
                canonical_binary_name(&launch_record(
                    instance,
                    "ddsm115",
                    ParticipantExecution::ComponentDriver {
                        binary_name: "phoxal-component-ddsm115".to_string(),
                    },
                    Some(instance),
                )),
                "phoxal-component-ddsm115"
            );
        }
    }

    #[test]
    fn stages_officials_and_shared_drivers_under_canonical_identity_names() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let resolved = bundle_plan()?;
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        let target_dir = project.path().join("target/debug");
        fs::create_dir_all(&target_dir)?;
        // A cargo-built official-service override and one driver binary shared
        // by two component instances - both under their local cargo names.
        let drive = target_dir.join("robot-service-drive");
        fs::write(&drive, "DRIVE")?;
        let driver = target_dir.join("ddsm115-driver");
        fs::write(&driver, "DDSM")?;

        // The source-overridden official lands at the canonical name.
        let staged_drive = stage_participant_binary(
            &staged,
            &launch_record(
                "drive",
                "drive",
                ParticipantExecution::OfficialArtifact {
                    binary_name: "phoxal-service-drive".to_string(),
                },
                None,
            ),
            &drive,
        )?;
        assert_eq!(staged_drive, staged.join("bin/phoxal-service-drive"));
        assert!(staged_drive.is_file());

        // One driver binary, one bin/ entry, both instances resolve to it.
        let left = stage_participant_binary(
            &staged,
            &launch_record(
                "left_drive",
                "ddsm115",
                ParticipantExecution::ComponentDriver {
                    binary_name: "phoxal-component-ddsm115".to_string(),
                },
                Some("left_drive"),
            ),
            &driver,
        )?;
        let right = stage_participant_binary(
            &staged,
            &launch_record(
                "right_drive",
                "ddsm115",
                ParticipantExecution::ComponentDriver {
                    binary_name: "phoxal-component-ddsm115".to_string(),
                },
                Some("right_drive"),
            ),
            &driver,
        )?;
        let shared = staged.join("bin/phoxal-component-ddsm115");
        assert_eq!(left, shared);
        assert_eq!(right, shared);
        assert!(shared.is_file());
        assert_eq!(fs::metadata(&driver)?.ino(), fs::metadata(&shared)?.ino());
        Ok(())
    }

    #[test]
    fn stages_component_bundle_without_mutating_its_source_tree() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "robot")?;
        let source = project.path().join("components/ddsm115");
        fs::create_dir_all(source.join("meshes"))?;
        fs::write(source.join("component.yaml"), "schema: component/v0\n")?;
        fs::write(source.join("simulation.yaml"), "device: motor\n")?;
        fs::write(source.join("meshes/wheel.dae"), "wheel")?;
        let source_before = [
            fs::read(source.join("component.yaml"))?,
            fs::read(source.join("simulation.yaml"))?,
            fs::read(source.join("meshes/wheel.dae"))?,
        ];

        let mut resolved = bundle_plan()?;
        resolved.components.push(ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets_root: source.clone(),
            driver: None,
        });

        let staged = stage_runtime_layout(project.path(), &resolved)?;

        assert!(staged.join("robot.json").is_file());
        assert!(!staged.join("components/ddsm115/component.yaml").exists());
        assert!(!staged.join("components/ddsm115/simulation.yaml").exists());
        assert!(!staged.join("components/ddsm115/meshes/wheel.dae").exists());
        assert_eq!(
            source_before,
            [
                fs::read(source.join("component.yaml"))?,
                fs::read(source.join("simulation.yaml"))?,
                fs::read(source.join("meshes/wheel.dae"))?,
            ],
            "staging must leave source component assets unchanged"
        );
        Ok(())
    }

    fn platform_runtime(name: &str, kind: ArtifactKind) -> ResolvedPlatformRuntime {
        let dir = match kind {
            ArtifactKind::Service => "service",
            ArtifactKind::Simulator => "simulator",
            _ => unreachable!("only services/simulators are platform runtimes here"),
        };
        ResolvedPlatformRuntime {
            name: name.to_string(),
            package: format!("phoxal/{dir}-{name}"),
            kind,
            path_override: None,
            train: "0.36.0".to_string(),
            target: Some(host_target_triple()),
        }
    }

    /// A source-overridden dormant official is built through `build_override`
    /// and never reaches `cargo install` - this is the one materialization
    /// path unit-testable with no network. The `cargo install` path itself is
    /// covered by `crate::build::materialise`'s own command-construction tests.
    #[test]
    fn materialize_builds_source_overridden_dormant_officials() -> Result<()> {
        let _scratch = ScratchPhoxalHome::new()?;
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join("model"))?;
        fs::write(project.path().join("model/structure.urdf"), "<robot/>")?;
        let mut resolved = bundle_plan()?;
        let mut drive = platform_runtime("drive", ArtifactKind::Service);
        drive.path_override = Some(PathBuf::from("services/drive"));
        resolved.platform_runtimes = vec![drive];
        let staged = stage_runtime_layout(project.path(), &resolved)?;

        // The override "build" produces a real artifact the store links from.
        let built = project.path().join("target/debug/robot-service-drive");
        fs::create_dir_all(built.parent().unwrap())?;
        fs::write(&built, "OVERRIDE")?;
        materialize_candidate_store(
            &staged,
            &resolved,
            &[],
            false,
            None,
            &MaterializeSettings::default(),
            &crate::SilentReporter,
            |crate_dir, name| {
                assert_eq!(crate_dir, Path::new("services/drive"));
                assert_eq!(name, "drive");
                Ok(built.clone())
            },
        )?;

        assert_eq!(
            fs::read_to_string(staged.join("bin/phoxal-service-drive"))?,
            "OVERRIDE"
        );
        Ok(())
    }
}
