//! The single staged-runtime-layout loader.
//!
//! Execution consumes ONLY a staged runtime layout - `<root>/robot.yaml` (the
//! compiled, flattened `robot/v0` document), a flat `bin/` lookup store, and
//! runtime assets. There is no source loader and no compiled loader: this one
//! loader reads the same layout whether it was staged from a source project
//! into `.phoxal/build/<triple>/` or extracted from a `build.phoxal` bundle
//! (#936). Staging (Cargo, the vendored artifact store, `extends:` flattening)
//! is the only code that knows about source; the loader never does.
//!
//! The loader derives the required runtime set from two authorities - the
//! CLI-internal official catalog ([`super::catalog`]) and the compiled
//! `robot.yaml` (user services, driven component instances, robot model) -
//! resolves every required runtime to exactly one canonical binary under
//! `bin/`, and inspects only the selected binaries (host-architecture
//! compatibility plus embedded metadata) without ever executing them.
//! Unreferenced extra files in `bin/` are ignored and never inspected.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use phoxal::model::robot::{Robot as RobotDocument, v0::Robot as RobotModel};

use super::catalog::{self, OfficialRuntime};
use super::resolver::official_binary_name;
use super::suite::ArtifactKind;
use crate::check::participant_metadata::{
    ExpectedTarget, ParticipantMeta, expected_target_for_host, inspect_selected_binary_for_target,
};
use crate::schema::{DocumentKind, ensure_supported_revision};

pub mod plan;

pub use plan::PlanOptions;

const ROBOT_FILE: &str = "robot.yaml";
const BIN_DIR: &str = "bin";

/// Which target signature the loader inspects selected binaries against (#936).
/// An in-place `run`/`start` inspects against the host - a bundle only ever runs
/// on the host it was staged/extracted for. `phoxal build --target <TRIPLE>`
/// stages a foreign-target layout it will never execute here, so it inspects
/// against the *declared* target signature: a correct cross-compiled binary
/// validates, while a wrong-format, wrong-arch, or wrong-endian binary for that
/// target still fails precisely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutInspection {
    /// Inspect against the host target signature (in-place run/start).
    #[default]
    Host,
    /// Inspect against a declared `--target` signature (cross build).
    Target(ExpectedTarget),
}

impl LayoutInspection {
    /// The [`ExpectedTarget`] a selected binary is checked against.
    #[must_use]
    pub fn expected_target(self) -> ExpectedTarget {
        match self {
            Self::Host => expected_target_for_host(),
            Self::Target(expected) => expected,
        }
    }
}

/// Which official runtimes the layout requires. `Native` excludes the
/// simulator-only binaries; `Webots` adds them (they are launched by the
/// simulator, not by the CLI supervisor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeProfile {
    Native,
    Webots,
}

/// Which component drivers the run policy keeps in the required set (#936). The
/// `--drivers off` / `--driver <ID>` options on `run` gate the required set at
/// its source: an excluded driver is never required, never resolved from
/// `bin/`, never architecture-inspected, and never planned as a participant.
/// That is what lets a driven robot run on a host whose driver binaries it
/// cannot inspect (`--drivers off` on a macOS host, whose component drivers are
/// Linux-only) without the plan constructor hard-failing on a foreign-arch
/// driver binary. Selection is by component-instance id, matching the
/// `--driver <ID>` vocabulary and the plan participant ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DriverSelection {
    /// Every driven component instance is required (drivers on, no subset).
    #[default]
    All,
    /// Only these component-instance ids are required (drivers on, a
    /// `--driver <ID>` subset). A driver binary is required if at least one of
    /// its instances is selected; the unselected instances are not planned.
    Only(BTreeSet<String>),
    /// No component drivers are required (drivers off).
    None,
}

impl DriverSelection {
    /// Whether the component instance `instance` is kept by this selection.
    #[must_use]
    pub fn includes_instance(&self, instance: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(set) => set.contains(instance),
            Self::None => false,
        }
    }
}

/// The role a required runtime plays. Drives board classification and which
/// runtimes the CLI supervisor launches (services/tools/drivers) versus which
/// the simulator owns (simulators) or the CLI resolves on its own (the
/// infrastructure router).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredRuntimeKind {
    OfficialService,
    OfficialTool,
    Infrastructure,
    Simulator,
    UserService,
    ComponentDriver,
}

/// One runtime the compiled layout requires, with the canonical `bin/` file
/// name the staging step wrote it under and any config the compiled
/// `robot.yaml` carries for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredRuntime {
    /// The launch/board identity, used in diagnostics: the official short name
    /// (`drive`), the user-service name (`mission`), or the component id shared
    /// by every instance a single driver binary serves (`ddsm115`).
    pub identity: String,
    /// The canonical file name inside `bin/` the staging step wrote this
    /// runtime under, and the only file the loader ever looks up for it.
    pub binary_name: String,
    pub kind: RequiredRuntimeKind,
    /// Config carried by the compiled `robot.yaml` `services` map. `None` for a
    /// runtime with no authored config.
    pub config: Option<serde_json::Value>,
}

/// A required runtime resolved to its canonical `bin/` binary and inspected
/// (architecture-checked, metadata-extracted) without execution.
#[derive(Debug, Clone)]
pub struct SelectedBinary {
    pub path: PathBuf,
    pub meta: ParticipantMeta,
}

/// A parsed staged runtime layout: the flattened `robot/v0` document plus its
/// root, from which the required runtime set and its `bin/` binaries are
/// derived. Holds no source, no Cargo graph, and no suite.
#[derive(Debug, Clone)]
pub struct RuntimeLayout {
    root: PathBuf,
    robot: RobotModel,
}

impl RuntimeLayout {
    /// Whether `root` is shaped like a staged runtime layout - a `robot.yaml`
    /// next to a `bin/` store. Cheap existence checks only (no parse), used by
    /// universal `run` root classification to tell an extracted bundle / staged
    /// `.phoxal/build/<triple>/` directory (run in place) from a source project
    /// (staged first).
    #[must_use]
    pub fn is_layout_root(root: &Path) -> bool {
        root.join(ROBOT_FILE).is_file() && root.join(BIN_DIR).is_dir()
    }

    /// Parse the compiled `robot.yaml` at `root` through the per-document
    /// schema-revision gate and the framework's strict `robot/v0` parser. An
    /// unsupported declared revision fails with the exact "update the CLI"
    /// message before anything else runs.
    pub fn open(root: &Path) -> Result<Self> {
        let robot_path = root.join(ROBOT_FILE);
        ensure_supported_revision(&robot_path, DocumentKind::Robot)?;
        let robot = RobotDocument::parse_from_dir(root)
            .with_context(|| {
                format!(
                    "failed to parse compiled robot.yaml in staged runtime layout {}",
                    root.display()
                )
            })?
            .into_v0();
        Ok(Self {
            root: root.to_path_buf(),
            robot,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn robot(&self) -> &RobotModel {
        &self.robot
    }

    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join(BIN_DIR)
    }

    /// The runtimes the compiled layout requires. The official set comes from
    /// the CLI-internal catalog (simulator binaries excluded from `Native`);
    /// the user-service set is every `services` entry that is not an official
    /// service; component drivers are one per driven component id (a single
    /// driver binary serves every instance of that component id), gated by
    /// `drivers`: a driver whose every instance is excluded is not required, so
    /// it is never resolved or inspected (#936).
    #[must_use]
    pub fn required_runtimes(
        &self,
        profile: RuntimeProfile,
        drivers: &DriverSelection,
    ) -> Vec<RequiredRuntime> {
        let mut required = Vec::new();
        let official_services = official_service_short_names();

        for official in catalog::for_webots(profile == RuntimeProfile::Webots) {
            let short = official_short_name(official);
            let kind = match official.kind {
                ArtifactKind::Service => RequiredRuntimeKind::OfficialService,
                ArtifactKind::Tool => RequiredRuntimeKind::OfficialTool,
                ArtifactKind::Infrastructure => RequiredRuntimeKind::Infrastructure,
                ArtifactKind::Simulator => RequiredRuntimeKind::Simulator,
                // The catalog carries no component packages; assets/drivers are
                // never members of the official runtime set.
                ArtifactKind::ComponentAssets | ArtifactKind::ComponentDriver => continue,
            };
            let config = self
                .robot
                .services
                .get(&short)
                .and_then(|service| service.config.clone());
            required.push(RequiredRuntime {
                identity: short.clone(),
                binary_name: official_binary_name(official.kind, &short),
                kind,
                config,
            });
        }

        for (name, service) in &self.robot.services {
            if official_services.contains(name.as_str()) {
                continue;
            }
            required.push(RequiredRuntime {
                identity: name.clone(),
                binary_name: name.clone(),
                kind: RequiredRuntimeKind::UserService,
                config: service.config.clone(),
            });
        }

        let mut seen_driver_ids = BTreeSet::new();
        for (instance, component) in &self.robot.robot.components {
            if component.driver.is_none() {
                continue;
            }
            // The policy gates the required set: an instance the run excludes
            // (drivers off, or not named in a `--driver` subset) does not pull
            // its driver binary into resolution/inspection. A driver binary is
            // still required if any other instance of the same component id is
            // selected.
            if !drivers.includes_instance(instance) {
                continue;
            }
            if !seen_driver_ids.insert(component.component.clone()) {
                continue;
            }
            required.push(RequiredRuntime {
                identity: component.component.clone(),
                binary_name: official_binary_name(
                    ArtifactKind::ComponentDriver,
                    &component.component,
                ),
                kind: RequiredRuntimeKind::ComponentDriver,
                config: None,
            });
        }

        required
    }

    /// Resolve one required runtime to its canonical binary under `bin/`. A
    /// missing binary fails startup with a precise error naming the required
    /// identity. Extra unreferenced files in `bin/` are never looked up here,
    /// so they are ignored and never launched.
    pub fn resolve_binary(&self, required: &RequiredRuntime) -> Result<PathBuf> {
        let path = self.bin_dir().join(&required.binary_name);
        if !path.is_file() {
            bail!(
                "staged runtime layout {} is missing the binary for required runtime `{}` \
                 (expected bin/{}); re-stage the source project or rebuild the bundle",
                self.root.display(),
                required.identity,
                required.binary_name
            );
        }
        Ok(path)
    }

    /// Resolve and inspect a required runtime without executing it, against the
    /// host architecture. A foreign-architecture binary fails here with a
    /// precise diagnostic rather than crashing later with an exec-format error.
    pub fn inspect(&self, required: &RequiredRuntime) -> Result<SelectedBinary> {
        self.inspect_for(required, LayoutInspection::Host)
    }

    /// Resolve and inspect a required runtime without executing it, against the
    /// architecture `inspection` selects: the host for an in-place run/start, or
    /// a declared `--target` for a cross build (#936). Reads the binary's
    /// embedded participant metadata straight from the object file; never
    /// executes it.
    pub fn inspect_for(
        &self,
        required: &RequiredRuntime,
        inspection: LayoutInspection,
    ) -> Result<SelectedBinary> {
        let path = self.resolve_binary(required)?;
        let meta = inspect_selected_binary_for_target(&path, &inspection.expected_target())
            .with_context(|| {
                format!(
                    "failed to inspect required runtime `{}` at {}",
                    required.identity,
                    path.display()
                )
            })?;
        Ok(SelectedBinary { path, meta })
    }
}

/// The short (kind-stripped) name of one catalog official, e.g.
/// `phoxal/service-drive` -> `drive`, `phoxal/tool-bus` -> `bus`,
/// `phoxal/infrastructure-router` -> `router`.
fn official_short_name(official: &OfficialRuntime) -> String {
    let prefix = match official.kind {
        ArtifactKind::Service => "phoxal/service-",
        ArtifactKind::Tool => "phoxal/tool-",
        ArtifactKind::Simulator => "phoxal/simulator-",
        ArtifactKind::Infrastructure => "phoxal/infrastructure-",
        ArtifactKind::ComponentAssets | ArtifactKind::ComponentDriver => "phoxal/component-",
    };
    official
        .package
        .strip_prefix(prefix)
        .unwrap_or(official.package)
        .to_string()
}

/// The short names of every official service in the CLI catalog, so the loader
/// can tell an official-service config entry from a user service in the
/// compiled `robot.yaml` `services` map.
fn official_service_short_names() -> BTreeSet<&'static str> {
    catalog::NATIVE
        .iter()
        .filter(|official| official.kind == ArtifactKind::Service)
        .map(|official| {
            official
                .package
                .strip_prefix("phoxal/service-")
                .unwrap_or(official.package)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::check::participant_metadata::host_architecture;

    const ROBOT_YAML: &str = r#"schema: robot/v0
robot:
  id: robot_v1
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: model/structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components:
    left_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
    right_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB1
          baud: 115200
    caster:
      component: caster
      mount_link: base
services:
  mission:
    config:
      speed: 1
  drive:
    config:
      gain: 2
"#;

    /// Synthesize an object file of a given architecture carrying the phoxal
    /// metadata section, so the loader is exercised against real object shapes
    /// without building a binary.
    fn synthesize_binary(arch: object::Architecture, payload: &[u8]) -> Vec<u8> {
        use object::write::Object;
        let mut obj = Object::new(object::BinaryFormat::Elf, arch, object::Endianness::Little);
        let section = obj.add_section(
            Vec::new(),
            b".phoxal_api_meta".to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        obj.append_section_data(section, payload, 1);
        obj.write().expect("synthesize object file")
    }

    fn write_layout(robot_yaml: &str) -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join(ROBOT_FILE), robot_yaml)?;
        fs::create_dir_all(dir.path().join(BIN_DIR))?;
        Ok(dir)
    }

    fn write_bin(layout: &Path, name: &str, bytes: &[u8]) -> Result<()> {
        fs::write(layout.join(BIN_DIR).join(name), bytes)?;
        Ok(())
    }

    #[test]
    fn native_required_set_derives_officials_users_and_deduped_drivers() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        let layout = RuntimeLayout::open(dir.path())?;
        let required = layout.required_runtimes(RuntimeProfile::Native, &DriverSelection::All);

        // No simulator binaries in the Native profile.
        assert!(
            !required
                .iter()
                .any(|runtime| runtime.kind == RequiredRuntimeKind::Simulator),
            "native profile must exclude simulator binaries"
        );
        // Every catalog service is required, each under its canonical bin name.
        let drive = required
            .iter()
            .find(|runtime| runtime.identity == "drive")
            .expect("official service `drive` is required");
        assert_eq!(drive.binary_name, "phoxal-service-drive");
        assert_eq!(drive.kind, RequiredRuntimeKind::OfficialService);
        assert_eq!(drive.config, Some(serde_json::json!({"gain": 2})));

        // The user service is required under its identity, and is NOT confused
        // with an official service.
        let mission = required
            .iter()
            .find(|runtime| runtime.identity == "mission")
            .expect("user service `mission` is required");
        assert_eq!(mission.binary_name, "mission");
        assert_eq!(mission.kind, RequiredRuntimeKind::UserService);
        assert_eq!(mission.config, Some(serde_json::json!({"speed": 1})));

        // One driver binary for the two `ddsm115` instances; the driverless
        // `caster` component contributes none.
        let drivers = required
            .iter()
            .filter(|runtime| runtime.kind == RequiredRuntimeKind::ComponentDriver)
            .collect::<Vec<_>>();
        assert_eq!(drivers.len(), 1, "one driver binary serves both instances");
        assert_eq!(drivers[0].identity, "ddsm115");
        assert_eq!(drivers[0].binary_name, "phoxal-component-ddsm115");
        Ok(())
    }

    #[test]
    fn webots_profile_adds_simulator_runtimes() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        let layout = RuntimeLayout::open(dir.path())?;
        let native = layout
            .required_runtimes(RuntimeProfile::Native, &DriverSelection::All)
            .len();
        let webots = layout.required_runtimes(RuntimeProfile::Webots, &DriverSelection::All);
        assert!(webots.len() > native);
        assert!(
            webots
                .iter()
                .any(|runtime| runtime.kind == RequiredRuntimeKind::Simulator),
            "webots profile must add simulator runtimes"
        );
        Ok(())
    }

    #[test]
    fn resolves_selected_binaries_and_ignores_extra_files() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        let host_binary = synthesize_binary(host_architecture(), b"{}");
        write_bin(dir.path(), "mission", &host_binary)?;
        // An unreferenced extra file in bin/ must never be looked up.
        write_bin(dir.path(), "leftover-tool", b"not even an object file")?;

        let layout = RuntimeLayout::open(dir.path())?;
        let required = layout.required_runtimes(RuntimeProfile::Native, &DriverSelection::All);
        let mission = required
            .iter()
            .find(|runtime| runtime.identity == "mission")
            .expect("mission required");
        let path = layout.resolve_binary(mission)?;
        assert_eq!(path, dir.path().join("bin/mission"));
        // Inspecting the extra file is never attempted; it stays inert.
        assert!(dir.path().join("bin/leftover-tool").is_file());
        Ok(())
    }

    #[test]
    fn a_missing_selected_binary_fails_naming_the_identity() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        let layout = RuntimeLayout::open(dir.path())?;
        let required = layout.required_runtimes(RuntimeProfile::Native, &DriverSelection::All);
        let drive = required
            .iter()
            .find(|runtime| runtime.identity == "drive")
            .expect("drive required");
        let error = layout.resolve_binary(drive).expect_err("missing binary");
        let message = error.to_string();
        assert!(message.contains("drive"), "{message}");
        assert!(message.contains("phoxal-service-drive"), "{message}");
        Ok(())
    }

    #[test]
    fn inspecting_a_foreign_arch_binary_is_rejected_precisely() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        let foreign = if host_architecture() == object::Architecture::X86_64 {
            object::Architecture::Aarch64
        } else {
            object::Architecture::X86_64
        };
        write_bin(dir.path(), "mission", &synthesize_binary(foreign, b"{}"))?;

        let layout = RuntimeLayout::open(dir.path())?;
        let required = layout.required_runtimes(RuntimeProfile::Native, &DriverSelection::All);
        let mission = required
            .iter()
            .find(|runtime| runtime.identity == "mission")
            .expect("mission required");
        let error = layout.inspect(mission).expect_err("foreign arch rejected");
        let message = format!("{error:#}");
        assert!(message.contains("mission"), "{message}");
        assert!(message.contains("built for"), "{message}");
        Ok(())
    }

    #[test]
    fn inspecting_a_host_binary_returns_its_embedded_metadata() -> Result<()> {
        let payload = br#"{"participant_api":"Api","contracts":[{"role":"publish","version":"v0.1","contract":"drive::Target","external":false}],"config_schema":{"type":"null"}}"#;
        let dir = write_layout(ROBOT_YAML)?;
        write_bin(
            dir.path(),
            "mission",
            &synthesize_binary(host_architecture(), payload),
        )?;
        let layout = RuntimeLayout::open(dir.path())?;
        let required = layout.required_runtimes(RuntimeProfile::Native, &DriverSelection::All);
        let mission = required
            .iter()
            .find(|runtime| runtime.identity == "mission")
            .expect("mission required");
        let selected = layout.inspect(mission)?;
        assert_eq!(selected.meta.contracts.len(), 1);
        assert_eq!(selected.meta.contracts[0].contract, "drive::Target");
        Ok(())
    }

    #[test]
    fn is_layout_root_distinguishes_a_staged_layout_from_a_bare_dir() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        assert!(RuntimeLayout::is_layout_root(dir.path()));

        let source = tempfile::tempdir()?;
        fs::write(source.path().join(ROBOT_FILE), ROBOT_YAML)?;
        // A source project has robot.yaml but no bin/ store yet.
        assert!(!RuntimeLayout::is_layout_root(source.path()));
        Ok(())
    }
}
