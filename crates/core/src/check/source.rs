//! Source-built participant records used by checking and launch planning.

use std::path::PathBuf;

use crate::session::ParticipantKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceParticipant {
    pub name: String,
    pub expected_artifact_id: String,
    pub crate_dir: PathBuf,
    pub kind: SourceParticipantKind,
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
        }
    }

    #[must_use]
    pub fn component_driver(name: impl Into<String>, crate_dir: PathBuf) -> Self {
        let name = name.into();
        Self::component_driver_with_artifact_id(name.clone(), name, crate_dir)
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
        }
    }

    #[must_use]
    pub fn tool(
        name: impl Into<String>,
        expected_artifact_id: impl Into<String>,
        crate_dir: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            expected_artifact_id: expected_artifact_id.into(),
            crate_dir,
            kind: SourceParticipantKind::Tool,
        }
    }

    #[must_use]
    pub fn simulator(
        name: impl Into<String>,
        expected_artifact_id: impl Into<String>,
        crate_dir: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            expected_artifact_id: expected_artifact_id.into(),
            crate_dir,
            kind: SourceParticipantKind::Simulator,
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            SourceParticipantKind::UserService => "user service",
            SourceParticipantKind::OfficialService => "path-overridden official service",
            SourceParticipantKind::ComponentDriver => "component driver",
            SourceParticipantKind::Tool => "path-overridden tool",
            SourceParticipantKind::Simulator => "path-overridden simulator",
        }
    }
}

/// A source participant's role plus whether it has a known official/catalog
/// identity it locally overrides. Deliberately kept as its own enum rather
/// than collapsed into the shared
/// `crate::session::ParticipantKind`: every
/// `SourceParticipant` already carries a `crate_dir`, so it is inherently
/// "local" in the supervisor's sense - the real orthogonal bit this domain
/// needs is "does an official/catalog identity exist for this name" (see
/// `official`), not "is it local". `UserService` has no catalog counterpart
/// at all (a robot developer's own service); `OfficialService` is a known
/// official service whose source the robot developer is locally overriding;
/// `Tool`/`Simulator` are always the latter shape (a source override of a
/// known official artifact - see `kind_label`); `ComponentDriver` has no
/// such axis. Use [`Self::shared_kind`] to bridge into the shared enum for
/// call sites (`supervisor`, `watch`) that only care about the role split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceParticipantKind {
    UserService,
    OfficialService,
    ComponentDriver,
    Tool,
    Simulator,
}

impl SourceParticipantKind {
    #[must_use]
    pub const fn shared_kind(self) -> ParticipantKind {
        match self {
            Self::UserService | Self::OfficialService => ParticipantKind::Service,
            Self::ComponentDriver => ParticipantKind::Driver,
            Self::Tool => ParticipantKind::Tool,
            Self::Simulator => ParticipantKind::Simulator,
        }
    }

    /// Whether this source participant has a known official/catalog identity
    /// it is locally overriding, vs one invented purely by the user with no
    /// catalog counterpart at all (only `UserService`). See the type docs for
    /// why this is named `official`, not `local`.
    #[must_use]
    pub const fn official(self) -> bool {
        !matches!(self, Self::UserService)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolParticipant {
    pub name: String,
    pub binary_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserServiceImageParticipant {
    pub name: String,
    pub image_ref: String,
}
