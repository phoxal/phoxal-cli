use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use phoxal::behavior::{BehaviorCatalog, Node, ValueType};
use phoxal::bus::{ContractBody, LogicalTime, Publish, Publisher, Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v0_2 as api;
use serde::Serialize;
use tokio::time::timeout;

use crate::AppContext;
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::project::resolver::{discover_robot_yaml, load_robot_with_extras};

#[derive(Debug, Args)]
pub struct Behavior {
    #[command(subcommand)]
    pub command: BehaviorSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum BehaviorSubcommand {
    #[command(about = "Validate every behavior definition and configured root.")]
    Validate(ValidateArgs),
    #[command(about = "Run a deterministic behavior scenario without robot services.")]
    Test(TestArgs),
    #[command(about = "Request a behavior execution from a running robot.")]
    Request(RequestArgs),
    #[command(about = "Inspect the latest behavior execution snapshot.")]
    Inspect(InspectArgs),
    #[command(about = "Pause the active behavior execution.")]
    Pause(ControlArgs),
    #[command(about = "Resume the active behavior execution.")]
    Resume(ControlArgs),
    #[command(about = "Cancel the active behavior execution.")]
    Cancel(ControlArgs),
}

#[derive(Debug, Args)]
pub struct ValidateArgs {
    #[arg(
        long,
        value_name = "PATH",
        help = "Explicit robot root or robot.yaml path."
    )]
    pub robot: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct TestArgs {
    pub behavior_id: String,
    #[arg(long = "arg", value_name = "KEY=VALUE")]
    pub args: Vec<String>,
    #[arg(long, default_value = "happy")]
    pub scenario: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ConflictPolicy {
    Reject,
    Queue,
    Interrupt,
}

#[derive(Debug, Args)]
pub struct RequestArgs {
    pub behavior_id: String,
    #[arg(long = "arg", value_name = "KEY=VALUE")]
    pub args: Vec<String>,
    #[arg(long, default_value_t = 0)]
    pub priority: u8,
    #[arg(long, value_enum, default_value_t = ConflictPolicy::Reject)]
    pub conflict: ConflictPolicy,
    #[arg(long, default_value = DEFAULT_ROUTER_CONNECT)]
    pub connect: String,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    #[arg(default_value = "latest")]
    pub execution: String,
    #[arg(long, default_value = DEFAULT_ROUTER_CONNECT)]
    pub connect: String,
}

#[derive(Debug, Args)]
pub struct ControlArgs {
    #[arg(long, default_value = DEFAULT_ROUTER_CONNECT)]
    pub connect: String,
}

#[derive(Debug, Serialize)]
struct ValidationSummary {
    root: Option<String>,
    definitions: Vec<DefinitionSummary>,
}

#[derive(Debug, Serialize)]
struct DefinitionSummary {
    id: String,
    version: String,
    content_hash: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct TestSummary {
    behavior_id: String,
    scenario: String,
    outcome: String,
    args: BTreeMap<String, api::behavior::Value>,
    visited_nodes: Vec<String>,
}

#[derive(Clone, Copy)]
enum FakeScenario {
    Success,
    Failure,
    Refusal,
    Timeout,
    Cancellation,
}

impl FakeScenario {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "happy" | "success" => Ok(Self::Success),
            "failure" => Ok(Self::Failure),
            "refusal" => Ok(Self::Refusal),
            "timeout" => Ok(Self::Timeout),
            "cancel" | "cancellation" => Ok(Self::Cancellation),
            other => bail!(
                "unknown scenario '{other}'; use happy, failure, refusal, timeout, or cancellation"
            ),
        }
    }

    fn action_outcome(self) -> &'static str {
        match self {
            Self::Success => "succeeded",
            Self::Failure => "failed",
            Self::Refusal => "refused",
            Self::Timeout => "timed_out",
            Self::Cancellation => "cancelled",
        }
    }
}

impl Behavior {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            BehaviorSubcommand::Validate(args) => validate(app, args),
            BehaviorSubcommand::Test(args) => test(app, args),
            BehaviorSubcommand::Request(args) => request(app, args).await,
            BehaviorSubcommand::Inspect(args) => inspect(app, args).await,
            BehaviorSubcommand::Pause(args) => {
                control(app, args, api::behavior::Command::Pause).await
            }
            BehaviorSubcommand::Resume(args) => {
                control(app, args, api::behavior::Command::Resume).await
            }
            BehaviorSubcommand::Cancel(args) => {
                control(app, args, api::behavior::Command::Cancel).await
            }
        }
    }
}

fn validate(app: &AppContext, args: &ValidateArgs) -> Result<()> {
    let root = explicit_root(app, args.robot.as_ref())?;
    let robot_path = discover_robot_yaml(&root)?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let suite = BehaviorCatalog::load(&root)?;
    let configured = loaded
        .robot
        .behavior
        .as_ref()
        .map(|behavior| behavior.root.clone());
    if let Some(root_id) = &configured {
        suite.validate_root(root_id)?;
    }
    let summary = ValidationSummary {
        root: configured,
        definitions: suite
            .iter()
            .map(|(_, definition)| DefinitionSummary {
                id: definition.authored.id.clone(),
                version: definition.authored.version.clone(),
                content_hash: definition.content_hash.clone(),
                path: definition.source.display().to_string(),
            })
            .collect(),
    };
    println!(
        "ok: {} behavior definition(s) validated",
        summary.definitions.len()
    );
    if let Some(root) = &summary.root {
        println!("root: {root}");
    }
    for definition in &summary.definitions {
        println!(
            "  {} {} {}",
            definition.id, definition.version, definition.content_hash
        );
    }
    Ok(())
}

fn test(app: &AppContext, args: &TestArgs) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root())?;
    let root = robot_path.parent().context("robot.yaml has no parent")?;
    let suite = BehaviorCatalog::load(root)?;
    let definition = suite
        .get(&args.behavior_id)
        .with_context(|| format!("unknown behavior '{}'", args.behavior_id))?;
    let parsed = parse_args(&args.args)?;
    validate_test_args(definition, &parsed)?;
    let scenario = FakeScenario::parse(&args.scenario)?;
    let mut visited_nodes = Vec::new();
    let succeeded = run_fake_node(
        &suite,
        &definition.authored.root,
        &definition.authored.id,
        scenario,
        &mut visited_nodes,
    )?;
    let outcome = if succeeded {
        "succeeded"
    } else {
        scenario.action_outcome()
    };
    let summary = TestSummary {
        behavior_id: args.behavior_id.clone(),
        scenario: args.scenario.clone(),
        outcome: outcome.to_string(),
        args: parsed,
        visited_nodes,
    };
    println!(
        "{}: {} ({}, {} node(s))",
        summary.behavior_id,
        summary.outcome,
        summary.scenario,
        summary.visited_nodes.len()
    );
    Ok(())
}

fn validate_test_args(
    definition: &phoxal::behavior::BehaviorDefinition,
    args: &BTreeMap<String, api::behavior::Value>,
) -> Result<()> {
    for name in args.keys() {
        if !definition.authored.inputs.contains_key(name) {
            bail!(
                "behavior '{}' does not declare arg '{name}'",
                definition.authored.id
            );
        }
    }
    for (name, kind) in &definition.authored.inputs {
        let value = args.get(name).with_context(|| {
            format!(
                "behavior '{}' requires arg '{name}'",
                definition.authored.id
            )
        })?;
        let matches = matches!(
            (kind, value),
            (ValueType::Bool, api::behavior::Value::Bool(_))
                | (ValueType::Integer, api::behavior::Value::Integer(_))
                | (ValueType::Number, api::behavior::Value::Number(_))
                | (ValueType::String, api::behavior::Value::String(_))
                | (ValueType::Pose, api::behavior::Value::Pose(_))
        );
        if !matches {
            bail!(
                "behavior '{}' arg '{name}' has the wrong type",
                definition.authored.id
            );
        }
    }
    Ok(())
}

fn run_fake_node(
    suite: &BehaviorCatalog,
    node: &Node,
    parent: &str,
    scenario: FakeScenario,
    visited: &mut Vec<String>,
) -> Result<bool> {
    let path = format!("{parent}/{}", node.id());
    visited.push(path.clone());
    match node {
        Node::Sequence { children, .. } => {
            for child in children {
                if !run_fake_node(suite, child, &path, scenario, visited)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Node::Selector { children, .. } | Node::ReactiveSelector { children, .. } => {
            for child in children {
                if run_fake_node(suite, child, &path, scenario, visited)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Node::Condition { .. } | Node::Action { .. } => {
            Ok(matches!(scenario, FakeScenario::Success))
        }
        Node::Wait { .. } => Ok(!matches!(scenario, FakeScenario::Timeout)),
        Node::Timeout { child, .. } => {
            if matches!(scenario, FakeScenario::Timeout) {
                Ok(false)
            } else {
                run_fake_node(suite, child, &path, scenario, visited)
            }
        }
        Node::Retry {
            attempts, child, ..
        } => {
            for _ in 0..*attempts {
                if run_fake_node(suite, child, &path, scenario, visited)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Node::Subtree { behavior, .. } => {
            let definition = suite
                .get(behavior)
                .with_context(|| format!("subtree '{behavior}' is not in the validated suite"))?;
            run_fake_node(
                suite,
                &definition.authored.root,
                &format!("{path}::{behavior}"),
                scenario,
                visited,
            )
        }
    }
}

async fn request(app: &AppContext, args: &RequestArgs) -> Result<()> {
    let (bus, at) = open_bus(app, &args.connect, "phoxal-cli-behavior-request").await?;
    let topic = Topic::<Publish<api::behavior::Request>>::new_owned(
        api::topic::new()
            .behavior()
            .request()
            .publish_key()?
            .to_owned(),
    );
    let publisher = Publisher::new(bus.clone(), &topic)?;
    let request_id = format!("cli-{}", at.time_ns());
    publisher
        .publish_at(
            at,
            api::behavior::Request {
                request_id: api::behavior::RequestId {
                    value: request_id.clone(),
                },
                behavior_id: args.behavior_id.clone(),
                args: parse_args(&args.args)?,
                priority: args.priority,
                conflict_policy: match args.conflict {
                    ConflictPolicy::Reject => api::behavior::ConflictPolicy::Reject,
                    ConflictPolicy::Queue => api::behavior::ConflictPolicy::Queue,
                    ConflictPolicy::Interrupt => api::behavior::ConflictPolicy::Interrupt,
                },
            },
        )
        .await?;
    bus.close().await?;
    app.ui
        .info(format!("behavior request {request_id} published"));
    Ok(())
}

async fn inspect(app: &AppContext, args: &InspectArgs) -> Result<()> {
    if args.execution != "latest" {
        bail!("v0 supports only `behavior inspect latest`");
    }
    let (bus, _) = open_bus(app, &args.connect, "phoxal-cli-behavior-inspect").await?;
    let topic = Topic::<Subscribe<api::behavior::Snapshot>>::new_static(
        <api::behavior::Snapshot as ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let snapshot = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("behavior service did not publish a snapshot")??
        .body;
    println!(
        "execution: {}",
        snapshot.execution_id.as_deref().unwrap_or("idle")
    );
    println!("status: {:?}", snapshot.status);
    if let Some(path) = &snapshot.active_node_path {
        println!("active node: {path}");
    }
    bus.close().await?;
    Ok(())
}

async fn control(
    app: &AppContext,
    args: &ControlArgs,
    command: api::behavior::Command,
) -> Result<()> {
    let (bus, at) = open_bus(app, &args.connect, "phoxal-cli-behavior-control").await?;
    let topic = Topic::<Publish<api::behavior::Command>>::new_owned(
        api::topic::new()
            .behavior()
            .command()
            .publish_key()?
            .to_owned(),
    );
    Publisher::new(bus.clone(), &topic)?
        .publish_at(at, command)
        .await?;
    bus.close().await?;
    app.ui.info("behavior control command published");
    Ok(())
}

async fn open_bus(
    app: &AppContext,
    connect: &str,
    participant: &str,
) -> Result<(Bus, LogicalTime)> {
    let robot_path = discover_robot_yaml(app.project.root())?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let bus = Bus::open(BusConfig {
        namespace: loaded.robot.robot.namespace,
        robot_id: loaded.robot.robot.id,
        participant: participant.to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect.to_string()],
    })
    .await?;
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Ok((
        bus,
        LogicalTime::new(0, u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)),
    ))
}

fn explicit_root(app: &AppContext, explicit: Option<&PathBuf>) -> Result<PathBuf> {
    let start = explicit.map_or_else(|| app.project.root().to_path_buf(), Clone::clone);
    let path = discover_robot_yaml(&start)?;
    Ok(path
        .parent()
        .context("robot.yaml has no parent")?
        .to_path_buf())
}

fn parse_args(values: &[String]) -> Result<BTreeMap<String, api::behavior::Value>> {
    values
        .iter()
        .map(|raw| {
            let (key, value) = raw
                .split_once('=')
                .with_context(|| format!("behavior arg '{raw}' must be KEY=VALUE"))?;
            if key.is_empty() {
                bail!("behavior arg key cannot be empty");
            }
            Ok((key.to_string(), parse_value(value)?))
        })
        .collect()
}

fn parse_value(value: &str) -> Result<api::behavior::Value> {
    if let Ok(value) = value.parse::<bool>() {
        return Ok(api::behavior::Value::Bool(value));
    }
    if let Ok(value) = value.parse::<i64>() {
        return Ok(api::behavior::Value::Integer(value));
    }
    if let Ok(value) = value.parse::<f64>() {
        return Ok(api::behavior::Value::Number(value));
    }
    if value.starts_with('{') {
        return Ok(api::behavior::Value::Pose(serde_json::from_str(value)?));
    }
    Ok(api::behavior::Value::String(value.to_string()))
}
