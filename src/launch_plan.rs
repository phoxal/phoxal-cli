use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::check as graph_check;
use phoxal::participant::launch::{
    BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::commands::check::{SourceParticipant, SourceParticipantKind};
use crate::resolver::{ResolvedRobot, ResolvedTool, RobotManifestExtras};

pub const DEFAULT_ROUTER_CONNECT: &str = "tcp/localhost:7447";
pub const SITE_TOOL_ROUTER: &str = "tool-router";
pub const SITE_TOOL_JOYPAD: &str = "tool-joypad";
/// The host-resource-meter tool (CLI-UX Phase 3/4): a standard, OBSERVABLE
/// bus participant exactly like `tool-joypad`, published in every mode (Run,
/// Deploy, Webots) - a host meter is useful everywhere, including a deployed
/// robot, unlike the joypad peripheral. Degrades GRACEFULLY when the active
/// catalog snapshot predates it: see `resolver::OFFICIAL_OPTIONAL_TOOLS` and
/// `build_site_launches`.
pub const SITE_TOOL_TELEMETRY: &str = "tool-telemetry";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    Run,
    Deploy,
    /// Simulate under Webots, carrying the resolved `.wbt` world path the
    /// plan was built for. Replaces the old data-less `Sim` variant plus the
    /// `SimulatePlan::world_path` field it used to take a detour through -
    /// the plan's own mode now carries the world directly.
    Webots {
        world: PathBuf,
    },
}

/// The shared context a `LaunchPlan` is built from and launched alongside:
/// which `robot.yaml` it came from, the project root, the full resolved
/// robot, and its source participants. Not part of the plan itself (the plan
/// is the launch descriptor; this is where it came from), and - like
/// `LaunchPlan` - never persisted to disk. Replaces the fields the old
/// `SimulatePlan` wrapper re-declared next to its own `LaunchPlan`
/// (`resolved`/`project_root`/`source_participants`/`robot_path`), and the
/// matching re-declarations in `run`'s `PreparedRun`, `deploy`'s
/// `RenderPayloadInput`, and the `watch` configs.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanContext {
    pub robot_path: PathBuf,
    pub project_root: PathBuf,
    pub resolved: ResolvedRobot,
    pub source_participants: Vec<SourceParticipant>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub mode: LaunchMode,
    pub site: Vec<SiteLaunch>,
    pub robots: Vec<RobotLaunch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SiteLaunch {
    pub id: String,
    pub artifact_ref: String,
    #[serde(rename = "PHOXAL_CONFIG")]
    pub phoxal_config: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RobotLaunch {
    pub id: String,
    pub namespace: String,
    pub participants: Vec<ParticipantLaunchRecord>,
    pub substitutions: Vec<SubstitutionRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParticipantLaunchRecord {
    pub artifact_id: String,
    pub execution: ParticipantExecution,
    pub launch: ParticipantLaunch,
    #[serde(default)]
    pub launch_ownership: LaunchOwnership,
}

/// Who owns a participant's process lifecycle. Orthogonal to `participant_kind`:
/// most participants are `CliManaged` (the CLI supervisor spawns, restarts, and
/// tears them down). A `SimulationManaged` participant still satisfies the
/// graph proof and appears on the board via bus presence/logs (D23), but the
/// CLI supervisor never spawns or restarts it - Webots (via the supervisor)
/// owns its lifecycle. Both the Webots supervisor and each robot's controller
/// are `SimulationManaged` in `Webots` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchOwnership {
    #[default]
    CliManaged,
    SimulationManaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "execution", rename_all = "snake_case")]
pub enum ParticipantExecution {
    OfficialArtifact { artifact_ref: String },
    UserService { crate_dir: PathBuf },
    SourceArtifact { kind: String, crate_dir: PathBuf },
    ComponentDriver { crate_dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstitutionRecord {
    pub component_instance: String,
    pub provider_participant_id: String,
    pub provider_artifact_id: String,
    pub provider_kind: String,
    pub contracts: Vec<SubstitutedContract>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstitutedContract {
    pub family: String,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckedRobotLaunchInput<'a> {
    pub project_root: &'a Path,
    pub resolved: &'a ResolvedRobot,
    pub manifest_extras: &'a RobotManifestExtras,
    pub checked_participants: &'a [graph_check::ParticipantApis],
    pub substitutions: &'a [SubstitutionRecord],
    pub source_participants: &'a [SourceParticipant],
}

pub fn build_launch_plan(
    mode: LaunchMode,
    robots: &[CheckedRobotLaunchInput<'_>],
) -> Result<LaunchPlan> {
    if robots.is_empty() {
        bail!("LaunchPlan requires at least one robot");
    }
    if matches!(mode, LaunchMode::Deploy) && robots.len() != 1 {
        bail!("deploy LaunchPlan must contain exactly one robot");
    }

    let site = build_site_launches(&mode, robots)?;
    let robots = robots
        .iter()
        .map(|robot| build_robot_launch(&mode, robot))
        .collect::<Result<Vec<_>>>()?;

    Ok(LaunchPlan { mode, site, robots })
}

fn build_site_launches(
    mode: &LaunchMode,
    robots: &[CheckedRobotLaunchInput<'_>],
) -> Result<Vec<SiteLaunch>> {
    let router = merge_site_tool_artifact(SITE_TOOL_ROUTER, robots)?;
    let router_config = router_config(mode, robots)?;
    let mut site = vec![SiteLaunch {
        id: SITE_TOOL_ROUTER.to_string(),
        artifact_ref: router,
        phoxal_config: router_config,
    }];
    // `tool-joypad` is a standard site tool in every mode, including Deploy
    // (CLI-UX Phase 4): a deployed robot ships the peripheral tool the same
    // way it ships every other official site tool, ordered after the router
    // in its own systemd unit (`commands::deploy::render_units`).
    let joypad = merge_site_tool_artifact(SITE_TOOL_JOYPAD, robots)?;
    site.push(SiteLaunch {
        id: SITE_TOOL_JOYPAD.to_string(),
        artifact_ref: joypad,
        // Configless tool (`type Config = ()`): `Value::Null` marks "no config"
        // so the env builders OMIT `PHOXAL_CONFIG` entirely. Passing `{}` makes
        // a unit-config tool fail to deserialize (`invalid type: map, expected
        // unit`); an absent var uses the runner's null/unit fallback.
        phoxal_config: Value::Null,
    });
    // `tool-telemetry` is a standard OBSERVABLE site tool too, but degrades
    // GRACEFULLY rather than failing the whole launch plan: unlike router and
    // joypad (`merge_site_tool_artifact`, hard-required), a catalog snapshot
    // that predates telemetry's addition to the framework simply has no
    // resolved entry for it (`resolver::OFFICIAL_OPTIONAL_TOOLS`), so it is
    // omitted from the site set here instead of erroring. Every downstream
    // consumer (`commands::run::prepare_site_tools`, the TUI host meter)
    // already treats "this optional tool is absent" as a normal state.
    if let Some(telemetry) = merge_optional_site_tool_artifact(SITE_TOOL_TELEMETRY, robots) {
        site.push(SiteLaunch {
            id: SITE_TOOL_TELEMETRY.to_string(),
            artifact_ref: telemetry,
            // Configless (`type Config = ()`): omit `PHOXAL_CONFIG` - see joypad above.
            phoxal_config: Value::Null,
        });
    }
    Ok(site)
}

fn merge_site_tool_artifact(
    tool_name: &str,
    robots: &[CheckedRobotLaunchInput<'_>],
) -> Result<String> {
    let mut artifact_ref = None;
    for robot in robots {
        let tool = resolved_tool(robot.resolved, tool_name)?;
        let current = tool_artifact_ref(tool);
        if let Some(existing) = &artifact_ref {
            if existing != &current {
                bail!(
                    "site tool {tool_name} resolves to conflicting artifacts: {existing} and {current}"
                );
            }
        } else {
            artifact_ref = Some(current);
        }
    }
    artifact_ref.ok_or_else(|| anyhow!("site tool {tool_name} was not resolved"))
}

/// The optional-tool counterpart to [`merge_site_tool_artifact`]: `None` if
/// ANY robot's resolved graph lacks `tool_name` (the catalog-absent case -
/// tolerated, not an error) rather than bailing. A conflicting artifact ref
/// across robots that DO have it resolved is still a genuine inconsistency
/// and bails, same as the required path.
fn merge_optional_site_tool_artifact(
    tool_name: &str,
    robots: &[CheckedRobotLaunchInput<'_>],
) -> Option<String> {
    let mut artifact_ref: Option<String> = None;
    for robot in robots {
        let tool = robot
            .resolved
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)?;
        let current = tool_artifact_ref(tool);
        match &artifact_ref {
            Some(existing) if existing != &current => {
                tracing::warn!(
                    tool = tool_name,
                    "optional site tool resolves to conflicting artifacts across robots; omitting it from the site set"
                );
                return None;
            }
            Some(_) => {}
            None => artifact_ref = Some(current),
        }
    }
    artifact_ref
}

fn resolved_tool<'a>(resolved: &'a ResolvedRobot, tool_name: &str) -> Result<&'a ResolvedTool> {
    resolved
        .tools
        .iter()
        .find(|tool| tool.name == tool_name)
        .ok_or_else(|| {
            anyhow!(
                "resolved robot {} is missing required site tool {tool_name}",
                resolved.robot.robot.id
            )
        })
}

fn tool_artifact_ref(tool: &ResolvedTool) -> String {
    format!("{}@{}:{}", tool.repo, tool.resolved, tool.asset)
}

fn router_config(mode: &LaunchMode, robots: &[CheckedRobotLaunchInput<'_>]) -> Result<Value> {
    let mut listen = BTreeSet::<String>::new();
    let mut device_claims = BTreeMap::<String, String>::new();
    for robot in robots {
        for endpoint in &robot.resolved.robot.bus.listen {
            let endpoint = endpoint.trim();
            if endpoint.is_empty() {
                continue;
            }
            if let Some(device) = listen_device_claim(endpoint) {
                if let Some(existing) = device_claims.get(&device) {
                    if existing != endpoint {
                        bail!(
                            "conflicting router listen claims for device {device}: {existing} and {endpoint}"
                        );
                    }
                } else {
                    device_claims.insert(device, endpoint.to_string());
                }
            }
            listen.insert(endpoint.to_string());
        }
    }

    let mut config = Map::new();
    if !listen.is_empty() {
        config.insert(
            "listen".to_string(),
            Value::Array(listen.into_iter().map(Value::String).collect()),
        );
    }
    if matches!(mode, LaunchMode::Deploy)
        && let Some(uplink) = &robots[0].resolved.robot.bus.uplink
    {
        config.insert(
            "uplink".to_string(),
            serde_json::to_value(uplink).context("failed to encode router uplink config")?,
        );
    }
    Ok(Value::Object(config))
}

fn listen_device_claim(endpoint: &str) -> Option<String> {
    let rest = endpoint.strip_prefix("serial/")?;
    let query = rest.find('?').unwrap_or(rest.len());
    let fragment = rest.find('#').unwrap_or(rest.len());
    let device = &rest[..query.min(fragment)];
    Some(device.to_string())
}

fn build_robot_launch(
    mode: &LaunchMode,
    input: &CheckedRobotLaunchInput<'_>,
) -> Result<RobotLaunch> {
    ensure_launch_set_parity(mode, input)?;
    let source_participants = input
        .source_participants
        .iter()
        .map(|participant| (participant.name.as_str(), participant))
        .collect::<BTreeMap<_, _>>();
    let mut official_artifacts = input
        .resolved
        .platform_runtimes
        .iter()
        .chain(input.resolved.simulators.iter())
        .map(|runtime| (runtime.name.as_str(), runtime.artifact_ref().to_string()))
        .collect::<BTreeMap<_, _>>();
    // A Catalog-sourced component driver is a first-class catalog artifact
    // too (docs #21): its `catalog_runtime` projects onto the identical
    // `ResolvedPlatformRuntime` shape a service/simulator resolves to, keyed
    // here by the component id (`checked.artifact_id`) exactly like a
    // service is keyed by its own name.
    official_artifacts.extend(
        input
            .resolved
            .components
            .iter()
            .filter_map(|component| component.driver.as_ref())
            .filter_map(|driver| driver.catalog_runtime.as_ref())
            .map(|runtime| (runtime.name.as_str(), runtime.artifact_ref().to_string())),
    );

    let mut participants = Vec::new();
    for checked in input
        .checked_participants
        .iter()
        .filter(|participant| is_robot_launch_participant(mode, participant))
    {
        let execution = participant_execution(checked, &source_participants, &official_artifacts)?;
        let launch = participant_launch(mode, input, checked);
        let launch_ownership = launch_ownership(mode, checked);
        participants.push(ParticipantLaunchRecord {
            artifact_id: checked.artifact_id.clone(),
            execution,
            launch,
            launch_ownership,
        });
    }
    participants.sort_by(|left, right| {
        left.launch
            .participant_id
            .cmp(&right.launch.participant_id)
            .then_with(|| left.artifact_id.cmp(&right.artifact_id))
    });

    Ok(RobotLaunch {
        id: input.resolved.robot.robot.id.clone(),
        namespace: input.resolved.robot.robot.namespace.clone(),
        participants,
        substitutions: input.substitutions.to_vec(),
    })
}

fn is_robot_launch_participant(
    mode: &LaunchMode,
    participant: &graph_check::ParticipantApis,
) -> bool {
    if !participant.participant_class.is_checked() {
        return false;
    }
    if participant.participant_kind == graph_check::ParticipantKind::Tool {
        return false;
    }
    if participant.participant_kind == graph_check::ParticipantKind::Simulator {
        // Simulator participants (the Webots supervisor + each robot's
        // controller) are launched by Webots itself, never by the CLI
        // supervisor - but in Webots mode they still need a launch record for
        // board presence and controllerArgs/spawn-descriptor rendering (Part
        // 3/4). Outside Webots mode a simulator participant never appears in
        // the checked set at all (substitutions are sim-only), so this only
        // takes effect for Webots.
        return matches!(mode, LaunchMode::Webots { .. });
    }
    if matches!(mode, LaunchMode::Webots { .. })
        && matches!(
            participant.participant_kind,
            graph_check::ParticipantKind::Driver
        )
    {
        return false;
    }
    true
}

/// Which launch-ownership a checked participant gets in this plan. Simulator
/// participants (the Webots supervisor and each robot's controller) are
/// `SimulationManaged` in `Webots` mode - the CLI supervisor never spawns or
/// restarts them, Webots does. Every other participant (services, user
/// runtimes, component drivers) is `CliManaged`.
fn launch_ownership(
    mode: &LaunchMode,
    participant: &graph_check::ParticipantApis,
) -> LaunchOwnership {
    if matches!(mode, LaunchMode::Webots { .. })
        && participant.participant_kind == graph_check::ParticipantKind::Simulator
    {
        LaunchOwnership::SimulationManaged
    } else {
        LaunchOwnership::CliManaged
    }
}

fn participant_execution(
    checked: &graph_check::ParticipantApis,
    source_participants: &BTreeMap<&str, &SourceParticipant>,
    official_artifacts: &BTreeMap<&str, String>,
) -> Result<ParticipantExecution> {
    let source = source_participants
        .get(checked.participant_id.as_str())
        .or_else(|| {
            if checked.participant_kind == graph_check::ParticipantKind::Simulator {
                source_participants.get(checked.artifact_id.as_str())
            } else {
                None
            }
        });
    if let Some(source) = source {
        return Ok(match source.kind {
            SourceParticipantKind::UserService => ParticipantExecution::UserService {
                crate_dir: source.crate_dir.clone(),
            },
            SourceParticipantKind::OfficialService => ParticipantExecution::SourceArtifact {
                kind: "service".to_string(),
                crate_dir: source.crate_dir.clone(),
            },
            SourceParticipantKind::ComponentDriver => ParticipantExecution::ComponentDriver {
                crate_dir: source.crate_dir.clone(),
            },
            SourceParticipantKind::Tool => ParticipantExecution::SourceArtifact {
                kind: "tool".to_string(),
                crate_dir: source.crate_dir.clone(),
            },
            SourceParticipantKind::Simulator => ParticipantExecution::SourceArtifact {
                kind: "simulator".to_string(),
                crate_dir: source.crate_dir.clone(),
            },
        });
    }
    if let Some(artifact_ref) = official_artifacts.get(checked.artifact_id.as_str()) {
        return Ok(ParticipantExecution::OfficialArtifact {
            artifact_ref: artifact_ref.clone(),
        });
    }
    bail!(
        "checked participant {} has no resolved execution source",
        checked.participant_id
    )
}

fn participant_launch(
    mode: &LaunchMode,
    input: &CheckedRobotLaunchInput<'_>,
    checked: &graph_check::ParticipantApis,
) -> ParticipantLaunch {
    let component_instance = match &checked.scope {
        graph_check::ParticipantScope::ComponentInstance(instance) => Some(instance.clone()),
        graph_check::ParticipantScope::Graph => None,
    };
    ParticipantLaunch {
        participant_id: checked.participant_id.clone(),
        namespace: input.resolved.robot.robot.namespace.clone(),
        robot_id: input.resolved.robot.robot.id.clone(),
        bus: BusProfile {
            connect_endpoints: vec![DEFAULT_ROUTER_CONNECT.to_string()],
        },
        clock: match mode {
            LaunchMode::Run | LaunchMode::Deploy => ClockMode::Real,
            // In a Webots simulation the supervisor and per-robot controllers
            // ARE the clock authority: they are Webots controllers that
            // self-drive via `wb_robot_step` (both spawn `synchronization TRUE`,
            // so Webots will not advance until each has stepped) and the
            // supervisor publishes `simulation/clock` from that advance. They
            // must therefore run on the REAL scheduler. Only pure-bus
            // participants (services, component drivers) follow the published
            // clock in Simulation mode. Giving a simulator ClockMode::Simulation
            // deadlocks it: its `#[step]` would block waiting for the very
            // `simulation/clock` feed it is supposed to produce, so the whole
            // simulation freezes with no clock ever ticking.
            LaunchMode::Webots { .. } => {
                if checked.participant_kind == graph_check::ParticipantKind::Simulator {
                    ClockMode::Real
                } else {
                    ClockMode::Simulation
                }
            }
        },
        config: input
            .manifest_extras
            .user_runtime_config(&checked.participant_id)
            .cloned(),
        robot_root: Some(robot_root_for_mode(mode, input.project_root)),
        component_instance,
        shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
    }
}

fn robot_root_for_mode(mode: &LaunchMode, project_root: &Path) -> PathBuf {
    match mode {
        LaunchMode::Run | LaunchMode::Webots { .. } => project_root.to_path_buf(),
        // The deployed robot root is the active generation symlink, not the
        // flat `/opt/phoxal` - robot.yaml, structure.urdf, and phoxal-release.json
        // are staged per-generation under `/opt/phoxal/active/` (see deploy's
        // ACTIVE_ROOT), so a participant reading `$PHOXAL_ROBOT_ROOT/robot.yaml`
        // must resolve through `active`. Pointing at `/opt/phoxal` makes every
        // participant fail with "failed to read robot file /opt/phoxal/robot.yaml".
        LaunchMode::Deploy => PathBuf::from("/opt/phoxal/active"),
    }
}

fn ensure_launch_set_parity(mode: &LaunchMode, input: &CheckedRobotLaunchInput<'_>) -> Result<()> {
    let expected = expected_checked_participant_ids(mode, input.resolved);
    let checked = input
        .checked_participants
        .iter()
        .filter(|participant| is_robot_launch_participant(mode, participant))
        .map(|participant| participant.participant_id.clone())
        .collect::<BTreeSet<_>>();

    let missing = expected
        .difference(&checked)
        .cloned()
        .collect::<Vec<String>>();
    let extra = checked
        .difference(&expected)
        .cloned()
        .collect::<Vec<String>>();

    if !missing.is_empty() || !extra.is_empty() {
        let mut message = String::from("checked participant set does not match the LaunchPlan set");
        if !missing.is_empty() {
            message.push_str("; missing from checked metadata: ");
            message.push_str(&missing.join(", "));
        }
        if !extra.is_empty() {
            message.push_str("; checked metadata has no resolved participant: ");
            message.push_str(&extra.join(", "));
        }
        bail!("{message}");
    }
    Ok(())
}

fn expected_checked_participant_ids(
    mode: &LaunchMode,
    resolved: &ResolvedRobot,
) -> BTreeSet<String> {
    let mut expected = BTreeSet::new();
    expected.extend(
        resolved
            .platform_runtimes
            .iter()
            .map(|runtime| runtime.name.clone()),
    );
    expected.extend(
        resolved
            .user_runtimes
            .iter()
            .map(|runtime| runtime.name.clone()),
    );
    if matches!(mode, LaunchMode::Webots { .. }) {
        expected.extend(expected_simulator_participant_ids(resolved));
    } else {
        expected.extend(
            resolved
                .components
                .iter()
                .filter(|component| component.has_driver)
                .map(|component| component.instance.clone()),
        );
    }
    expected
}

/// The participant ids the Webots launch set must carry for the resolved
/// simulator artifacts (the Webots supervisor plus this robot's controller),
/// using the same world-scoped/robot-scoped id scheme
/// `commands::simulate::official_simulator_participants` assigns.
fn expected_simulator_participant_ids(resolved: &ResolvedRobot) -> BTreeSet<String> {
    resolved
        .simulators
        .iter()
        .filter_map(|runtime| {
            crate::commands::simulate::simulator_participant_id_for_resolved_artifact(
                &runtime.name,
                &resolved.robot.robot.id,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use anyhow::bail;

    use crate::catalog::{
        SelectionChannel as CatalogChannel, fixture_catalog_for_tests,
        fixture_component_assets_entry_for_tests, fixture_component_driver_entry_for_tests,
        fixture_contract_for_tests, fixture_service_entry_for_tests,
    };
    use crate::commands::check::{
        CheckGraphContext, RawArtifact, RawEmitApis, SourceParticipant,
        platform_artifact_refs_from_resolved, run_check_with_context,
    };
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use crate::resolver::{ResolveOptions, ResolvedRobot, host_target_triple, resolve};

    use super::*;

    #[test]
    fn launch_plan_covers_site_singletons_services_and_component_instances() -> anyhow::Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        std::fs::create_dir_all(temp.path().join("runtimes/mission"))?;
        std::fs::write(
            temp.path().join("runtimes/mission/Cargo.toml"),
            "[package]\nname = \"mission\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        std::fs::write(temp.path().join("runtimes/mission/src.txt"), "source")?;
        let robot = phoxal::model::robot::v0::Robot::parse_from_string(FIXTURE_ROBOT)?;
        let catalog = fixture_catalog_for_tests(vec![
            fixture_service_entry_for_tests(
                "drive",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                true,
                vec![fixture_contract_for_tests(
                    "y2026_1::drive::Target",
                    "publish",
                )],
            ),
            fixture_component_assets_entry_for_tests("ddsm115", "0.1.0", CatalogChannel::Stable),
            fixture_component_driver_entry_for_tests(
                "ddsm115",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                true,
                Vec::new(),
            ),
        ]);
        let mut resolved = resolve(
            &robot,
            temp.path(),
            Some(&catalog),
            ResolveOptions {
                resolve_source_commits: false,
                resolve_component_asset_commits: false,
                ..ResolveOptions::default()
            },
        )?;
        add_site_tools(&mut resolved);
        let mut extras = RobotManifestExtras::default();
        extras.user_runtimes.insert(
            "mission".to_string(),
            crate::resolver::UserRuntimeManifestExtras {
                config: Some(serde_json::json!({
                    "message": "line\nquoted \"value\"",
                })),
            },
        );
        let source_participants = vec![
            SourceParticipant::user_service("mission", temp.path().join("runtimes/mission")),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive",
                "ddsm115",
                temp.path().join("components/ddsm115"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "right_drive",
                "ddsm115",
                temp.path().join("components/ddsm115"),
            ),
        ];
        let platform_refs = platform_artifact_refs_from_resolved(&resolved);
        let outcome = run_check_with_context(
            &platform_refs,
            &[],
            &source_participants,
            CheckGraphContext {
                manifest_extras: &extras,
            },
            |artifact_ref| {
                let participant = platform_refs
                    .iter()
                    .find(|participant| participant.artifact_ref == artifact_ref)
                    .ok_or_else(|| {
                        anyhow::anyhow!("unexpected platform artifact {artifact_ref}")
                    })?;
                Ok(raw_emit_apis(
                    participant.kind.emit_apis_kind(),
                    &participant.name,
                ))
            },
            |_| bail!("no tools in this check fixture"),
            |source| match source.kind {
                SourceParticipantKind::UserService => Ok(raw_emit_apis("service", &source.name)),
                SourceParticipantKind::ComponentDriver => {
                    Ok(raw_emit_apis("driver", &source.expected_artifact_id))
                }
                SourceParticipantKind::OfficialService => {
                    Ok(raw_emit_apis("service", &source.expected_artifact_id))
                }
                SourceParticipantKind::Tool => {
                    Ok(raw_emit_apis("tool", &source.expected_artifact_id))
                }
                SourceParticipantKind::Simulator => {
                    Ok(raw_emit_apis("simulator", &source.expected_artifact_id))
                }
            },
        )?;
        assert!(outcome.is_ok(), "fixture check should pass: {outcome:?}");
        let plan = build_launch_plan(
            LaunchMode::Run,
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &outcome.checked_participants,
                substitutions: &[],
                source_participants: &source_participants,
            }],
        )?;

        assert_eq!(plan.mode, LaunchMode::Run);
        assert_eq!(plan.site[0].id, SITE_TOOL_ROUTER);
        assert_eq!(
            plan.site[0].phoxal_config,
            serde_json::json!({"listen": ["serial//dev/ttyUSB0?baudrate=115200"]})
        );
        assert_eq!(plan.site[1].id, SITE_TOOL_JOYPAD);
        let robot = &plan.robots[0];
        assert_eq!(robot.id, "robot_v1");
        assert_eq!(robot.substitutions, Vec::<SubstitutionRecord>::new());
        let participant_ids = robot
            .participants
            .iter()
            .map(|participant| participant.launch.participant_id.as_str())
            .collect::<Vec<_>>();
        for (service, _) in crate::catalog::OFFICIAL_SERVICES {
            assert!(
                participant_ids.contains(service),
                "missing platform service {service}: {participant_ids:?}"
            );
        }
        assert!(participant_ids.contains(&"left_drive"));
        assert!(participant_ids.contains(&"right_drive"));
        assert_eq!(
            participant_ids
                .iter()
                .filter(|id| **id == "mission")
                .count(),
            2,
            "official and user mission participants are both represented"
        );
        let left_drive = robot
            .participants
            .iter()
            .find(|participant| participant.launch.participant_id == "left_drive")
            .expect("left_drive participant");
        assert_eq!(left_drive.artifact_id, "ddsm115");
        assert_eq!(
            left_drive.launch.component_instance.as_deref(),
            Some("left_drive")
        );
        let mission = robot
            .participants
            .iter()
            .find(|participant| participant.launch.participant_id == "mission")
            .expect("mission participant");
        assert_eq!(
            mission.launch.config,
            Some(serde_json::json!({"message": "line\nquoted \"value\""}))
        );
        let encoded = crate::launch_env::encode_participant_env(&mission.launch)?;
        assert_eq!(
            encoded
                .variables()
                .get(phoxal::participant::launch::env::CONFIG)
                .map(String::as_str),
            Some(r#"{"message":"line\nquoted \"value\""}"#)
        );
        Ok(())
    }

    #[test]
    fn deploy_plan_rejects_multiple_robots() -> anyhow::Result<()> {
        let robot = empty_resolved_robot("robot_a")?;
        let extras = RobotManifestExtras::default();
        let inputs = [
            empty_checked_input(Path::new("/tmp/a"), &robot, &extras),
            empty_checked_input(Path::new("/tmp/b"), &robot, &extras),
        ];
        let error =
            build_launch_plan(LaunchMode::Deploy, &inputs).expect_err("deploy is one robot");
        assert!(error.to_string().contains("exactly one robot"), "{error:#}");
        Ok(())
    }

    #[test]
    fn deploy_robot_root_is_the_active_generation() {
        // Regression: deployed participants read robot.yaml/structure.urdf via
        // `$PHOXAL_ROBOT_ROOT`, and those files are staged per-generation under
        // `/opt/phoxal/active/` (the transactional-release symlink). Pointing the
        // root at the flat `/opt/phoxal` made every participant on a real robot
        // die with "failed to read robot file /opt/phoxal/robot.yaml". It must
        // resolve through the active generation, matching deploy's ACTIVE_ROOT.
        assert_eq!(
            robot_root_for_mode(&LaunchMode::Deploy, Path::new("/tmp/project")),
            PathBuf::from("/opt/phoxal/active"),
        );
    }

    #[test]
    fn multi_robot_listen_endpoints_collapse_and_conflicts_error() -> anyhow::Result<()> {
        let mut left = empty_resolved_robot("left")?;
        add_site_tools(&mut left);
        left.robot.bus.listen = vec![
            "serial//dev/ttyUSB0?baudrate=115200".to_string(),
            "tcp/127.0.0.1:7448".to_string(),
        ];
        let mut right = empty_resolved_robot("right")?;
        add_site_tools(&mut right);
        right.robot.bus.listen = vec![
            "serial//dev/ttyUSB0?baudrate=115200".to_string(),
            "tcp/127.0.0.1:7448".to_string(),
        ];
        let extras = RobotManifestExtras::default();
        let inputs = [
            empty_checked_input(Path::new("/tmp/left"), &left, &extras),
            empty_checked_input(Path::new("/tmp/right"), &right, &extras),
        ];
        let plan = build_launch_plan(
            LaunchMode::Webots {
                world: PathBuf::from("worlds/test.wbt"),
            },
            &inputs,
        )?;
        assert_eq!(
            plan.site[0].phoxal_config,
            serde_json::json!({
                "listen": [
                    "serial//dev/ttyUSB0?baudrate=115200",
                    "tcp/127.0.0.1:7448"
                ]
            })
        );

        right.robot.bus.listen = vec!["serial//dev/ttyUSB0?baudrate=9600".to_string()];
        let inputs = [
            empty_checked_input(Path::new("/tmp/left"), &left, &extras),
            empty_checked_input(Path::new("/tmp/right"), &right, &extras),
        ];
        let error = build_launch_plan(
            LaunchMode::Webots {
                world: PathBuf::from("worlds/test.wbt"),
            },
            &inputs,
        )
        .expect_err("same serial device with different options conflicts");
        assert!(
            error
                .to_string()
                .contains("conflicting router listen claims"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn parity_rejects_missing_and_extra_checked_metadata() -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("robot_v1")?;
        add_site_tools(&mut resolved);
        resolved
            .user_runtimes
            .push(crate::resolver::ResolvedUserRuntime {
                name: "mission".to_string(),
                path: PathBuf::from("runtimes/mission"),
                source_hash: "hash".to_string(),
            });
        let extras = RobotManifestExtras::default();
        let sources = vec![SourceParticipant::user_service(
            "mission",
            PathBuf::from("/tmp/mission"),
        )];
        let checked = vec![participant(
            "other",
            "other",
            graph_check::ParticipantScope::Graph,
        )];
        let error = build_launch_plan(
            LaunchMode::Run,
            &[CheckedRobotLaunchInput {
                project_root: Path::new("/tmp/robot"),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &checked,
                substitutions: &[],
                source_participants: &sources,
            }],
        )
        .expect_err("parity should fail");
        let message = error.to_string();
        assert!(message.contains("mission"), "{message}");
        assert!(message.contains("other"), "{message}");
        Ok(())
    }

    fn participant(
        participant_id: &str,
        artifact_id: &str,
        scope: graph_check::ParticipantScope,
    ) -> graph_check::ParticipantApis {
        graph_check::ParticipantApis {
            participant_id: participant_id.to_string(),
            artifact_id: artifact_id.to_string(),
            participant_kind: graph_check::ParticipantKind::Service,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "y2026_1".to_string(),
            config_schema: None,
            scope,
            contracts: Vec::new(),
        }
    }

    fn raw_emit_apis(kind: &str, id: &str) -> RawEmitApis {
        RawEmitApis {
            artifact: RawArtifact {
                kind: kind.to_string(),
                id: id.to_string(),
            },
            participant_class: "checked".to_string(),
            api_version: "y2026_1".to_string(),
            required_contracts: Vec::new(),
            config_schema: None,
        }
    }

    fn empty_checked_input<'a>(
        project_root: &'a Path,
        resolved: &'a ResolvedRobot,
        manifest_extras: &'a RobotManifestExtras,
    ) -> CheckedRobotLaunchInput<'a> {
        CheckedRobotLaunchInput {
            project_root,
            resolved,
            manifest_extras,
            checked_participants: &[],
            substitutions: &[],
            source_participants: &[],
        }
    }

    fn empty_resolved_robot(id: &str) -> anyhow::Result<ResolvedRobot> {
        let yaml = format!(
            r#"schema: robot/v0
robot:
  id: {id}
  namespace: dev
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {{}}
"#
        );
        let robot = phoxal::model::robot::v0::Robot::parse_from_string(&yaml)?;
        Ok(ResolvedRobot {
            robot,
            channel: crate::catalog::SelectionChannel::Stable,
            target: host_target_triple(),
            catalog_snapshot: None,
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    fn add_site_tools(resolved: &mut ResolvedRobot) {
        resolved.tools.push(tool(SITE_TOOL_ROUTER));
        resolved.tools.push(tool(SITE_TOOL_JOYPAD));
    }

    fn tool(name: &str) -> ResolvedTool {
        ResolvedTool {
            name: name.to_string(),
            package: format!("phoxal/{name}"),
            requested: "0.1.0".to_string(),
            resolved: "0.1.0".to_string(),
            repo: "phoxal/framework".to_string(),
            asset: format!("{name}-0.1.0-{}.tar.gz", host_target_triple()),
            binary_name: name.to_string(),
            sha256: "0".repeat(64),
            url: None,
            size: None,
            published: false,
            path_override: None,
            channel: crate::catalog::SelectionChannel::Stable,
            target: host_target_triple(),
        }
    }

    const FIXTURE_ROBOT: &str = r#"schema: robot/v0
robot:
  id: robot_v1
  namespace: dev
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
    right_drive:
      component: ddsm115
      mount_link: right_wheel
      driver:
        connection: { type: can, bus: 0, node_id: 2 }
bus:
  listen:
    - serial//dev/ttyUSB0?baudrate=115200
services:
  mission:
    path: runtimes/mission
"#;
}
