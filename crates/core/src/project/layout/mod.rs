//! The single staged-runtime-layout loader.
//!
//! Execution consumes only the canonical staged bundle: `robot.json`,
//! `assets/`, and `bin/`. Authored YAML, component documents, simulation
//! documents, and URDF never enter this loader.
//!
//! The loader derives the required runtime set from two authorities - the
//! CLI-internal official catalog ([`super::catalog`]) and the compiler-owned
//! participant declarations beside `robot.json` -
//! resolves every required runtime to exactly one canonical binary under
//! `bin/`, and inspects only the selected binaries (host-architecture
//! compatibility plus embedded metadata) without ever executing them.
//! Unreferenced extra files in `bin/` are ignored and never inspected.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use phoxal_manifest::{Participant, ParticipantKind};

use super::catalog::{self, ArtifactKind, OfficialRuntime};
use super::resolver::official_binary_name;
use crate::check::participant_metadata::{
    ExpectedTarget, ParticipantMeta, expected_target_for_host, inspect_selected_binary_for_target,
};

pub mod plan;

pub use plan::PlanOptions;

pub const ROBOT_FILE: &str = "robot.json";
pub const ASSETS_DIR: &str = "assets";
pub const PARTICIPANTS_ASSET: &str = "participants.json";
pub const ROUTER_CONFIG_ASSET: &str = "router/config.json5";
pub const RUNTIME_HEADER_ASSET: &str = "runtime.json";
pub const PARTICIPANTS_PATH: &str = "assets/participants.json";
pub const ROUTER_CONFIG_PATH: &str = "assets/router/config.json5";
pub const RUNTIME_HEADER_PATH: &str = "assets/runtime.json";
const PARTICIPANTS_SCHEMA: &str = "phoxal/participants/v0";
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
/// runtimes the CLI supervisor launches (services and drivers) versus which
/// the simulator owns (simulators). The comms router is not here at all: it
/// runs inside the supervisor process and has no staged binary
/// (organization#978).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredRuntimeKind {
    /// The one mandatory root brain. Derived from the bundle-format invariant
    /// itself, not from authored `robot.yaml` or Cargo source: every staged
    /// layout carries exactly one `bin/brain` (organization#973).
    Brain,
    OfficialService,
    UserService,
    ComponentDriver,
}

/// One runtime the compiled layout requires, with the canonical `bin/` file
/// name the staging step wrote it under and any compiler-owned config.
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
    /// Config carried by the compiler-owned participant declaration. `None`
    /// for a runtime with no authored config.
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
/// derived. Holds no source and no Cargo graph - everything it needs already
/// materialized into `bin/` before this type is constructed.
#[derive(Debug, Clone)]
pub struct RuntimeLayout {
    root: PathBuf,
    robot: phoxal_model::Robot,
    participants: Vec<Participant>,
}

impl RuntimeLayout {
    /// Whether `root` is shaped like a staged runtime layout - canonical
    /// `robot.json`, `assets/`, and `bin/`. Cheap existence checks only, used by
    /// universal `run` root classification to tell an extracted bundle / staged
    /// `.phoxal/bundle/` directory (run in place) from a source project
    /// (staged first).
    #[must_use]
    pub fn is_layout_root(root: &Path) -> bool {
        root.join(ROBOT_FILE).is_file()
            && root.join(ASSETS_DIR).is_dir()
            && root.join(BIN_DIR).is_dir()
    }

    /// Strictly decode the canonical robot and compiled participant
    /// declarations. No source-format parser is available on this path.
    pub fn open(root: &Path) -> Result<Self> {
        let robot_path = root.join(ROBOT_FILE);
        let robot = phoxal_model::Robot::decode(
            &std::fs::read(&robot_path)
                .with_context(|| format!("failed to read {}", robot_path.display()))?,
        )
        .with_context(|| format!("failed to decode canonical {}", robot_path.display()))?;
        validate_model_assets(root, &robot)?;
        let participants = decode_participants(&root.join(ASSETS_DIR).join(PARTICIPANTS_ASSET))?;
        validate_participant_model_membership(&robot, &participants)?;
        Ok(Self {
            root: root.to_path_buf(),
            robot,
            participants,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn robot(&self) -> &phoxal_model::Robot {
        &self.robot
    }

    #[must_use]
    pub fn participants(&self) -> &[Participant] {
        &self.participants
    }

    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join(BIN_DIR)
    }

    /// The runtimes the compiled layout requires. The official set comes from
    /// the CLI-internal catalog (simulator binaries excluded from `Native`);
    /// the user-service set is every `services` entry that is not an official
    /// service; component
    /// drivers are one per driven component id (a single driver binary serves
    /// every instance of that component id), gated by `drivers`: a driver whose
    /// every instance is excluded is not required, so it is never resolved or
    /// inspected (#936).
    #[must_use]
    pub fn required_runtimes(&self, drivers: &DriverSelection) -> Vec<RequiredRuntime> {
        let mut required = Vec::new();
        let official_services = official_service_short_names();

        // The mandatory root brain, always first and always exactly once. It
        // takes no config: `#[phoxal::brain]` fixes `Config = ()`, so there is
        // no brain config side channel to carry here.
        required.push(RequiredRuntime {
            identity: crate::check::source::BRAIN_ID.to_string(),
            binary_name: crate::check::source::BRAIN_ID.to_string(),
            kind: RequiredRuntimeKind::Brain,
            config: None,
        });

        // Native official set only: the layout constructor serves `run`/
        // `start`/`build`, which exclude simulator binaries. Simulation still
        // constructs its plan on the legacy resolved-robot path; its layout
        // swap is #931, which reintroduces the webots profile WITH its real
        // consumer.
        for official in catalog::for_webots(false) {
            let short = official_short_name(official);
            let kind = match official.kind {
                ArtifactKind::Service => RequiredRuntimeKind::OfficialService,
                // The catalog's native set carries no simulators; the webots
                // additions are not requested here (see the doc above), so a
                // simulator entry can never appear. Guard structurally anyway.
                ArtifactKind::Simulator => continue,
                // The catalog carries no component drivers.
                ArtifactKind::ComponentDriver => continue,
            };
            // Official runtimes take no authored service-map configuration and
            // no compiler-owned configuration either.
            let config = None;
            required.push(RequiredRuntime {
                identity: short.clone(),
                binary_name: official_binary_name(official.kind, &short),
                kind,
                config,
            });
        }

        let mut seen_driver_ids = BTreeSet::new();
        for participant in &self.participants {
            match participant.kind {
                ParticipantKind::Service
                    if !official_services.contains(participant.id.as_str()) =>
                {
                    required.push(RequiredRuntime {
                        identity: participant.id.clone(),
                        binary_name: participant.id.clone(),
                        kind: RequiredRuntimeKind::UserService,
                        config: participant.config.clone(),
                    });
                }
                ParticipantKind::Driver => {
                    let Some(instance) = participant.component_instance.as_deref() else {
                        continue;
                    };
                    if !drivers.includes_instance(instance)
                        || !seen_driver_ids.insert(participant.id.clone())
                    {
                        continue;
                    }
                    required.push(RequiredRuntime {
                        identity: participant.id.clone(),
                        binary_name: official_binary_name(
                            ArtifactKind::ComponentDriver,
                            &participant.id,
                        ),
                        kind: RequiredRuntimeKind::ComponentDriver,
                        // Driver configuration is instance-scoped. The shared
                        // required binary cannot carry one representative
                        // instance's config; plan construction joins each
                        // driver declaration back to its own launch record.
                        config: None,
                    });
                }
                ParticipantKind::Simulator | ParticipantKind::Service => {}
            }
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
    /// before the schema is trusted. `required.identity` is the canonical
    /// short/component id `required_runtimes` derived from the compiled
    /// participant declarations plus the CLI catalog - the official short name, the user
    /// service name, or the component id shared by every driven
    /// instance - which is exactly the identity a matching binary's own
    /// `#[phoxal::service]`/`driver` attribute declares. A mismatch
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
        let expected_kind = match required.kind {
            RequiredRuntimeKind::Brain => phoxal_runtime_contract::ParticipantKind::Brain,
            RequiredRuntimeKind::OfficialService | RequiredRuntimeKind::UserService => {
                phoxal_runtime_contract::ParticipantKind::Service
            }
            RequiredRuntimeKind::ComponentDriver => {
                phoxal_runtime_contract::ParticipantKind::Driver
            }
        };
        ensure!(
            meta.kind == expected_kind,
            "staged runtime layout {} binary bin/{} declares participant kind {:?}, but required \
             runtime `{}` expects {:?}; the wrong participant kind is staged at this canonical path",
            self.root.display(),
            required.binary_name,
            meta.kind,
            required.identity,
            expected_kind,
        );
        Ok(SelectedBinary { path, meta })
    }
}

/// Validate every canonical model asset against the staged asset root. This is
/// repeated while opening an extracted or hand-provided bundle; candidate
/// validation alone cannot protect a bundle after publication or transport.
fn validate_model_assets(root: &Path, robot: &phoxal_model::Robot) -> Result<()> {
    let mut referenced = robot.structure().asset_ids().collect::<Vec<_>>();
    for instance in robot.components() {
        let component = robot
            .component_for_instance(instance.id())
            .with_context(|| {
                format!(
                    "canonical robot component instance '{}' has no component definition",
                    instance.id()
                )
            })?;
        referenced.extend(component.structure().asset_ids());
    }
    for id in referenced {
        validate_bundle_asset(root, id.as_str(), "canonical robot asset")?;
    }
    Ok(())
}

fn validate_bundle_asset(root: &Path, relative: &str, label: &str) -> Result<()> {
    let asset_root = root.join(ASSETS_DIR);
    let canonical_root = asset_root.canonicalize().with_context(|| {
        format!(
            "failed to resolve bundle asset root {}",
            asset_root.display()
        )
    })?;
    let path = asset_root.join(relative);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("{label} '{relative}' is missing"))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{label} '{relative}' must be a regular file below assets/"
    );
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {label} '{relative}'"))?;
    ensure!(
        canonical.starts_with(&canonical_root),
        "{label} '{relative}' escapes the bundle asset root"
    );
    Ok(())
}

/// The short (kind-stripped) name of one catalog official, e.g.
/// `phoxal/service-drive` -> `drive`.
fn official_short_name(official: &OfficialRuntime) -> String {
    let prefix = match official.kind {
        ArtifactKind::Service => "phoxal/service-",
        ArtifactKind::Simulator => "phoxal/simulator-",
        ArtifactKind::ComponentDriver => "phoxal/component-",
    };
    official
        .package
        .strip_prefix(prefix)
        .unwrap_or(official.package)
        .to_string()
}

#[derive(serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ParticipantsWire<'a> {
    schema: &'static str,
    participants: &'a [Participant],
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ParticipantsWireOwned {
    schema: String,
    participants: Vec<Participant>,
}

pub fn encode_participants(participants: &[Participant]) -> Result<Vec<u8>> {
    validate_participants(participants)?;
    serde_json::to_vec_pretty(&ParticipantsWire {
        schema: PARTICIPANTS_SCHEMA,
        participants,
    })
    .context("failed to encode compiled participant declarations")
}

pub fn decode_participants(path: &Path) -> Result<Vec<Participant>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read compiled participants {}", path.display()))?;
    let wire: ParticipantsWireOwned = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode compiled participants {}", path.display()))?;
    anyhow::ensure!(
        wire.schema == PARTICIPANTS_SCHEMA,
        "unsupported compiled participant schema '{}'",
        wire.schema
    );
    validate_participants(&wire.participants)?;
    Ok(wire.participants)
}

fn validate_participants(participants: &[Participant]) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut user_namespace = BTreeMap::new();
    let official_names = official_service_short_names();
    for participant in participants {
        // `brain` is reserved across every compiled participant kind, so a
        // malformed or hand-edited participants.json cannot create a user
        // runtime that would overwrite the mandatory `bin/brain`
        // (organization#973).
        anyhow::ensure!(
            participant.id != crate::check::source::BRAIN_ID,
            "compiled participant '{}' claims the reserved root-brain identity; the brain is the \
             root Cargo package's binary and is never a compiled participant declaration",
            participant.id
        );
        let kind = match participant.kind {
            ParticipantKind::Service => "service",
            ParticipantKind::Driver => "driver",
            ParticipantKind::Simulator => "simulator",
        };
        anyhow::ensure!(
            seen.insert((
                kind,
                participant.id.as_str(),
                participant.component_instance.as_deref()
            )),
            "duplicate compiled participant declaration '{}'",
            participant.id
        );
        match participant.kind {
            ParticipantKind::Service => {
                anyhow::ensure!(
                    participant.component_instance.is_none(),
                    "compiled {kind} participant '{}' must not name a component instance",
                    participant.id
                );
                anyhow::ensure!(
                    !official_names.contains(participant.id.as_str()),
                    "compiled {kind} participant '{}' collides with a catalog-owned runtime",
                    participant.id
                );
                if let Some(previous) = user_namespace.insert(participant.id.as_str(), kind) {
                    anyhow::ensure!(
                        previous == kind,
                        "compiled participant '{}' is declared as both {previous} and {kind}",
                        participant.id
                    );
                }
            }
            ParticipantKind::Driver | ParticipantKind::Simulator => {
                anyhow::ensure!(
                    participant.component_instance.is_some(),
                    "compiled {kind} participant '{}' must name a component instance",
                    participant.id
                );
                if participant.kind == ParticipantKind::Driver {
                    let config = participant.config.clone().with_context(|| {
                        format!(
                            "compiled driver participant '{}' must carry typed config",
                            participant.id
                        )
                    })?;
                    serde_json::from_value::<phoxal_manifest::source::robot::v0::DriverConfig>(
                        config,
                    )
                    .with_context(|| {
                        format!(
                            "compiled driver participant '{}' has invalid typed config",
                            participant.id
                        )
                    })?;
                }
            }
        }
    }
    Ok(())
}

fn validate_participant_model_membership(
    robot: &phoxal_model::Robot,
    participants: &[Participant],
) -> Result<()> {
    for participant in participants {
        if !matches!(
            participant.kind,
            ParticipantKind::Driver | ParticipantKind::Simulator
        ) {
            continue;
        }
        let instance_id = participant
            .component_instance
            .as_deref()
            .context("compiled component participant has no component instance")?;
        let instance = robot
            .components()
            .find(|instance| instance.id() == instance_id)
            .with_context(|| {
                format!(
                    "compiled {:?} participant '{}' names unknown component instance '{}'",
                    participant.kind, participant.id, instance_id
                )
            })?;
        ensure!(
            instance.component_type() == participant.id,
            "compiled {:?} participant '{}' does not match component instance '{}' of type '{}'",
            participant.kind,
            participant.id,
            instance_id,
            instance.component_type()
        );
    }
    Ok(())
}

/// Validate the runtime declaration maps of a `robot/v0` document against the
/// CLI catalog (#950), shared by source resolution (which runs it FIRST, before
/// any workspace scanning) and by [`RuntimeLayout::open`] (so a hand-edited
/// bundle cannot smuggle a forbidden declaration past the loader):
///
/// - `brain` is never declared - the mandatory root brain IS the root Cargo
///   package's binary, discovered from Cargo metadata and staged as
///   `bin/brain` (organization#973). This is the single owner of the
///   actionable authored-name rejection on every CLI validation/resolve path;
///   `phoxal-manifest`'s own reservation stays as defense for direct compiler
///   consumers outside the CLI.
/// - official identities are never declared - official runtimes are
///   catalog-owned, always run, and take no authored configuration. A
///   workspace crate overriding an official identity does so WITHOUT a
///   declaration.
pub fn validate_runtime_declarations(
    robot: &phoxal_manifest::source::robot::v0::Manifest,
) -> Result<()> {
    for name in robot.services.keys() {
        if name == crate::check::source::BRAIN_ID {
            bail!(
                "{}",
                phoxal_manifest::source::robot::v0::reserved_brain_id_message("services")
            );
        }
        if official_service_short_names().contains(name.as_str()) {
            bail!(
                "robot.yaml declares services.{name}, but '{name}' is an official service; \
                 official runtimes are catalog-owned, always run, and take no robot.yaml \
                 declaration (a workspace crate matching an official identity overrides its \
                 binary without being declared)"
            );
        }
    }
    Ok(())
}

/// The short names of every official service in the CLI catalog, so the loader
/// can tell an official-service config entry from a user service in the
/// compiler-owned participant declarations.
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
pub(crate) fn write_test_layout(root: &Path, robot_yaml: &str) -> Result<()> {
    let source = tempfile::tempdir()?;
    let parsed = crate::project::resolver::parse_robot_from_string(robot_yaml)?;
    let fixture_yaml = parsed
        .robot
        .components
        .keys()
        .next()
        .filter(|_| robot_yaml.contains("actuators: []"))
        .map_or_else(
            || robot_yaml.to_string(),
            |instance| {
                robot_yaml.replace(
                    "actuators: []",
                    &format!("actuators:\n      - {instance}.motor"),
                )
            },
        );
    let manifest = crate::project::resolver::parse_robot_from_string(&fixture_yaml)?;
    crate::project::resolver::write_robot_to_dir(&manifest, source.path())?;
    let structure_path = source.path().join(&manifest.robot.structure);
    if let Some(parent) = structure_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &structure_path,
        r#"<robot name="fixture"><link name="base_footprint"/><link name="base_link"/><link name="base"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="base_mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
    )?;
    let mut component_roots = std::collections::BTreeMap::new();
    for component_type in manifest.used_component_types() {
        let component_root = source.path().join("components").join(component_type);
        std::fs::create_dir_all(&component_root)?;
        std::fs::write(
            component_root.join("component.yaml"),
            "schema: component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
        )?;
        std::fs::write(
            component_root.join("structure.urdf"),
            r#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
        )?;
        component_roots.insert(component_type.to_string(), component_root);
    }
    let compiled = phoxal_manifest::compile(phoxal_manifest::SourceSet {
        project_root: source.path().to_path_buf(),
        robot_manifest: source.path().join("robot.yaml"),
        component_roots,
    })?;
    let (robot, participants, assets) = compiled.into_parts();
    std::fs::create_dir_all(root.join(ASSETS_DIR))?;
    std::fs::create_dir_all(root.join(BIN_DIR))?;
    std::fs::write(root.join(ROBOT_FILE), robot.encode()?)?;
    std::fs::write(
        root.join(ASSETS_DIR).join(PARTICIPANTS_ASSET),
        encode_participants(&participants.into_vec())?,
    )?;
    for (id, bytes) in assets.into_map() {
        let path = root.join(ASSETS_DIR).join(id.as_str());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn official_identities_are_rejected_in_either_map_across_namespaces() -> anyhow::Result<()> {
        use phoxal_manifest::source::robot::v0::{Manifest as RobotManifest, UserService};

        let base = || -> RobotManifest {
            crate::project::resolver::parse_robot_from_string(
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
        };

        // `services.drive` - drive is an official service, so it may not be
        // declared: officials are catalog-owned, always run, and configless.
        let mut robot = base();
        robot
            .services
            .insert("drive".to_string(), UserService { config: None });
        let error = super::validate_runtime_declarations(&robot)
            .expect_err("an official service name in services: is rejected")
            .to_string();
        assert!(error.contains("official service"), "{error}");

        // `services.brain` is reserved for the mandatory root brain and gets
        // the same actionable shape as official-identity rejection (#973).
        let mut robot = base();
        robot
            .services
            .insert("brain".to_string(), UserService { config: None });
        let error = super::validate_runtime_declarations(&robot)
            .expect_err("the reserved brain identity in services: is rejected")
            .to_string();
        assert!(error.contains("services.brain is reserved"), "{error}");
        assert!(error.contains("#[phoxal::brain]"), "{error}");

        // A non-official user name is accepted.
        let mut robot = base();
        robot
            .services
            .insert("lidar-viz".to_string(), UserService { config: None });
        super::validate_runtime_declarations(&robot).expect("a user service name is accepted");
        Ok(())
    }

    #[test]
    fn bundle_path_constants_match_their_asset_locations() {
        for (asset, path) in [
            (PARTICIPANTS_ASSET, PARTICIPANTS_PATH),
            (ROUTER_CONFIG_ASSET, ROUTER_CONFIG_PATH),
            (RUNTIME_HEADER_ASSET, RUNTIME_HEADER_PATH),
        ] {
            assert_eq!(format!("{ASSETS_DIR}/{asset}"), path);
        }
    }

    #[test]
    fn compiled_official_names_and_the_reserved_brain_identity_are_rejected() {
        let service = |id: &str| phoxal_manifest::Participant {
            id: id.to_string(),
            kind: phoxal_manifest::ParticipantKind::Service,
            component_instance: None,
            config: None,
        };
        let error = encode_participants(&[service("drive")])
            .expect_err("catalog-owned service declarations must fail closed")
            .to_string();
        assert!(error.contains("catalog-owned runtime"), "{error}");
        // A hand-edited or malformed participants.json must not be able to
        // create a user runtime that would overwrite `bin/brain`.
        let error = encode_participants(&[service("brain")])
            .expect_err("the reserved root-brain identity must fail closed")
            .to_string();
        assert!(error.contains("reserved root-brain identity"), "{error}");
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

    fn metadata(id: &str, kind: &str, schema: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": phoxal_runtime_contract::PARTICIPANT_METADATA_SCHEMA,
            "id": id,
            "kind": kind,
            "config_schema": schema,
        }))
        .expect("metadata serializes")
    }

    fn write_layout(robot_yaml: &str) -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        super::write_test_layout(dir.path(), robot_yaml)?;
        Ok(dir)
    }

    fn write_bin(layout: &Path, name: &str, bytes: &[u8]) -> Result<()> {
        fs::write(layout.join(BIN_DIR).join(name), bytes)?;
        Ok(())
    }

    #[test]
    fn component_participants_must_match_a_canonical_model_instance() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        let participants_path = dir.path().join(ASSETS_DIR).join(PARTICIPANTS_ASSET);
        let mut participants = decode_participants(&participants_path)?;
        participants.push(phoxal_manifest::Participant {
            id: "ddsm115".to_string(),
            kind: phoxal_manifest::ParticipantKind::Driver,
            component_instance: Some("unknown_drive".to_string()),
            config: Some(serde_json::json!({
                "connection": {"type": "serial", "port": "/dev/null", "baud": 115200}
            })),
        });
        fs::write(&participants_path, encode_participants(&participants)?)?;
        let error = RuntimeLayout::open(dir.path())
            .expect_err("an unknown compiled component instance must fail")
            .to_string();
        assert!(error.contains("unknown component instance"), "{error}");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_a_canonical_asset_symlink_escape() -> Result<()> {
        let source = tempfile::tempdir()?;
        fs::create_dir_all(source.path().join("meshes"))?;
        let component = source.path().join("components/wheel");
        fs::create_dir_all(&component)?;
        fs::write(
            source.path().join("robot.yaml"),
            r#"schema: robot/v0
robot:
  id: asset-bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators:
      - wheel.motor
    encoders: []
  components:
    wheel:
      component: wheel
      mount_link: base
"#,
        )?;
        fs::write(
            source.path().join("structure.urdf"),
            r#"<robot name="asset-bot"><link name="base_footprint"/><link name="base_link"/><link name="base"><visual><geometry><mesh filename="package://robot/meshes/body.stl"/></geometry></visual></link><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
        )?;
        fs::write(
            source.path().join("meshes/body.stl"),
            b"solid body\nendsolid body\n",
        )?;
        fs::write(
            component.join("component.yaml"),
            "schema: component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
        )?;
        fs::write(
            component.join("structure.urdf"),
            r#"<robot name="wheel"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
        )?;
        let compiled = phoxal_manifest::compile(phoxal_manifest::SourceSet {
            project_root: source.path().to_path_buf(),
            robot_manifest: source.path().join("robot.yaml"),
            component_roots: std::collections::BTreeMap::from([("wheel".to_string(), component)]),
        })?;
        let (robot, participants, assets) = compiled.into_parts();
        let dir = tempfile::tempdir()?;
        fs::create_dir_all(dir.path().join(ASSETS_DIR))?;
        fs::create_dir_all(dir.path().join(BIN_DIR))?;
        fs::write(dir.path().join(ROBOT_FILE), robot.encode()?)?;
        fs::write(
            dir.path().join(ASSETS_DIR).join(PARTICIPANTS_ASSET),
            encode_participants(&participants.into_vec())?,
        )?;
        for (id, bytes) in assets.into_map() {
            let path = dir.path().join(ASSETS_DIR).join(id.as_str());
            fs::create_dir_all(path.parent().context("asset has no parent")?)?;
            fs::write(path, bytes)?;
        }
        let layout = RuntimeLayout::open(dir.path())?;
        let id = layout
            .robot()
            .structure()
            .asset_ids()
            .next()
            .context("fixture canonical robot has no structure asset")?
            .clone();
        let asset = dir.path().join(ASSETS_DIR).join(id.as_str());
        let outside = dir.path().join("outside-asset");
        fs::write(&outside, b"outside")?;
        fs::remove_file(&asset)?;
        std::os::unix::fs::symlink(&outside, &asset)?;

        let error = RuntimeLayout::open(dir.path())
            .expect_err("a referenced canonical asset symlink must fail closed")
            .to_string();
        assert!(error.contains("regular file below assets"), "{error}");
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
    fn a_hand_edited_bundle_with_an_unknown_participant_schema_is_rejected() -> Result<()> {
        let dir = write_layout(ROBOT_YAML)?;
        let path = dir.path().join(ASSETS_DIR).join(PARTICIPANTS_ASSET);
        let edited = fs::read_to_string(&path)?
            .replace("phoxal/participants/v0", "phoxal/participants/v999");
        fs::write(path, edited)?;
        let error = RuntimeLayout::open(dir.path())
            .expect_err("an unknown compiled participant schema must be rejected")
            .to_string();
        assert!(
            error.contains("unsupported compiled participant schema"),
            "{error}"
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
        let payload = metadata("mission", "service", serde_json::json!({"type":"null"}));
        let dir = write_layout(ROBOT_YAML)?;
        write_bin(
            dir.path(),
            "mission",
            &synthesize_binary(host_architecture(), &payload),
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

    /// A binary declaring the wrong participant id for the canonical `bin/`
    /// path it landed at must
    /// fail inspection, naming both the declared and the expected identity -
    /// not silently pass with its schema paired to the wrong runtime.
    #[test]
    fn inspecting_a_binary_declaring_the_wrong_id_is_rejected() -> Result<()> {
        let payload = metadata("drive", "service", serde_json::json!({"type":"null"}));
        let dir = write_layout(ROBOT_YAML)?;
        write_bin(
            dir.path(),
            "mission",
            &synthesize_binary(host_architecture(), &payload),
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
    /// `participant_metadata::tests::foreign_object_without_section_is_a_clear_error`.
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
