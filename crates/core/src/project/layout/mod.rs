//! The single staged-runtime-layout loader.
//!
//! Execution consumes ONLY a staged runtime layout - `<root>/robot.yaml` (the
//! compiled, flattened `robot/v0` document), a flat `bin/` lookup store, and
//! runtime assets. There is no source loader and no compiled loader: this one
//! loader reads the same layout whether it was staged from a source project
//! into `.phoxal/bundle/` or extracted from a `build.phoxal` bundle
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

use super::catalog::{self, ArtifactKind, OfficialRuntime};
use super::resolver::official_binary_name;
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

/// Which component drivers the run policy keeps in the required set (#936). The
/// `--drivers off` / `--driver <ID>` options on `run` gate the required set at
/// its source: an excluded driver is never required, never resolved from
/// `bin/`, never architecture-inspected, and never planned as a participant.
/// That is what lets a driven robot run on a host whose driver binaries it
/// cannot inspect (`--drivers off` on a bench host missing that target's
/// driver binaries) without the plan constructor hard-failing on a foreign-arch
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
    UserService,
    /// A declared additional user tool (`tools:` in robot.yaml, #950).
    UserTool,
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
    /// `.phoxal/bundle/` directory (run in place) from a source project
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
        // Declaration invariants are re-checked here, not only at source
        // resolution: an extracted bundle is untrusted input, and a hand-edited
        // `tools:`/`services:` map naming an official identity or a dual name
        // must fail before the required set is derived (#950).
        let robot = RobotDocument::parse_from_dir(root)
            .with_context(|| {
                format!(
                    "failed to parse compiled robot.yaml in staged runtime layout {}",
                    root.display()
                )
            })?
            .into_v0();
        validate_runtime_declarations(&robot).with_context(|| {
            format!(
                "compiled robot.yaml in staged runtime layout {} declares an invalid runtime set",
                root.display()
            )
        })?;
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
    /// service; the user-tool set is every `tools` entry (#950); component
    /// drivers are one per driven component id (a single driver binary serves
    /// every instance of that component id), gated by `drivers`: a driver whose
    /// every instance is excluded is not required, so it is never resolved or
    /// inspected (#936).
    #[must_use]
    pub fn required_runtimes(&self, drivers: &DriverSelection) -> Vec<RequiredRuntime> {
        let mut required = Vec::new();
        let official_services = official_service_short_names();

        // Native official set only: the layout constructor serves `run`/
        // `start`/`build`, which exclude simulator binaries. Simulation still
        // constructs its plan on the legacy resolved-robot path; its layout
        // swap is #931, which reintroduces the webots profile WITH its real
        // consumer.
        for official in catalog::for_webots(false) {
            let short = official_short_name(official);
            let kind = match official.kind {
                ArtifactKind::Service => RequiredRuntimeKind::OfficialService,
                ArtifactKind::Tool => RequiredRuntimeKind::OfficialTool,
                ArtifactKind::Infrastructure => RequiredRuntimeKind::Infrastructure,
                // The catalog's native set carries no simulators; the webots
                // additions are not requested here (see the doc above), so a
                // simulator entry can never appear. Guard structurally anyway.
                ArtifactKind::Simulator => continue,
                // The catalog carries no component packages; assets/drivers are
                // never members of the official runtime set.
                ArtifactKind::ComponentAssets | ArtifactKind::ComponentDriver => continue,
            };
            // Official runtimes take no configuration from robot.yaml (#950):
            // the declaration maps are user-only, and `open` rejected any
            // official identity declared in them.
            required.push(RequiredRuntime {
                identity: short.clone(),
                binary_name: official_binary_name(official.kind, &short),
                kind,
                config: None,
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

        // The tools declaration (#950): each declared additional user tool is
        // required under its own identity, exactly like a user service.
        // Resolution already rejects official identities in this map.
        for (name, tool) in &self.robot.tools {
            required.push(RequiredRuntime {
                identity: name.clone(),
                binary_name: name.clone(),
                kind: RequiredRuntimeKind::UserTool,
                config: tool.config.clone(),
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
    /// architecture `inspection` selects: the host for an in-place run/start, or
    /// a declared `--target` for a cross build (#936). Reads the binary's
    /// embedded participant metadata straight from the object file; never
    /// executes it.
    ///
    /// The selected binary's own declared `meta.id` is checked against
    /// `required.identity` before the caller ever sees its config schema
    /// (organization#957 review): `required.identity` is the canonical
    /// short/component id `required_runtimes` derived from the compiled
    /// `robot.yaml` plus the CLI catalog - the official short name, the user
    /// service/tool name, or the component id shared by every driven
    /// instance - which is exactly the identity a matching binary's own
    /// `#[phoxal::service]`/`driver`/`tool` attribute declares. A mismatch
    /// here means the wrong binary landed at this canonical `bin/` path (a
    /// stale rebuild, a hand-edited bundle, two artifacts swapped on disk),
    /// and must fail before that binary's schema is used to validate
    /// anything.
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
        if meta.id != required.identity {
            bail!(
                "staged runtime layout {} binary bin/{} at {} declares participant id `{}`, but \
                 required runtime `{}` expects `{}`; the wrong binary is staged at this canonical \
                 path",
                self.root.display(),
                required.binary_name,
                path.display(),
                meta.id,
                required.identity,
                required.identity,
            );
        }
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

/// Validate the runtime declaration maps of a `robot/v0` document against the
/// CLI catalog (#950), shared by source resolution (which runs it FIRST, before
/// any workspace scanning) and by [`RuntimeLayout::open`] (so a hand-edited
/// bundle cannot smuggle a forbidden declaration past the loader):
///
/// - a name may be declared under `services:` or `tools:`, never both (one
///   binary namespace);
/// - official identities are never declared - official runtimes are
///   catalog-owned, always run, and take no configuration from robot.yaml. A
///   workspace crate overriding an official identity does so WITHOUT a
///   declaration.
pub fn validate_runtime_declarations(robot: &phoxal::model::robot::v0::Robot) -> Result<()> {
    // Officials share ONE binary namespace across services and tools, so a
    // declared name is checked against the WHOLE reserved catalog set, not just
    // the map it appears in - `tools.drive` (drive is an official service) and
    // `services.log` (log is an official tool) are both rejected (#950).
    let official_kind = |name: &str| -> Option<&'static str> {
        if official_service_short_names().contains(name) {
            Some("service")
        } else if official_tool_short_names().contains(name) {
            Some("tool")
        } else {
            None
        }
    };
    let declared = robot
        .services
        .keys()
        .map(|name| ("services", name))
        .chain(robot.tools.keys().map(|name| ("tools", name)));
    for (map, name) in declared {
        if map == "services" && robot.tools.contains_key(name) {
            bail!(
                "robot.yaml declares '{name}' under both services and tools; the two maps \
                 share one binary namespace, so a name may appear in only one"
            );
        }
        if let Some(kind) = official_kind(name) {
            bail!(
                "robot.yaml declares {map}.{name}, but '{name}' is an official {kind}; \
                 official runtimes are catalog-owned, always run, and take no robot.yaml \
                 declaration (a workspace crate matching an official identity overrides its \
                 binary without being declared)"
            );
        }
    }
    Ok(())
}

/// The short names of every official tool in the CLI catalog.
fn official_tool_short_names() -> BTreeSet<&'static str> {
    catalog::NATIVE
        .iter()
        .filter(|official| official.kind == ArtifactKind::Tool)
        .map(|official| {
            official
                .package
                .strip_prefix("phoxal/tool-")
                .unwrap_or(official.package)
        })
        .collect()
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
    #[test]
    fn official_identities_are_rejected_in_either_map_across_namespaces() -> anyhow::Result<()> {
        use phoxal::model::robot::v0::{Robot as RobotV0, UserService, UserTool};

        let base = || -> RobotV0 {
            RobotDocument::parse_from_string(
                r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#,
            )
            .expect("minimal robot parses")
            .into_v0()
        };

        // `tools.drive` - drive is an official SERVICE, rejected in the tools map.
        let mut robot = base();
        robot
            .tools
            .insert("drive".to_string(), UserTool { config: None });
        let error = super::validate_runtime_declarations(&robot)
            .expect_err("an official service name in tools: is rejected")
            .to_string();
        assert!(error.contains("official service"), "{error}");

        // `services.log` - log is an official TOOL, rejected in the services map.
        let mut robot = base();
        robot
            .services
            .insert("log".to_string(), UserService { config: None });
        let error = super::validate_runtime_declarations(&robot)
            .expect_err("an official tool name in services: is rejected")
            .to_string();
        assert!(error.contains("official tool"), "{error}");

        // A non-official user name in either map is accepted.
        let mut robot = base();
        robot
            .tools
            .insert("lidar-viz".to_string(), UserTool { config: None });
        super::validate_runtime_declarations(&robot).expect("a user tool name is accepted");
        Ok(())
    }

    use super::*;
    use std::fs;

    use crate::check::participant_metadata::host_architecture;

    const ROBOT_YAML: &str = r#"schema: robot/v0
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
tools:
  lidar-viz:
    config:
      port: 9000
"#;

    /// Synthesize an object file of a given architecture carrying the phoxal
    /// metadata section, so the loader is exercised against real object shapes
    /// without building a binary.
    fn synthesize_binary(arch: object::Architecture, payload: &[u8]) -> Vec<u8> {
        use object::write::Object;
        let format = crate::check::participant_metadata::host_binary_format();
        let (segment, name): (&[u8], &[u8]) = match format {
            object::BinaryFormat::MachO => (b"__DATA", b"__phoxal_meta"),
            _ => (b"", b".phoxal_meta"),
        };
        let mut obj = Object::new(format, arch, object::Endianness::Little);
        let section = obj.add_section(
            segment.to_vec(),
            name.to_vec(),
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
        let required = layout.required_runtimes(&DriverSelection::All);

        // No simulator binaries in the required set (the kind vocabulary has
        // no simulator member until #931 adds the simulation consumer).
        assert!(
            !required
                .iter()
                .any(|runtime| runtime.identity.contains("webots")),
            "the run required set must exclude simulator binaries"
        );
        // Every catalog service is required, each under its canonical bin
        // name, and officials take NO configuration from robot.yaml (#950).
        let drive = required
            .iter()
            .find(|runtime| runtime.identity == "drive")
            .expect("official service `drive` is required");
        assert_eq!(drive.binary_name, "phoxal-service-drive");
        assert_eq!(drive.kind, RequiredRuntimeKind::OfficialService);
        assert_eq!(drive.config, None);

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
    fn a_hand_edited_bundle_declaring_official_or_dual_names_is_rejected() -> Result<()> {
        // The loader re-validates declarations (#950): an extracted bundle is
        // untrusted input.
        let official = ROBOT_YAML.replace(
            "tools:\n  lidar-viz:\n    config:\n      port: 9000\n",
            "tools:\n  log: {}\n",
        );
        let dir = write_layout(&official)?;
        let error = RuntimeLayout::open(dir.path())
            .expect_err("an official identity in tools: must be rejected")
            .to_string();
        assert!(error.contains("invalid runtime set"), "{error}");

        let dual = ROBOT_YAML.replace(
            "tools:\n  lidar-viz:\n    config:\n      port: 9000\n",
            "tools:\n  mission: {}\n",
        );
        let dir = write_layout(&dual)?;
        let error = RuntimeLayout::open(dir.path())
            .expect_err("a dual services/tools name must be rejected")
            .to_string();
        assert!(error.contains("invalid runtime set"), "{error}");
        Ok(())
    }

    #[test]
    fn declared_user_tools_are_required_under_their_identity() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        let layout = RuntimeLayout::open(dir.path())?;
        let required = layout.required_runtimes(&DriverSelection::All);
        let tool = required
            .iter()
            .find(|runtime| runtime.identity == "lidar-viz")
            .expect("declared user tool is required (#950)");
        assert_eq!(tool.kind, RequiredRuntimeKind::UserTool);
        assert_eq!(tool.binary_name, "lidar-viz");
        assert_eq!(
            tool.config
                .as_ref()
                .and_then(|config| config.get("port"))
                .and_then(serde_json::Value::as_u64),
            Some(9000)
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
        let required = layout.required_runtimes(&DriverSelection::All);
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
        let required = layout.required_runtimes(&DriverSelection::All);
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
        let required = layout.required_runtimes(&DriverSelection::All);
        let mission = required
            .iter()
            .find(|runtime| runtime.identity == "mission")
            .expect("mission required");
        let error = layout
            .inspect_for(mission, LayoutInspection::Host)
            .expect_err("foreign arch rejected");
        let message = format!("{error:#}");
        assert!(message.contains("mission"), "{message}");
        assert!(message.contains("built for"), "{message}");
        Ok(())
    }

    #[test]
    fn inspecting_a_host_binary_returns_its_embedded_metadata() -> Result<()> {
        let payload = br#"{"id":"mission","config_schema":{"type":"null"}}"#;
        let dir = write_layout(ROBOT_YAML)?;
        write_bin(
            dir.path(),
            "mission",
            &synthesize_binary(host_architecture(), payload),
        )?;
        let layout = RuntimeLayout::open(dir.path())?;
        let required = layout.required_runtimes(&DriverSelection::All);
        let mission = required
            .iter()
            .find(|runtime| runtime.identity == "mission")
            .expect("mission required");
        let selected = layout.inspect_for(mission, LayoutInspection::Host)?;
        assert_eq!(selected.meta.id, "mission");
        assert_eq!(
            selected.meta.config_schema,
            serde_json::json!({"type": "null"})
        );
        Ok(())
    }

    /// The regression the review caught (organization#957): a binary declaring
    /// the WRONG participant id for the canonical `bin/` path it landed at must
    /// fail inspection, naming both the declared and the expected identity -
    /// not silently pass with its schema paired to the wrong runtime.
    #[test]
    fn inspecting_a_binary_declaring_the_wrong_id_is_rejected() -> Result<()> {
        let payload = br#"{"id":"drive","config_schema":{"type":"null"}}"#;
        let dir = write_layout(ROBOT_YAML)?;
        write_bin(
            dir.path(),
            "mission",
            &synthesize_binary(host_architecture(), payload),
        )?;
        let layout = RuntimeLayout::open(dir.path())?;
        let required = layout.required_runtimes(&DriverSelection::All);
        let mission = required
            .iter()
            .find(|runtime| runtime.identity == "mission")
            .expect("mission required");
        let error = layout
            .inspect_for(mission, LayoutInspection::Host)
            .expect_err("a binary declaring a mismatched id must be rejected");
        let message = error.to_string();
        assert!(message.contains("mission"), "{message}");
        assert!(message.contains("drive"), "{message}");
        Ok(())
    }

    /// A binary whose metadata section carries garbage instead of the
    /// `{id, config_schema}` JSON shape must fail inspection with a clear
    /// parse error, not synthesize a placeholder identity. Missing-section
    /// coverage lives with the extractor itself:
    /// `participant_metadata::tests::foreign_object_without_section_is_a_clear_error`
    /// (organization#957 review).
    #[test]
    fn inspecting_a_binary_with_malformed_metadata_is_rejected() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        write_bin(
            dir.path(),
            "mission",
            &synthesize_binary(host_architecture(), b"not phoxal metadata"),
        )?;
        let layout = RuntimeLayout::open(dir.path())?;
        let required = layout.required_runtimes(&DriverSelection::All);
        let mission = required
            .iter()
            .find(|runtime| runtime.identity == "mission")
            .expect("mission required");
        let error = layout
            .inspect_for(mission, LayoutInspection::Host)
            .expect_err("a binary with malformed metadata must be rejected");
        let message = format!("{error:#}");
        assert!(message.contains("not valid JSON"), "{message}");
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
