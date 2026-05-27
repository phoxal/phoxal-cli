use anyhow::{Result, bail};
use async_trait::async_trait;
use clap::Args;
use phoxal_utils_scenario_spec::{ScenarioOutcome, ScenarioReport};

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;

pub(crate) mod catalog;
pub(crate) mod discovery;
pub(crate) mod result;
pub(crate) mod runner;

pub(crate) use catalog::Phase;

#[derive(Debug, Args, Clone)]
#[command(group(
    clap::ArgGroup::new("selector")
        .args(["phase", "tier", "scenario", "list"])
        .required(true)
        .multiple(false)
))]
pub(crate) struct Scenario {
    #[arg(
        long,
        value_enum,
        conflicts_with_all = ["tier", "category", "scenario"],
        help = "Run all categories required by the named delivery phase."
    )]
    pub(crate) phase: Option<Phase>,

    #[arg(
        long,
        requires = "category",
        conflicts_with_all = ["phase", "scenario"],
        help = "Run scenarios in the named tier for the selected categories."
    )]
    pub(crate) tier: Option<u8>,

    #[arg(
        long,
        value_delimiter = ',',
        requires = "tier",
        conflicts_with_all = ["phase", "scenario"],
        help = "Comma-separated category names. Required when --tier is set."
    )]
    pub(crate) category: Vec<String>,

    #[arg(
        long,
        conflicts_with_all = ["phase", "tier", "category"],
        help = "Run one specific framework-conformance scenario by name."
    )]
    pub(crate) scenario: Option<String>,

    #[arg(
        long,
        conflicts_with_all = ["phase", "tier", "category", "scenario"],
        help = "List the framework-conformance scenario catalog and exit."
    )]
    pub(crate) list: bool,

    #[arg(long, help = "Launch Webots with its GUI for Tier 2+ scenarios.")]
    pub(crate) gui: bool,

    #[arg(
        long,
        default_value_t = 900,
        help = "Wallclock seconds before the whole scenario command fails."
    )]
    pub(crate) wallclock_timeout_secs: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) enum ScenarioRequest {
    Phase {
        phase: Phase,
        gui: bool,
        wallclock_timeout_secs: u64,
    },
    TierCategories {
        tier: u8,
        categories: Vec<catalog::Category>,
        gui: bool,
        wallclock_timeout_secs: u64,
    },
    Scenario {
        name: String,
        gui: bool,
        wallclock_timeout_secs: u64,
    },
}

#[async_trait(?Send)]
impl Command for Scenario {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        if self.list {
            println!("{}", serde_json::to_string_pretty(&runner::listing(app)?)?);
            return Ok(());
        }

        let request = self.request()?;
        let selector_slug = request.selector_slug()?;
        let report = runner::ScenarioRunner::discover(app)?
            .run(&request, app)
            .await?;
        let report_dir = app.project.dist_validation_scenario_dir(&selector_slug);
        result::write_report(&report_dir, &report.report)?;
        let report_display = format!("dist/validation/scenario/{selector_slug}/report.json");

        print_scenario_results(app, &report.report);
        print_summary(app, &selector_slug, &report.report, &report_display);

        if report.has_failures() {
            bail!("{}", report.failure_summary());
        }

        Ok(())
    }
}

impl Scenario {
    fn request(&self) -> Result<ScenarioRequest> {
        if let Some(phase) = self.phase {
            return Ok(ScenarioRequest::Phase {
                phase,
                gui: self.gui,
                wallclock_timeout_secs: self.wallclock_timeout_secs,
            });
        }

        if let Some(tier) = self.tier {
            if tier > 4 {
                bail!("scenario tier must be between 0 and 4, got {tier}");
            }

            let categories = self
                .category
                .iter()
                .map(|category| catalog::Category::parse(category))
                .collect::<Result<Vec<_>>>()?;

            return Ok(ScenarioRequest::TierCategories {
                tier,
                categories,
                gui: self.gui,
                wallclock_timeout_secs: self.wallclock_timeout_secs,
            });
        }

        if let Some(scenario) = &self.scenario {
            return Ok(ScenarioRequest::Scenario {
                name: scenario.clone(),
                gui: self.gui,
                wallclock_timeout_secs: self.wallclock_timeout_secs,
            });
        }

        bail!("scenario selector is required")
    }
}

impl ScenarioRequest {
    fn gui(&self) -> bool {
        match self {
            Self::Phase { gui, .. }
            | Self::TierCategories { gui, .. }
            | Self::Scenario { gui, .. } => *gui,
        }
    }

    fn wallclock_timeout_secs(&self) -> u64 {
        match self {
            Self::Phase {
                wallclock_timeout_secs,
                ..
            }
            | Self::TierCategories {
                wallclock_timeout_secs,
                ..
            }
            | Self::Scenario {
                wallclock_timeout_secs,
                ..
            } => *wallclock_timeout_secs,
        }
    }

    fn selector_slug(&self) -> Result<String> {
        match self {
            Self::Phase { phase, .. } => Ok(format!("phase-{}", phase.slug())),
            Self::TierCategories {
                tier, categories, ..
            } => Ok(format!(
                "tier-{tier}-{}",
                categories
                    .iter()
                    .map(|category| category.name())
                    .collect::<Vec<_>>()
                    .join("-")
            )),
            Self::Scenario { name, .. } => {
                if name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                {
                    Ok(format!("scenario-{name}"))
                } else {
                    bail!("scenario name {name:?} is not slug-safe")
                }
            }
        }
    }
}

fn print_scenario_results(app: &AppContext, report: &ScenarioReport) {
    for result in &report.results {
        let category = result.spec.category.as_deref().unwrap_or("unknown");
        let tier = result
            .spec
            .tier
            .map(|tier| tier.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        app.ui.info(format!(
            "scenario {} category={category} tier={tier} outcome={}",
            result.spec.name,
            outcome_slug(result.outcome)
        ));
    }
}

fn print_summary(
    app: &AppContext,
    selector_slug: &str,
    report: &ScenarioReport,
    report_display: &str,
) {
    let counts = ScenarioOutcomeCounts::from_report(report);
    let line = format!(
        "summary selector={selector_slug} passed={} failed={} missing={} timeout={} teardown-error={} report={report_display}",
        counts.passed, counts.failed, counts.missing, counts.timeout, counts.teardown_error
    );

    if report.has_failures() {
        app.ui.warn(line);
    } else {
        app.ui.success(line);
    }
}

fn outcome_slug(outcome: ScenarioOutcome) -> &'static str {
    match outcome {
        ScenarioOutcome::Passed => "passed",
        ScenarioOutcome::Failed => "failed",
        ScenarioOutcome::MissingImplementation => "missing-implementation",
        ScenarioOutcome::Timeout => "timeout",
        ScenarioOutcome::TeardownError => "teardown-error",
    }
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
struct ScenarioOutcomeCounts {
    passed: usize,
    failed: usize,
    missing: usize,
    timeout: usize,
    teardown_error: usize,
}

impl ScenarioOutcomeCounts {
    fn from_report(report: &ScenarioReport) -> Self {
        let mut counts = Self::default();
        for result in &report.results {
            match result.outcome {
                ScenarioOutcome::Passed => counts.passed += 1,
                ScenarioOutcome::Failed => counts.failed += 1,
                ScenarioOutcome::MissingImplementation => counts.missing += 1,
                ScenarioOutcome::Timeout => counts.timeout += 1,
                ScenarioOutcome::TeardownError => counts.teardown_error += 1,
            }
        }
        counts
    }
}

impl std::fmt::Display for ScenarioRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Phase {
                phase,
                gui,
                wallclock_timeout_secs,
            } => {
                let requirement = catalog::phase_requirement(*phase);
                write!(
                    f,
                    "phase {phase:?} at tier {} with categories [{}] (gui={gui}, wallclock_timeout_secs={wallclock_timeout_secs})",
                    requirement.tier,
                    requirement
                        .categories
                        .iter()
                        .map(|category| category.name())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            }
            Self::TierCategories {
                tier,
                categories,
                gui,
                wallclock_timeout_secs,
            } => write!(
                f,
                "tier {tier} with categories [{}] (gui={gui}, wallclock_timeout_secs={wallclock_timeout_secs})",
                categories
                    .iter()
                    .map(|category| category.name())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::Scenario {
                name,
                gui,
                wallclock_timeout_secs,
            } => write!(
                f,
                "scenario '{name}' (gui={gui}, wallclock_timeout_secs={wallclock_timeout_secs})"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use clap::Parser;

    use crate::commands::{Cli, RootCommand, validate::Validate};

    use super::catalog::Category;
    use super::{Phase, ScenarioRequest, catalog};
    use phoxal_utils_scenario_spec::{
        ScenarioOutcome, ScenarioReport, ScenarioResult, ScenarioSpec, WebotsWorld,
    };

    #[test]
    fn phase_selector_parses_without_robot_model() -> Result<()> {
        let cli = Cli::parse_from(["xtask", "validate", "scenario", "--phase", "p2"]);

        let RootCommand::Validate(Validate::Scenario(scenario)) = cli.command else {
            panic!("expected validate scenario command");
        };

        assert_eq!(
            scenario.request()?,
            ScenarioRequest::Phase {
                phase: Phase::P2,
                gui: false,
                wallclock_timeout_secs: 900,
            }
        );
        Ok(())
    }

    #[test]
    fn tier_category_selector_accepts_comma_separated_categories() -> Result<()> {
        let cli = Cli::parse_from([
            "xtask",
            "validate",
            "scenario",
            "--tier",
            "2",
            "--category",
            "localization,mapping",
        ]);

        let RootCommand::Validate(Validate::Scenario(scenario)) = cli.command else {
            panic!("expected validate scenario command");
        };

        assert_eq!(
            scenario.request()?,
            ScenarioRequest::TierCategories {
                tier: 2,
                categories: vec![Category::Localization, Category::Mapping],
                gui: false,
                wallclock_timeout_secs: 900,
            }
        );
        Ok(())
    }

    #[test]
    fn old_positional_scenario_shape_is_rejected() {
        let result = Cli::try_parse_from(["xtask", "validate", "scenario", "robot-v1", "tier3"]);

        assert!(result.is_err());
    }

    #[test]
    fn catalog_lists_phase_gate_categories_from_blueprint_validation() {
        let requirement = catalog::phase_requirement(Phase::P2);

        assert_eq!(requirement.tier, 2);
        assert_eq!(
            requirement.categories,
            &[
                Category::Localization,
                Category::Mapping,
                Category::Traversability,
                Category::RevisionConvergence,
            ]
        );
    }

    #[test]
    fn selector_slug_per_variant() -> Result<()> {
        assert_eq!(
            ScenarioRequest::Phase {
                phase: Phase::P0,
                gui: false,
                wallclock_timeout_secs: 900,
            }
            .selector_slug()?,
            "phase-p0"
        );
        assert_eq!(
            ScenarioRequest::Phase {
                phase: Phase::P2,
                gui: false,
                wallclock_timeout_secs: 900,
            }
            .selector_slug()?,
            "phase-p2"
        );
        assert_eq!(
            ScenarioRequest::TierCategories {
                tier: 2,
                categories: vec![Category::Localization, Category::Mapping],
                gui: false,
                wallclock_timeout_secs: 900,
            }
            .selector_slug()?,
            "tier-2-localization-mapping"
        );
        assert_eq!(
            ScenarioRequest::Scenario {
                name: "p2-localization-rgbd-inertial-orb-slam3".to_string(),
                gui: false,
                wallclock_timeout_secs: 900,
            }
            .selector_slug()?,
            "scenario-p2-localization-rgbd-inertial-orb-slam3"
        );

        Ok(())
    }

    #[test]
    fn selector_slug_rejects_unsafe_scenario_name() {
        let request = ScenarioRequest::Scenario {
            name: "../etc/passwd".to_string(),
            gui: false,
            wallclock_timeout_secs: 900,
        };

        assert!(request.selector_slug().is_err());
    }

    #[test]
    fn report_json_written_for_failing_run() -> Result<()> {
        let report = ScenarioReport::new(
            "scenario-failing-for-report-json-test",
            vec![ScenarioResult {
                spec: ScenarioSpec::new("failing-for-report-json-test")
                    .world(WebotsWorld::named("headless"))
                    .test("failing-for-report-json-test")
                    .timeout_secs(0)
                    .fixture_bundle("rgbd-imu-diff-drive")
                    .category("localization")
                    .tier(2),
                outcome: ScenarioOutcome::Failed,
                elapsed_logical_ns: Some(0),
                failure_diagnostics: vec!["intentional failure for report JSON test".to_string()],
            }],
        );
        let dir = tempfile::tempdir()?;

        let path = super::result::write_report(dir.path(), &report)?;

        assert!(path.exists());
        let contents = std::fs::read_to_string(path)?;
        let decoded: ScenarioReport = serde_json::from_str(&contents)?;
        assert_eq!(decoded, report);
        assert!(decoded.has_failures());

        Ok(())
    }
}
