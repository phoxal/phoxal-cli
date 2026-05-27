use anyhow::{Result, bail};
use serde::Serialize;

pub(crate) use phoxal_engine::step::Phase;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Category {
    ReadinessBootstrap,
    FrameCalibration,
    Odometry,
    Localization,
    StreamProfile,
    Mapping,
    Traversability,
    RevisionConvergence,
    Safety,
    FailureRecovery,
    Planning,
    Following,
    Mission,
    Exploration,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "condition", rename_all = "kebab-case")]
pub(crate) enum ProfileRequirement {
    RequiredByV1Reference,
    ProfileConditional(&'static str),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct PhaseRequirement {
    pub(crate) phase: Phase,
    pub(crate) tier: u8,
    pub(crate) categories: &'static [Category],
}

const P0_CATEGORIES: &[Category] = &[Category::ReadinessBootstrap];
const P1_CATEGORIES: &[Category] = &[Category::FrameCalibration, Category::Odometry];
const P2_CATEGORIES: &[Category] = &[
    Category::Localization,
    Category::Mapping,
    Category::Traversability,
    Category::RevisionConvergence,
];
const P3_CATEGORIES: &[Category] = &[Category::Safety, Category::FailureRecovery];
const P4_CATEGORIES: &[Category] = &[Category::Planning, Category::Following];
const P5_CATEGORIES: &[Category] = &[Category::Mission, Category::Exploration];

const P0_REQUIREMENT: PhaseRequirement = PhaseRequirement {
    phase: Phase::P0,
    tier: 1,
    categories: P0_CATEGORIES,
};
const P1_REQUIREMENT: PhaseRequirement = PhaseRequirement {
    phase: Phase::P1,
    tier: 1,
    categories: P1_CATEGORIES,
};
const P2_REQUIREMENT: PhaseRequirement = PhaseRequirement {
    phase: Phase::P2,
    tier: 2,
    categories: P2_CATEGORIES,
};
const P3_REQUIREMENT: PhaseRequirement = PhaseRequirement {
    phase: Phase::P3,
    tier: 2,
    categories: P3_CATEGORIES,
};
const P4_REQUIREMENT: PhaseRequirement = PhaseRequirement {
    phase: Phase::P4,
    tier: 2,
    categories: P4_CATEGORIES,
};
const P5_REQUIREMENT: PhaseRequirement = PhaseRequirement {
    phase: Phase::P5,
    tier: 3,
    categories: P5_CATEGORIES,
};

pub(crate) fn phase_requirement(phase: Phase) -> &'static PhaseRequirement {
    match phase {
        Phase::P0 => &P0_REQUIREMENT,
        Phase::P1 => &P1_REQUIREMENT,
        Phase::P2 => &P2_REQUIREMENT,
        Phase::P3 => &P3_REQUIREMENT,
        Phase::P4 => &P4_REQUIREMENT,
        Phase::P5 => &P5_REQUIREMENT,
    }
}

impl Category {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let normalized = value.trim();
        match normalized {
            "readiness-bootstrap" => Ok(Self::ReadinessBootstrap),
            "frame-calibration" => Ok(Self::FrameCalibration),
            "odometry" => Ok(Self::Odometry),
            "localization" => Ok(Self::Localization),
            "stream-profile" => Ok(Self::StreamProfile),
            "mapping" => Ok(Self::Mapping),
            "traversability" => Ok(Self::Traversability),
            "revision-convergence" => Ok(Self::RevisionConvergence),
            "safety" => Ok(Self::Safety),
            "failure-recovery" => Ok(Self::FailureRecovery),
            "planning" => Ok(Self::Planning),
            "following" => Ok(Self::Following),
            "mission" => Ok(Self::Mission),
            "exploration" => Ok(Self::Exploration),
            _ => bail!("unknown scenario category '{normalized}'"),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ReadinessBootstrap => "readiness-bootstrap",
            Self::FrameCalibration => "frame-calibration",
            Self::Odometry => "odometry",
            Self::Localization => "localization",
            Self::StreamProfile => "stream-profile",
            Self::Mapping => "mapping",
            Self::Traversability => "traversability",
            Self::RevisionConvergence => "revision-convergence",
            Self::Safety => "safety",
            Self::FailureRecovery => "failure-recovery",
            Self::Planning => "planning",
            Self::Following => "following",
            Self::Mission => "mission",
            Self::Exploration => "exploration",
        }
    }
}

#[cfg(test)]
mod fixture_bundle_tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use phoxal_utils_robot::Model;
    use phoxal_utils_robot::v1::{SourceBundle, resolve_source_bundle};

    const COMPONENT_TYPES: &[&str] = &["camera_rgbd_640x480", "drive_motor", "imu", "range_tof"];

    #[test]
    fn rgbd_imu_diff_drive_fixture_conforms_to_unseen_ground_navigation() {
        let manifest_dir = match std::env::var("CARGO_MANIFEST_DIR") {
            Ok(value) => PathBuf::from(value),
            Err(error) => panic!("CARGO_MANIFEST_DIR is not set: {error}"),
        };
        let workspace_root = match manifest_dir.parent() {
            Some(path) => path,
            None => panic!(
                "phoxal-cli CARGO_MANIFEST_DIR must live one level below the workspace root: {}",
                manifest_dir.display()
            ),
        };
        let bundle_root = workspace_root
            .join("fixture")
            .join("robot")
            .join("rgbd-imu-diff-drive");

        let model = match Model::read_from_dir(&bundle_root) {
            Ok(model) => match model.as_v1() {
                Some(model) => model.clone(),
                None => panic!("fixture model.yaml is not version v1"),
            },
            Err(error) => panic!(
                "failed to read fixture model from {}: {error:#}",
                bundle_root.display()
            ),
        };

        let components = COMPONENT_TYPES
            .iter()
            .map(|component_type| {
                let component_dir = workspace_root
                    .join("fixture")
                    .join("component")
                    .join(component_type);
                let component =
                    match phoxal_utils_component::Component::read_from_dir(&component_dir) {
                        Ok(component) => match component.as_v1() {
                            Some(component) => component.clone(),
                            None => panic!("fixture component {component_type} is not version v1"),
                        },
                        Err(error) => panic!(
                            "failed to read fixture component {} from {}: {error:#}",
                            component_type,
                            component_dir.display()
                        ),
                    };
                ((*component_type).to_string(), component)
            })
            .collect::<BTreeMap<_, _>>();

        let resolved_facts = match resolve_source_bundle(SourceBundle::new(model, components)) {
            Ok(resolved_facts) => resolved_facts,
            Err(error) => panic!("fixture bundle failed autonomy conformance: {error:#}"),
        };

        assert!(
            resolved_facts.conformance_report.is_pass(),
            "{:#?}",
            resolved_facts.conformance_report
        );
    }
}
