//! The finalized-bundle loader.
//!
//! Execution consumes exactly one artifact: a finalized bundle, whose shape is
//!
//! ```text
//! <bundle>/robot.yaml               finalized robot/v0: no extends, explicit clock
//! <bundle>/assets/robot/...         robot structure and meshes
//! <bundle>/assets/components/...    frozen component definitions and meshes
//! <bundle>/assets/router/...        optional Zenoh router config
//! <bundle>/bin/...                  participant executables
//! ```
//!
//! There is no `robot.json`, no `participants.json`, and no `runtime.json`: the
//! finalized `robot.yaml` is the single persisted robot definition, and the
//! canonical [`phoxal_model::Robot`] is built from it in memory by the
//! framework's own [`FinalizedBundle`] loader.
//!
//! The loader derives the required participant set from two authorities - the
//! finalized document and the CLI-internal catalog, through the one
//! [`derive_runtime_requirements`] owner - resolves every required participant
//! to exactly one canonical binary under `bin/`, and inspects only the selected
//! binaries without ever executing them. Unreferenced extra files in `bin/` are
//! ignored and never inspected.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use phoxal_cli_catalog::Catalog;
use phoxal_manifest::bundle::FinalizedBundle;
use phoxal_manifest::source::robot::v0::Manifest;

use super::requirements::{
    RequiredParticipant, RequiredParticipantKind, RuntimeRequirements, SimulationMembership,
    derive_runtime_requirements,
};
use crate::check::participant_metadata::{
    CompatibilitySet, ExpectedTarget, ParticipantMeta, expected_target_for_host,
    inspect_selected_binary_for_target,
};

pub mod plan;

/// The finalized robot document, at the bundle root.
pub const ROBOT_FILE: &str = "robot.yaml";
/// The single root every participant-readable path lives below.
pub const ASSETS_DIR: &str = "assets";
/// The optional Zenoh router configuration, relative to `assets/`.
pub const ROUTER_CONFIG_ASSET: &str = "router/config.json5";
/// The optional Zenoh router configuration, relative to the bundle root.
pub const ROUTER_CONFIG_PATH: &str = "assets/router/config.json5";
/// The flat participant-executable store.
pub const BIN_DIR: &str = "bin";

/// Which target signature the loader inspects selected binaries against.
///
/// An in-place `run`/`start` inspects against the host - a bundle only ever
/// runs on the host it was staged or extracted for. `phoxal build --target
/// <TRIPLE>` produces a foreign-target bundle it will never execute here, so it
/// inspects against the *declared* target signature: a correct cross-compiled
/// binary validates, while a wrong-format, wrong-arch, or wrong-endian binary
/// for that target still fails precisely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutInspection {
    #[default]
    Host,
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

/// A required participant resolved to its canonical `bin/` binary and inspected
/// (target-checked, compatibility-checked, metadata-extracted) without
/// execution.
#[derive(Debug, Clone)]
pub struct SelectedBinary {
    pub path: PathBuf,
    pub meta: ParticipantMeta,
}

/// A loaded finalized bundle: the canonical model built from its `robot.yaml`,
/// the finalized document itself, and the participant set it requires. Holds no
/// source and no Cargo graph.
#[derive(Debug)]
pub struct RuntimeLayout {
    root: PathBuf,
    bundle: FinalizedBundle,
    manifest: Manifest,
    requirements: RuntimeRequirements,
}

impl RuntimeLayout {
    /// Whether `root` is shaped like a finalized bundle. Cheap existence checks
    /// only, used to tell an extracted or staged bundle (run in place) from a
    /// source project (finalized first).
    #[must_use]
    pub fn is_layout_root(root: &Path) -> bool {
        root.join(ROBOT_FILE).is_file()
            && root.join(ASSETS_DIR).is_dir()
            && root.join(BIN_DIR).is_dir()
    }

    /// Load and validate the finalized bundle at `root`. The framework loader
    /// owns document validation, `extends` rejection, deterministic component
    /// roots, and asset fencing; this adds the CLI-owned requirement derivation.
    pub fn open(root: &Path) -> Result<Self> {
        let bundle = FinalizedBundle::load(root)
            .with_context(|| format!("failed to load finalized bundle {}", root.display()))?;
        let robot_document = root.join(ROBOT_FILE);
        let text = std::fs::read_to_string(&robot_document)
            .with_context(|| format!("failed to read {}", robot_document.display()))?;
        let phoxal_manifest::source::robot::Manifest::V0(manifest) =
            phoxal_manifest::source::robot::parse_from_string(&text)
                .with_context(|| format!("failed to parse {}", robot_document.display()))?;
        let simulated = SimulationMembership::from_bundle_assets(
            &root.join(ASSETS_DIR),
            &manifest.used_component_types(),
        );
        let requirements =
            derive_runtime_requirements(&manifest, &simulated, &Catalog::official())?;
        Ok(Self {
            root: root.to_path_buf(),
            bundle,
            manifest,
            requirements,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn robot(&self) -> &phoxal_model::Robot {
        self.bundle.robot()
    }

    /// The finalized `robot/v0` document this bundle persists.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    #[must_use]
    pub fn requirements(&self) -> &RuntimeRequirements {
        &self.requirements
    }

    /// The Zenoh router configuration this bundle carries, when it declares one.
    #[must_use]
    pub fn router_config(&self) -> Option<&Path> {
        self.bundle.router_config()
    }

    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join(BIN_DIR)
    }

    /// Resolve one required participant to its canonical binary under `bin/`. A
    /// missing binary fails with a precise error naming the required identity.
    /// Extra unreferenced files in `bin/` are never looked up, so they are
    /// ignored and never launched.
    pub fn resolve_binary(&self, required: &RequiredParticipant) -> Result<PathBuf> {
        let path = self.bin_dir().join(&required.binary_name);
        if !path.is_file() {
            bail!(
                "finalized bundle {} is missing the binary for required participant `{}` \
                 (expected bin/{}); rebuild the bundle",
                self.root.display(),
                required.participant_id,
                required.binary_name
            );
        }
        Ok(path)
    }

    /// Resolve and inspect one required participant without executing it.
    ///
    /// Three gates, in order: the object file matches the target signature
    /// `inspection` selects; its embedded record is a supported
    /// `phoxal/participant-metadata/v0` document declaring this CLI's exact
    /// compatibility set (the reader owns that); and it declares the identity
    /// and kind the requirement expects. The last one is what catches the wrong
    /// binary landing at a canonical `bin/` path - a stale rebuild, a
    /// hand-edited bundle, two artifacts swapped on disk - before that binary's
    /// config schema is trusted to validate anything.
    pub fn inspect_for(
        &self,
        required: &RequiredParticipant,
        inspection: LayoutInspection,
    ) -> Result<SelectedBinary> {
        let path = self.resolve_binary(required)?;
        let meta = inspect_selected_binary_for_target(&path, &inspection.expected_target())
            .with_context(|| {
                format!(
                    "failed to inspect required participant `{}` at {}",
                    required.participant_id,
                    path.display()
                )
            })?;
        if meta.id != required.artifact_id {
            bail!(
                "finalized bundle {} binary bin/{} at {} declares participant id `{}`, but \
                 required participant `{}` expects `{}`; the wrong binary is staged at this \
                 canonical path",
                self.root.display(),
                required.binary_name,
                path.display(),
                meta.id,
                required.participant_id,
                required.artifact_id,
            );
        }
        let expected_kind = expected_kind(required.kind);
        ensure!(
            meta.kind == expected_kind,
            "finalized bundle {} binary bin/{} declares participant kind {:?}, but required \
             participant `{}` expects {:?}; the wrong participant kind is staged at this canonical \
             path",
            self.root.display(),
            required.binary_name,
            meta.kind,
            required.participant_id,
            expected_kind,
        );
        if required.kind == RequiredParticipantKind::Brain {
            // `#[phoxal::brain]` fixes `Config = ()`, so the brain's embedded
            // schema is exactly the unit schema. A binary claiming any other
            // config surface at `bin/brain` is not a brain, whatever it declares.
            ensure!(
                meta.config_schema == serde_json::json!({"type": "null"}),
                "finalized bundle {} binary bin/{} declares config schema {}, but the root brain \
                 takes no config at all and must declare {{\"type\":\"null\"}}",
                self.root.display(),
                required.binary_name,
                meta.config_schema,
            );
        }
        Ok(SelectedBinary { path, meta })
    }

    /// Inspect every distinct selected binary once, and require the whole graph
    /// to agree on one compatibility set. Keyed by canonical `bin/` name, since
    /// one binary serves every instance of a component type.
    pub fn inspect_selected(
        &self,
        inspection: LayoutInspection,
    ) -> Result<BTreeMap<String, SelectedBinary>> {
        let mut selected = BTreeMap::new();
        for (binary_name, required) in self.requirements.selected_binaries() {
            selected.insert(
                binary_name.to_string(),
                self.inspect_for(required, inspection)?,
            );
        }
        ensure_one_compatibility_set(&selected)?;
        Ok(selected)
    }
}

/// The participant kind a binary must declare for a given requirement role.
const fn expected_kind(kind: RequiredParticipantKind) -> phoxal_runtime_contract::ParticipantKind {
    use phoxal_runtime_contract::ParticipantKind;
    match kind {
        RequiredParticipantKind::Brain => ParticipantKind::Brain,
        RequiredParticipantKind::OfficialService | RequiredParticipantKind::UserService => {
            ParticipantKind::Service
        }
        RequiredParticipantKind::ComponentDriver => ParticipantKind::Driver,
        RequiredParticipantKind::WorldClock => ParticipantKind::Simulator,
    }
}

/// Every selected binary already matched this CLI's compatibility set
/// individually, so a disagreement between two of them is structurally
/// impossible. Asserting it here is what makes the graph-wide invariant
/// explicit rather than implied, and names the binary if it ever breaks.
fn ensure_one_compatibility_set(selected: &BTreeMap<String, SelectedBinary>) -> Result<()> {
    let expected = CompatibilitySet::current();
    for (binary_name, binary) in selected {
        ensure!(
            binary.meta.api == expected.api && binary.meta.schemas == expected.schemas,
            "bin/{binary_name} does not agree with the rest of the graph on the execution \
             compatibility set; every binary in one execution speaks exactly one API and one set \
             of document schemas"
        );
    }
    Ok(())
}

/// Validate the runtime declaration maps of an authored `robot/v0` document
/// against the CLI catalog, before any workspace scanning or build.
///
/// - `brain` is never declared: the mandatory root brain IS the root Cargo
///   package's binary, discovered from Cargo metadata and staged as `bin/brain`.
/// - official identities are never declared: official runtimes are
///   catalog-owned, always run, and take no authored configuration.
///
/// Both rules belong to [`derive_runtime_requirements`], which is why this
/// simply runs it: there is one owner, and the pre-build entry point is a
/// caller of it rather than a second copy.
pub fn validate_runtime_declarations(robot: &Manifest) -> Result<()> {
    derive_runtime_requirements(
        robot,
        &SimulationMembership::default(),
        &Catalog::official(),
    )
    .map(|_| ())
}

/// The short names of every official service in the CLI catalog.
#[must_use]
pub fn official_service_short_names() -> BTreeSet<&'static str> {
    Catalog::official().service_identities()
}
