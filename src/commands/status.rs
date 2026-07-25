use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use phoxal::bus::{Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v0_1 as api;
use tokio::time::timeout;

use crate::AppContext;
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::project::resolver::{discover_robot_yaml, load_robot};

#[derive(Debug, Args)]
pub struct Status {
    #[command(subcommand)]
    pub command: StatusSubcommand,
}

// There is no software emergency-stop command. Every emergency stop is a
// manifest-declared component under the ordinary communication rules (#952
// section A), so the CLI has no privileged latch to engage: a robot with
// e-stop hardware declared is stopped through that hardware, and a robot
// without it has no robot-wide software stop by design.
#[derive(Debug, Subcommand)]
pub enum StatusSubcommand {
    #[command(about = "Inspect the latest domain-native safety constraints.")]
    Safety(SafetyArg),
    #[command(about = "Inspect the latest domain-native motion arbitration state.")]
    Motion(SafetyArg),
    #[command(about = "Inspect the latest domain-native localization estimate.")]
    Localization(SafetyArg),
}

#[derive(Debug, Args)]
pub struct SafetyArg {
    #[arg(
        long,
        value_name = "ENDPOINT",
        default_value = DEFAULT_ROUTER_CONNECT,
        help = "Router endpoint to connect to."
    )]
    pub connect: String,
}

impl Status {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        run_action(&self.command, app).await
    }
}

/// Open an observer session on the **running** execution.
///
/// The bus key root is execution-scoped (#952 section B), so an inspector that
/// minted its own `ExecutionId` would subscribe a root nobody publishes on and
/// silently time out. Refusing when nothing is running is the honest answer.
async fn inspect_bus(
    app: &AppContext,
    robot: phoxal::model::robot::v0::Robot,
    participant: &str,
    connect: &str,
) -> Result<Bus> {
    let execution = crate::supervisor::active_execution(app.project.root())?
        .context("no phoxal run is active; start one before inspecting live state")?;
    Ok(Bus::open(BusConfig {
        namespace: robot.robot.namespace,
        robot_id: robot.robot.id,
        execution,
        participant: participant.to_string(),
        producer: phoxal::bus::ProducerId::mint(),
        connect_endpoints: vec![connect.to_string()],
    })
    .await?)
}

async fn run_action(command: &StatusSubcommand, app: &AppContext) -> Result<()> {
    match command {
        StatusSubcommand::Safety(arg) => {
            return inspect_safety(app, arg).await;
        }
        StatusSubcommand::Motion(arg) => {
            return inspect_motion(app, arg).await;
        }
        StatusSubcommand::Localization(arg) => inspect_localization(app, arg).await,
    }
}

async fn inspect_localization(app: &AppContext, arg: &SafetyArg) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root())?;
    let robot = load_robot(&robot_path)?;
    let bus = inspect_bus(app, robot, "phoxal-cli-localization-inspect", &arg.connect).await?;
    let topic = Topic::<Subscribe<api::localize::LocalizationState>>::new_static(
        <api::localize::LocalizationState as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let state = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("localize did not publish domain state")??
        .body;
    println!(
        "localization: ({:.3}, {:.3}) yaw={:.3} confidence={:.3}",
        state.x_m, state.y_m, state.yaw_rad, state.confidence
    );
    bus.close().await?;
    Ok(())
}

async fn inspect_motion(app: &AppContext, arg: &SafetyArg) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root())?;
    let robot = load_robot(&robot_path)?;
    let bus = inspect_bus(app, robot, "phoxal-cli-motion-inspect", &arg.connect).await?;
    let topic = Topic::<Subscribe<api::motion::State>>::new_static(
        <api::motion::State as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let state = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("motion did not publish domain state")??
        .body;
    println!(
        "motion: source={:?} target=({:.3} m/s, {:.3} rad/s) zero={:?}",
        state.selected_source,
        state.final_target.linear_x_mps,
        state.final_target.angular_z_radps,
        state.zero_reason
    );
    println!(
        "  component_estop_blocked={} safety_constraints={}",
        state.component_estop_blocked,
        state.active_safety_constraints.len()
    );
    bus.close().await?;
    Ok(())
}

async fn inspect_safety(app: &AppContext, arg: &SafetyArg) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root())?;
    let robot = load_robot(&robot_path)?;
    let bus = inspect_bus(app, robot, "phoxal-cli-safety-inspect", &arg.connect).await?;
    let topic = Topic::<Subscribe<api::safety::State>>::new_static(
        <api::safety::State as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let state = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("safety did not publish domain state")??
        .body;
    println!(
        "safety: {} (sequence {}, expires at {}ns)",
        if state.clear { "clear" } else { "constrained" },
        state.motion.sequence,
        state.motion.expires_at_ns
    );
    for constraint in &state.motion.constraints {
        println!(
            "  {:?} from {} stop={} linear_limit={:?} observed={:?}",
            constraint.reason,
            constraint.source.participant_id,
            constraint.stop,
            constraint.max_linear_speed_mps,
            constraint.observed_value
        );
    }
    bus.close().await?;
    Ok(())
}
