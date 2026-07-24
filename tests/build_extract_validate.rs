//! Acceptance chain, end to end (#936, finding G): a staged runtime layout is
//! archived as a deterministic `build.phoxal`, extracted through the SAFE
//! extractor to an arbitrary directory, and the extracted root passes the
//! offline loader validation with a plan digest identical to the staged root's
//! (deployment-scoped fields excepted). This is the command-level guarantee an
//! extracted bundle depends on: "source -> build -> extract -> loader-validate"
//! produces the same process graph without Cargo, suite, or network.
//!
//! The `mission` user service is the real `phoxal-cli-test-api-fixture`
//! participant binary, so genuine embedded metadata (its `drive::Target` publish
//! contract and its `gain` config schema) flows through archiving, extraction,
//! and the loader's off-disk inspection - not hand-rolled fixtures. The official
//! runtimes are synthesized host-architecture objects, since a full `phoxal
//! build` of the official catalog needs the entire vendored suite.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use phoxal_cli::archive::{extract_build_archive, write_build_archive};
use phoxal_cli::loader::validate_layout_plan;
use phoxal_cli_core::check::participant_metadata::host_architecture;
use phoxal_cli_core::project::launch_plan::{LaunchPlan, PlanRevision};
use phoxal_cli_core::project::layout::{
    DriverSelection, LayoutInspection, PlanOptions, RequiredRuntimeKind, RuntimeLayout,
};

/// A minimal, complete `robot/v0` compiled document: no components, one user
/// service `mission` whose config satisfies the api-fixture's `gain` schema.
const ROBOT_YAML: &str = r#"schema: robot/v0
robot:
  id: robot_v1
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
services:
  mission:
    config:
      gain: 2.0
"#;

/// Build the `phoxal-cli-test-api-fixture` participant and return its binary
/// path - a real host-architecture object carrying a genuine embedded metadata
/// section.
fn build_api_fixture() -> Result<PathBuf> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let package = "phoxal-cli-test-api-fixture";
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "-p", package])
        .current_dir(workspace_root)
        .status()
        .with_context(|| format!("failed to spawn cargo build for {package}"))?;
    assert!(status.success(), "cargo build -p {package} failed");
    let binary = workspace_root
        .join("target")
        .join("debug")
        .join(format!("{package}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.is_file(),
        "expected built fixture at {}",
        binary.display()
    );
    Ok(binary)
}

/// Synthesize a host-architecture ELF carrying an empty-contract metadata
/// section, standing in for an official runtime binary the vendored suite would
/// otherwise supply.
fn synthesize_official() -> Vec<u8> {
    use object::write::Object;
    let format = phoxal_cli_core::check::participant_metadata::host_binary_format();
    let (segment, name): (&[u8], &[u8]) = match format {
        object::BinaryFormat::MachO => (b"__DATA", b"__phoxal_meta"),
        _ => (b"", b".phoxal_api_meta"),
    };
    let mut obj = Object::new(format, host_architecture(), object::Endianness::Little);
    let section = obj.add_section(
        segment.to_vec(),
        name.to_vec(),
        object::SectionKind::ReadOnlyData,
    );
    obj.append_section_data(
        section,
        br#"{"participant_api":"()","contracts":[],"config_schema":{"type":"null"}}"#,
        1,
    );
    obj.write().expect("synthesize object file")
}

/// Stage a complete runtime layout at `root`: the compiled `robot.yaml` plus a
/// binary under every required `bin/` name - the real api-fixture for `mission`,
/// a synthesized object for every official.
fn stage_layout(root: &Path, fixture: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    fs::write(root.join("robot.yaml"), ROBOT_YAML)?;
    let bin = root.join("bin");
    fs::create_dir_all(&bin)?;
    let layout = RuntimeLayout::open(root)?;
    for required in layout.required_runtimes(&DriverSelection::All) {
        if required.kind == RequiredRuntimeKind::Infrastructure {
            continue;
        }
        let dest = bin.join(&required.binary_name);
        if required.binary_name == "mission" {
            fs::copy(fixture, &dest)?;
        } else {
            fs::write(&dest, synthesize_official())?;
        }
    }
    Ok(())
}

/// Erase the two deliberately deployment-scoped fields so a plan can be compared
/// for content identity across two extraction locations.
fn normalize(plan: &mut LaunchPlan) {
    for robot in &mut plan.robots {
        for participant in &mut robot.participants {
            participant.launch.robot_root = None;
            participant.launch.execution_device_id = None;
        }
    }
}

#[test]
fn source_build_extract_and_loader_validate_produce_the_same_plan() -> Result<()> {
    let fixture = build_api_fixture()?;
    let work = tempfile::tempdir()?;

    // "build": stage the runtime layout, then validate it through the shared
    // loader against the host - exactly what `phoxal build` does before archiving.
    let staged_root = work.path().join("staged");
    stage_layout(&staged_root, &fixture)?;
    let mut staged_plan = validate_layout_plan(
        &staged_root,
        &PlanOptions::default(),
        LayoutInspection::Host,
    )
    .context("the staged layout must validate through the loader")?;
    // The real fixture's user service is a planned participant.
    assert!(
        staged_plan.robots[0]
            .participants
            .iter()
            .any(|participant| participant.launch.participant_id == "mission"),
        "the api-fixture user service must be planned"
    );

    // Archive it as a deterministic `build.phoxal`, a sibling of the staged dir.
    let bundle = work.path().join("robot.build.phoxal");
    let digest = write_build_archive(&staged_root, &bundle)?;
    assert!(!digest.is_empty());

    // Extract through the SAFE extractor into a fresh, arbitrary directory - the
    // "extract to an arbitrary directory" step, with no source tree present.
    let extracted_root = work.path().join("elsewhere/extracted");
    extract_build_archive(&bundle, &extracted_root)?;
    assert!(extracted_root.join("robot.yaml").is_file());
    assert!(extracted_root.join("bin/mission").is_file());

    // The extracted root passes the offline loader validation with no Cargo,
    // suite, or network - and never touches `.phoxal/artifacts` (there is none).
    let mut extracted_plan = validate_layout_plan(
        &extracted_root,
        &PlanOptions::default(),
        LayoutInspection::Host,
    )
    .context("the extracted bundle must validate through the loader offline")?;

    // The plan digest matches the staged root's once the two deployment-scoped
    // fields (the layout root each participant runs from, and the device id
    // derived from it) are normalized: the process graph is determined by the
    // layout CONTENT, not where it was extracted.
    normalize(&mut staged_plan);
    normalize(&mut extracted_plan);
    assert_eq!(
        staged_plan, extracted_plan,
        "the extracted bundle must produce the same plan as its staged source"
    );
    assert_eq!(
        PlanRevision::compile(1, staged_plan)?.digest,
        PlanRevision::compile(1, extracted_plan)?.digest,
        "content-identical plans must have identical content digests"
    );
    Ok(())
}

/// The safe extractor is what the acceptance flow relies on: a fresh destination
/// extracts, but a non-empty destination is refused (finding E), so a real
/// `phoxal build` bundle cannot be unpacked over planted state.
#[test]
fn the_acceptance_extractor_refuses_a_dirty_destination() -> Result<()> {
    let fixture = build_api_fixture()?;
    let work = tempfile::tempdir()?;
    let staged_root = work.path().join("staged");
    stage_layout(&staged_root, &fixture)?;
    let bundle = work.path().join("robot.build.phoxal");
    write_build_archive(&staged_root, &bundle)?;

    let dirty = work.path().join("dirty");
    fs::create_dir_all(&dirty)?;
    fs::write(dirty.join("pre-existing"), b"x")?;
    let error = extract_build_archive(&bundle, &dirty)
        .expect_err("a non-empty destination must be refused");
    assert!(error.to_string().contains("not empty"), "{error}");
    Ok(())
}
