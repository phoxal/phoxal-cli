use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use phoxal::bus::{LogicalTime, Publish, Publisher, Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v0_2 as api;
use tokio::time::timeout;

use crate::AppContext;
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::project::resolver::{discover_robot_yaml, load_robot};

#[derive(Debug, Args)]
pub struct Status {
    #[command(subcommand)]
    pub command: StatusSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum StatusSubcommand {
    #[command(about = "Engage the robot-wide software emergency stop.")]
    EngageEstop(EmergencyStopArg),
    #[command(about = "Reset the robot-wide software emergency stop.")]
    ResetEstop(EmergencyStopArg),
    #[command(about = "Inspect the latest domain-native safety constraints.")]
    Safety(SafetyArg),
    #[command(about = "Inspect the latest domain-native motion arbitration state.")]
    Motion(SafetyArg),
    #[command(about = "Inspect the latest domain-native localization estimate.")]
    Localization(SafetyArg),
}

#[derive(Debug, Args)]
pub struct EmergencyStopArg {
    #[arg(
        long,
        value_name = "ENDPOINT",
        default_value = DEFAULT_ROUTER_CONNECT,
        help = "Router endpoint to connect to."
    )]
    pub connect: String,
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

async fn run_action(command: &StatusSubcommand, app: &AppContext) -> Result<()> {
    match command {
        StatusSubcommand::EngageEstop(arg) => {
            return publish_estop(app, arg, true).await;
        }
        StatusSubcommand::ResetEstop(arg) => {
            return publish_estop(app, arg, false).await;
        }
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
    let bus = Bus::open(BusConfig {
        namespace: robot.robot.namespace,
        robot_id: robot.robot.id,
        participant: "phoxal-cli-localization-inspect".to_string(),
        incarnation: 0,
        connect_endpoints: vec![arg.connect.clone()],
    })
    .await?;
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
    let bus = Bus::open(BusConfig {
        namespace: robot.robot.namespace,
        robot_id: robot.robot.id,
        participant: "phoxal-cli-motion-inspect".to_string(),
        incarnation: 0,
        connect_endpoints: vec![arg.connect.clone()],
    })
    .await?;
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
        "  software_estop={} component_estop_blocked={} safety_constraints={}",
        state.software_estop_engaged,
        state.component_estop_blocked,
        state.active_safety_constraints.len()
    );
    bus.close().await?;
    Ok(())
}

async fn inspect_safety(app: &AppContext, arg: &SafetyArg) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root())?;
    let robot = load_robot(&robot_path)?;
    let bus = Bus::open(BusConfig {
        namespace: robot.robot.namespace,
        robot_id: robot.robot.id,
        participant: "phoxal-cli-safety-inspect".to_string(),
        incarnation: 0,
        connect_endpoints: vec![arg.connect.clone()],
    })
    .await?;
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

async fn publish_estop(app: &AppContext, arg: &EmergencyStopArg, engaged: bool) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root()).with_context(|| {
        format!(
            "failed to find robot.yaml from {}",
            app.project.root().display()
        )
    })?;
    let robot = load_robot(&robot_path)?;
    let bus = Bus::open(BusConfig {
        namespace: robot.robot.namespace,
        robot_id: robot.robot.id,
        participant: "phoxal-cli-estop".to_string(),
        incarnation: 0,
        connect_endpoints: vec![arg.connect.clone()],
    })
    .await?;
    let topic = Topic::<Publish<api::motion::EmergencyStopRequest>>::new_owned(
        api::topic::new().motion().estop().publish_key()?.to_owned(),
    );
    let state_topic = Topic::<Subscribe<api::motion::State>>::new_owned(
        api::topic::new().motion().state().key().to_owned(),
    );
    let states = Subscriber::new(&bus, &state_topic, 32).await?;
    let publisher = Publisher::new(bus.clone(), &topic)?;
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    publisher
        .publish_at(
            LogicalTime::new(0, u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)),
            api::motion::EmergencyStopRequest { engaged },
        )
        .await?;
    timeout(Duration::from_secs(3), async {
        loop {
            let state = states.recv().await?;
            if state.body.software_estop_engaged == engaged {
                return Ok::<(), phoxal::bus::BusError>(());
            }
        }
    })
    .await
    .with_context(|| {
        if engaged {
            "motion did not acknowledge the software emergency stop"
        } else {
            "motion did not acknowledge the software emergency-stop reset"
        }
    })??;
    bus.close().await?;
    app.ui.info(if engaged {
        "software emergency stop engaged"
    } else {
        "software emergency stop reset"
    });
    Ok(())
}
