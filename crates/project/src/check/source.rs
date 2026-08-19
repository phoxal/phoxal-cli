//! Source-built participant records used by project checking and building.

use std::path::PathBuf;

use phoxal::participant::metadata::ParticipantKind;

/// The canonical participant identity, staged `bin/` name, and authored
/// reserved name of the one mandatory root brain.
pub const BRAIN_ID: &str = "brain";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceParticipant {
    pub name: String,
    pub expected_artifact_id: String,
    pub crate_dir: PathBuf,
    pub kind: SourceParticipantKind,
    /// The exact Cargo binary target this participant's crate builds, when the
    /// canonical runtime identity ([`Self::name`]) is NOT that target's name.
    /// Only the root brain needs it: its Cargo package and bin target are
    /// project-specific while its runtime identity is always `brain`, so the
    /// two axes must stay separate rather than be inferred from each other.
    pub bin_target: Option<String>,
}

impl SourceParticipant {
    #[must_use]
    pub fn user_service(name: impl Into<String>, crate_dir: PathBuf) -> Self {
        let name = name.into();
        Self {
            expected_artifact_id: name.clone(),
            name,
            crate_dir,
            kind: SourceParticipantKind::UserService,
            bin_target: None,
        }
    }

    /// The one mandatory root brain, discovered from the root Cargo package.
    ///
    /// `crate_dir` is the root package's own directory and `bin_target` the
    /// exact Cargo-metadata-reported binary target it builds. The canonical
    /// runtime identity is always [`BRAIN_ID`], regardless of either.
    #[must_use]
    pub fn brain(crate_dir: PathBuf, bin_target: impl Into<String>) -> Self {
        Self {
            name: BRAIN_ID.to_string(),
            expected_artifact_id: BRAIN_ID.to_string(),
            crate_dir,
            kind: SourceParticipantKind::Brain,
            bin_target: Some(bin_target.into()),
        }
    }

    #[must_use]
    pub fn component_driver_with_artifact_id(
        name: impl Into<String>,
        expected_artifact_id: impl Into<String>,
        crate_dir: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            expected_artifact_id: expected_artifact_id.into(),
            crate_dir,
            kind: SourceParticipantKind::ComponentDriver,
            bin_target: None,
        }
    }

    #[must_use]
    pub fn official_service(
        name: impl Into<String>,
        expected_artifact_id: impl Into<String>,
        crate_dir: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            expected_artifact_id: expected_artifact_id.into(),
            crate_dir,
            kind: SourceParticipantKind::OfficialService,
            bin_target: None,
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            SourceParticipantKind::Brain => "root brain",
            SourceParticipantKind::UserService => "user service",
            SourceParticipantKind::OfficialService => "path-overridden official service",
            SourceParticipantKind::ComponentDriver => "component driver",
        }
    }
}

/// A source participant's role plus whether it has a known official/registry
/// identity it locally overrides. Deliberately kept as its own enum rather
/// than collapsed into the shared
/// framework `ParticipantKind`: every
/// `SourceParticipant` already carries a `crate_dir`, so it is inherently
/// "local" in the supervisor's sense - the real orthogonal bit this domain
/// needs is "does an official/registry identity exist for this name", not
/// "is it local". `Brain` is the one mandatory root brain, which has no
/// registry counterpart and can never be resolved from one;
/// `UserService` has no registry counterpart
/// at all (a robot developer's own service); `OfficialService` is a known
/// official service whose source the robot developer is locally overriding;
/// `ComponentDriver` has no such axis. Use [`Self::shared_kind`] to bridge into the shared enum for call
/// sites that only care about the role split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceParticipantKind {
    Brain,
    UserService,
    OfficialService,
    ComponentDriver,
}

impl SourceParticipantKind {
    #[must_use]
    pub const fn shared_kind(self) -> ParticipantKind {
        match self {
            Self::Brain => ParticipantKind::Brain,
            Self::UserService | Self::OfficialService => ParticipantKind::Service,
            Self::ComponentDriver => ParticipantKind::Driver,
        }
    }
}
