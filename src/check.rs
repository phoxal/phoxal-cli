//! Graph validation for `phoxal-cli check` (D59/D63).
//!
//! This is the pure core: given the `emit-apis` report of every participant in a
//! robot graph plus the manifest's root `api_version`, it enforces the
//! single-API-version invariant and topology cardinality. It is deliberately
//! independent of how the reports are obtained (resolved images, local binaries),
//! so it is fully unit-testable without Docker or a registry.
//!
//! Two invariants:
//!
//! 1. **Single API version (D59/D63).** Every normal participant must report the
//!    graph's root `api_version`. The manifest's `api_version` is selection intent;
//!    the artifacts are the source of truth, so a mismatch is reported against the
//!    artifact that disagrees.
//! 2. **Topology cardinality.** Every contract a participant *consumes* (subscribes,
//!    or queries as a client) must have at least one *producer* in the graph (a
//!    publisher, or a server of that query). A consumed contract with no producer is
//!    a dangling edge.

use std::collections::{BTreeMap, BTreeSet};

/// One participant's `emit-apis` report, reduced to what graph validation needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantApis {
    /// The concrete participant/instance id used for graph membership and
    /// diagnostics. For most participants this equals `artifact_id`, but a
    /// component driver is launched once per component instance, so several
    /// instances of the same driver share one `artifact_id` yet must remain
    /// distinct nodes in the graph (e.g. `left_drive`, `right_drive`).
    pub participant_id: String,
    /// The artifact id (`emit-apis` `artifact.id`), e.g. `"drive"`. Kept for
    /// artifact-identity validation; not used to key the topology graph.
    pub artifact_id: String,
    /// The API version the artifact reports (`emit-apis` `api_version`).
    pub api_version: String,
    /// The framework-owned bus ABI reported by the artifact, if present.
    pub bus_abi: Option<String>,
    /// The artifact's emitted config schema, preserved for later validation.
    pub config_schema: Option<serde_json::Value>,
    /// The manifest scope this participant is launched under. Normal runtimes
    /// see the whole graph; component drivers are launched once per component
    /// instance and dynamic component topics must be materialized only for that
    /// instance.
    pub scope: ParticipantScope,
    /// The contracts the artifact participates in.
    pub contracts: Vec<Contract>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ParticipantScope {
    #[default]
    Graph,
    ComponentInstance(String),
}

/// One `{family, topic, direction}` contract use from an `emit-apis` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contract {
    pub family: String,
    pub topic: String,
    pub direction: Direction,
}

/// The direction a participant uses a contract on a topic. Mirrors the framework's
/// `emit-apis` `direction` vocabulary (snake_case on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Direction {
    Publish,
    Subscribe,
    QueryRequest,
    QueryResponse,
    ServerRequest,
    ServerResponse,
}

impl Direction {
    /// Parse the snake_case `emit-apis` direction string. Unknown strings return
    /// `None` so the caller can surface a forward-incompatible report rather than
    /// silently misclassifying it.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "publish" => Self::Publish,
            "subscribe" => Self::Subscribe,
            "query_request" => Self::QueryRequest,
            "query_response" => Self::QueryResponse,
            "server_request" => Self::ServerRequest,
            "server_response" => Self::ServerResponse,
            _ => return None,
        })
    }

    /// Whether this direction *provides* a contract to the graph: a publisher of a
    /// pub/sub topic, or the server side of a query.
    #[must_use]
    pub const fn is_producer(self) -> bool {
        matches!(
            self,
            Self::Publish | Self::ServerRequest | Self::ServerResponse
        )
    }

    /// The producer direction that must exist for this consumer direction to be
    /// satisfied — matched by *kind*, so a pub/sub `publish` cannot stand in for a
    /// query server. `None` for non-consumer directions.
    #[must_use]
    pub const fn required_producer(self) -> Option<Self> {
        Some(match self {
            Self::Subscribe => Self::Publish,
            Self::QueryRequest => Self::ServerRequest,
            Self::QueryResponse => Self::ServerResponse,
            _ => return None,
        })
    }
}

/// A problem found while validating a robot graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// A participant reports an API version other than the graph's root.
    ApiVersionMismatch {
        artifact_id: String,
        expected: String,
        found: String,
    },
    /// A consumed contract has no producer anywhere in the graph.
    MissingProducer {
        family: String,
        topic: String,
        /// Artifacts that consume the contract (sorted, de-duplicated).
        consumers: Vec<String>,
    },
    /// A query/server contract has no peer anywhere in the graph.
    MissingConsumer {
        family: String,
        topic: String,
        /// Artifacts that produce the contract (sorted, de-duplicated).
        producers: Vec<String>,
    },
    /// A user runtime's manifest config does not match its emitted JSON Schema.
    InvalidConfig {
        runtime_id: String,
        errors: Vec<String>,
    },
    /// A participant declares a `component/{instance}/...` template contract that
    /// expands to no concrete component capability in scope. The template can
    /// never bind to a real topic, so it is a hard error rather than a silent
    /// literal match between two placeholder topics.
    UnresolvedComponentTemplate {
        /// The artifact that declared the template (the concrete participant /
        /// instance id, not the emitted artifact id).
        artifact_id: String,
        /// The raw template topic, e.g. `component/{instance}/motor/{capability}/command`.
        template: String,
        family: String,
        /// What concrete candidate was missing for the template to expand:
        /// either a component instance (component-driver scope) or, in graph
        /// scope, the kind of capability the manifest has none of.
        missing: String,
    },
}

/// A non-fatal topology issue. Observable-only publishers are valid, so these
/// are surfaced as warnings rather than blocking `phoxal-cli check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    MissingConsumer {
        family: String,
        topic: String,
        /// Artifacts that produce the contract (sorted, de-duplicated).
        producers: Vec<String>,
    },
}

/// The outcome of validating a graph: the problems found (empty == healthy).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub problems: Vec<Problem>,
    pub warnings: Vec<Warning>,
}

impl Report {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RobotGraph {
    pub component_capabilities: Vec<ComponentCapability>,
    pub motion_capabilities: BTreeSet<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComponentCapability {
    pub instance: String,
    pub capability: String,
    pub kind: String,
}

/// Validate a robot graph: single-API-version (D59/D63) + topology cardinality.
///
/// `participants` is every normal participant's `emit-apis` report;
/// `root_api_version` is the manifest's root `api_version`. Problems are returned in
/// a stable order (api-version mismatches first, by artifact; then missing producers
/// by family+topic) so output and tests are deterministic.
#[must_use]
pub fn check_graph(participants: &[ParticipantApis], root_api_version: &str) -> Report {
    check_graph_with_topology(participants, root_api_version, &RobotGraph::default())
}

#[must_use]
pub fn check_graph_with_topology(
    participants: &[ParticipantApis],
    root_api_version: &str,
    robot_graph: &RobotGraph,
) -> Report {
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    // Expand component templates to concrete manifest topics. Templates that
    // cannot expand surface as hard `UnresolvedComponentTemplate` problems here
    // rather than leaking placeholder topics into the topology graph (where two
    // sides would otherwise "match" literally with no real binding).
    let MaterializedGraph {
        participants,
        problems: mut template_problems,
    } = materialize_participants(participants, robot_graph);
    template_problems.sort_by_key(problem_sort_key);
    problems.append(&mut template_problems);

    // 1. Single API version — report each disagreeing participant, in id order.
    let mut mismatches: Vec<&ParticipantApis> = participants
        .iter()
        .filter(|p| p.api_version != root_api_version)
        .collect();
    mismatches.sort_by(|a, b| a.participant_id.cmp(&b.participant_id));
    for p in mismatches {
        problems.push(Problem::ApiVersionMismatch {
            artifact_id: p.participant_id.clone(),
            expected: root_api_version.to_string(),
            found: p.api_version.clone(),
        });
    }

    // 2. Topology cardinality. Producers/consumers are keyed by
    // (family, concrete topic, direction) so matching is by *kind*: a `subscribe`
    // needs a `publish`, and a query client needs a server. Dynamic component
    // templates have already been expanded to manifest-derived concrete topics;
    // any topic still containing a placeholder was reported above and is skipped.
    let mut by_direction: BTreeMap<(String, String, Direction), BTreeSet<String>> = BTreeMap::new();
    for p in &participants {
        for c in &p.contracts {
            if is_template_topic(&c.topic) {
                continue;
            }
            by_direction
                .entry((c.family.clone(), c.topic.clone(), c.direction))
                .or_default()
                .insert(p.participant_id.clone());
        }
    }

    // Collect consumers per unmet contract, de-duplicated and ordered.
    let mut unmet: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for p in &participants {
        for c in &p.contracts {
            if is_template_topic(&c.topic) {
                continue;
            }
            if let Some(required) = c.direction.required_producer() {
                let needed = (c.family.clone(), c.topic.clone(), required);
                if !by_direction.contains_key(&needed) {
                    unmet
                        .entry((c.family.clone(), c.topic.clone()))
                        .or_default()
                        .insert(p.participant_id.clone());
                }
            }
        }
    }

    for ((family, topic), consumers) in unmet {
        problems.push(Problem::MissingProducer {
            family,
            topic,
            consumers: consumers.into_iter().collect(),
        });
    }

    let mut missing_query_consumers: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut missing_pubsub_consumers: BTreeMap<(String, String), BTreeSet<String>> =
        BTreeMap::new();
    for ((family, topic, direction), producers) in &by_direction {
        if let Some(required_consumer) = direction.required_consumer() {
            let needed = (family.clone(), topic.clone(), required_consumer);
            if !by_direction.contains_key(&needed) {
                let missing_consumers = if direction.is_query_server() {
                    &mut missing_query_consumers
                } else {
                    &mut missing_pubsub_consumers
                };
                missing_consumers
                    .entry((family.clone(), topic.clone()))
                    .or_default()
                    .extend(producers.iter().cloned());
            }
        }
    }
    for ((family, topic), producers) in missing_query_consumers {
        problems.push(Problem::MissingConsumer {
            family,
            topic,
            producers: producers.into_iter().collect(),
        });
    }
    for ((family, topic), producers) in missing_pubsub_consumers {
        warnings.push(Warning::MissingConsumer {
            family,
            topic,
            producers: producers.into_iter().collect(),
        });
    }

    Report { problems, warnings }
}

impl Direction {
    #[must_use]
    pub const fn required_consumer(self) -> Option<Self> {
        Some(match self {
            Self::Publish => Self::Subscribe,
            Self::ServerRequest => Self::QueryRequest,
            Self::ServerResponse => Self::QueryResponse,
            _ => return None,
        })
    }

    #[must_use]
    const fn is_query_server(self) -> bool {
        matches!(self, Self::ServerRequest | Self::ServerResponse)
    }
}

/// Whether a topic still carries an unexpanded component-template placeholder.
/// Such a topic must never be matched literally against another participant.
fn is_template_topic(topic: &str) -> bool {
    topic.contains("{instance}") || topic.contains("{capability}")
}

/// A stable ordering key for problems so report output and tests are
/// deterministic regardless of the order problems were appended.
fn problem_sort_key(problem: &Problem) -> (u8, String, String) {
    match problem {
        Problem::ApiVersionMismatch { artifact_id, .. } => (0, artifact_id.clone(), String::new()),
        Problem::MissingProducer { family, topic, .. } => (1, family.clone(), topic.clone()),
        Problem::MissingConsumer { family, topic, .. } => (2, family.clone(), topic.clone()),
        Problem::InvalidConfig { runtime_id, .. } => (3, runtime_id.clone(), String::new()),
        Problem::UnresolvedComponentTemplate {
            artifact_id,
            template,
            family,
            ..
        } => (4, artifact_id.clone(), format!("{template}\u{0}{family}")),
    }
}

/// The expanded graph plus any hard problems found while expanding component
/// templates (templates that bind to no concrete capability in scope).
struct MaterializedGraph {
    participants: Vec<ParticipantApis>,
    problems: Vec<Problem>,
}

fn materialize_participants(
    participants: &[ParticipantApis],
    robot_graph: &RobotGraph,
) -> MaterializedGraph {
    let mut problems = Vec::new();
    let materialized = participants
        .iter()
        .map(|participant| {
            let contracts = participant
                .contracts
                .iter()
                .flat_map(|contract| {
                    materialize_contract(participant, contract, robot_graph, &mut problems)
                })
                .collect();
            ParticipantApis {
                participant_id: participant.participant_id.clone(),
                artifact_id: participant.artifact_id.clone(),
                api_version: participant.api_version.clone(),
                bus_abi: participant.bus_abi.clone(),
                config_schema: participant.config_schema.clone(),
                scope: participant.scope.clone(),
                contracts,
            }
        })
        .collect();

    MaterializedGraph {
        participants: materialized,
        problems,
    }
}

fn materialize_contract(
    participant: &ParticipantApis,
    contract: &Contract,
    robot_graph: &RobotGraph,
    problems: &mut Vec<Problem>,
) -> Vec<Contract> {
    if !is_template_topic(&contract.topic) {
        return vec![contract.clone()];
    }

    let kind = component_topic_kind(&contract.topic);
    let mut candidates = robot_graph
        .component_capabilities
        .iter()
        .filter(|capability| kind.is_none_or(|kind| capability.kind == kind))
        .filter(|capability| match &participant.scope {
            ParticipantScope::Graph => {
                !is_motion_topic_kind(kind)
                    || robot_graph.motion_capabilities.is_empty()
                    || robot_graph
                        .motion_capabilities
                        .contains(&(capability.instance.clone(), capability.capability.clone()))
            }
            ParticipantScope::ComponentInstance(instance) => capability.instance == *instance,
        })
        .collect::<Vec<_>>();
    candidates.sort();

    let mut materialized = candidates
        .into_iter()
        .map(|capability| Contract {
            family: contract.family.clone(),
            topic: contract
                .topic
                .replace("{instance}", &capability.instance)
                .replace("{capability}", &capability.capability),
            direction: contract.direction,
        })
        .collect::<Vec<_>>();
    materialized.sort_by(|a, b| a.topic.cmp(&b.topic).then(a.direction.cmp(&b.direction)));
    materialized.dedup();

    if materialized.is_empty() {
        // A component template that expands to nothing in scope can never bind to
        // a real topic. Returning the placeholder topic would let two sides
        // satisfy each other literally, so emit a hard problem and drop the
        // contract from the graph instead.
        problems.push(Problem::UnresolvedComponentTemplate {
            artifact_id: participant.participant_id.clone(),
            template: contract.topic.clone(),
            family: contract.family.clone(),
            missing: missing_candidate_description(participant, kind),
        });
        Vec::new()
    } else {
        materialized
    }
}

/// Describe what concrete candidate the template lacked, for the diagnostic.
fn missing_candidate_description(participant: &ParticipantApis, kind: Option<&str>) -> String {
    match &participant.scope {
        ParticipantScope::ComponentInstance(instance) => match kind {
            Some(kind) => format!("component instance '{instance}' has no '{kind}' capability"),
            None => format!("component instance '{instance}' has no matching capability"),
        },
        ParticipantScope::Graph => match kind {
            Some(kind) => format!("no component instance provides a '{kind}' capability in scope"),
            None => "no component instance provides a matching capability in scope".to_string(),
        },
    }
}

fn component_topic_kind(topic: &str) -> Option<&str> {
    let mut segments = topic.split('/');
    match (segments.next(), segments.next(), segments.next()) {
        (Some("component"), Some("{instance}" | "*"), Some(kind)) if !kind.starts_with('{') => {
            Some(kind)
        }
        _ => None,
    }
}

fn is_motion_topic_kind(kind: Option<&str>) -> bool {
    matches!(kind, Some("motor" | "encoder"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(family: &str, topic: &str, direction: Direction) -> Contract {
        Contract {
            family: family.to_string(),
            topic: topic.to_string(),
            direction,
        }
    }

    fn participant(id: &str, api: &str, contracts: Vec<Contract>) -> ParticipantApis {
        ParticipantApis {
            participant_id: id.to_string(),
            artifact_id: id.to_string(),
            api_version: api.to_string(),
            bus_abi: None,
            config_schema: None,
            scope: ParticipantScope::Graph,
            contracts,
        }
    }

    fn scoped_component_participant(
        id: &str,
        instance: &str,
        contracts: Vec<Contract>,
    ) -> ParticipantApis {
        ParticipantApis {
            // A component driver shares one artifact id across instances but is a
            // distinct participant per instance — key the graph by the instance.
            participant_id: instance.to_string(),
            artifact_id: id.to_string(),
            api_version: "y2026_1".to_string(),
            bus_abi: None,
            config_schema: None,
            scope: ParticipantScope::ComponentInstance(instance.to_string()),
            contracts,
        }
    }

    fn robot_graph() -> RobotGraph {
        RobotGraph {
            component_capabilities: vec![
                ComponentCapability {
                    instance: "left_drive".to_string(),
                    capability: "motor".to_string(),
                    kind: "motor".to_string(),
                },
                ComponentCapability {
                    instance: "right_drive".to_string(),
                    capability: "motor".to_string(),
                    kind: "motor".to_string(),
                },
                ComponentCapability {
                    instance: "left_drive".to_string(),
                    capability: "encoder".to_string(),
                    kind: "encoder".to_string(),
                },
                ComponentCapability {
                    instance: "right_drive".to_string(),
                    capability: "encoder".to_string(),
                    kind: "encoder".to_string(),
                },
            ],
            motion_capabilities: BTreeSet::from([
                ("left_drive".to_string(), "motor".to_string()),
                ("right_drive".to_string(), "motor".to_string()),
                ("left_drive".to_string(), "encoder".to_string()),
                ("right_drive".to_string(), "encoder".to_string()),
            ]),
        }
    }

    #[test]
    fn direction_parse_round_trips_and_rejects_unknown() {
        assert_eq!(Direction::parse("publish"), Some(Direction::Publish));
        assert_eq!(Direction::parse("subscribe"), Some(Direction::Subscribe));
        assert_eq!(
            Direction::parse("server_response"),
            Some(Direction::ServerResponse)
        );
        assert_eq!(
            Direction::parse("query_request"),
            Some(Direction::QueryRequest)
        );
        assert_eq!(Direction::parse("nonsense"), None);
    }

    #[test]
    fn healthy_pubsub_graph_has_no_problems() {
        // producer publishes drive/target; consumer subscribes it.
        let graph = vec![
            participant(
                "mission",
                "y2026_1",
                vec![contract(
                    "drive::Target",
                    "drive/target",
                    Direction::Publish,
                )],
            ),
            participant(
                "drive",
                "y2026_1",
                vec![contract(
                    "drive::Target",
                    "drive/target",
                    Direction::Subscribe,
                )],
            ),
        ];
        assert!(check_graph(&graph, "y2026_1").is_ok());
    }

    #[test]
    fn healthy_query_graph_has_no_problems() {
        // a server serves asset/get; a client queries it.
        let graph = vec![
            participant(
                "asset",
                "y2026_1",
                vec![
                    contract("asset::GetRequest", "asset/get", Direction::ServerRequest),
                    contract("asset::GetResponse", "asset/get", Direction::ServerResponse),
                ],
            ),
            participant(
                "client",
                "y2026_1",
                vec![
                    contract("asset::GetRequest", "asset/get", Direction::QueryRequest),
                    contract("asset::GetResponse", "asset/get", Direction::QueryResponse),
                ],
            ),
        ];
        assert!(check_graph(&graph, "y2026_1").is_ok());
    }

    #[test]
    fn api_version_mismatch_is_reported_per_artifact() {
        let graph = vec![
            participant("drive", "y2026_1", vec![]),
            participant("battery", "y2026_2", vec![]),
        ];
        let report = check_graph(&graph, "y2026_1");
        assert_eq!(
            report.problems,
            vec![Problem::ApiVersionMismatch {
                artifact_id: "battery".to_string(),
                expected: "y2026_1".to_string(),
                found: "y2026_2".to_string(),
            }]
        );
    }

    #[test]
    fn subscribed_topic_without_publisher_is_a_missing_producer() {
        let graph = vec![participant(
            "drive",
            "y2026_1",
            vec![contract(
                "drive::Target",
                "drive/target",
                Direction::Subscribe,
            )],
        )];
        let report = check_graph(&graph, "y2026_1");
        assert_eq!(
            report.problems,
            vec![Problem::MissingProducer {
                family: "drive::Target".to_string(),
                topic: "drive/target".to_string(),
                consumers: vec!["drive".to_string()],
            }]
        );
    }

    #[test]
    fn publisher_without_consumer_is_a_warning() {
        let graph = vec![participant(
            "odometry",
            "y2026_1",
            vec![contract(
                "odometry::State",
                "odometry/state",
                Direction::Publish,
            )],
        )];
        let report = check_graph(&graph, "y2026_1");

        assert!(report.problems.is_empty());
        assert_eq!(
            report.warnings,
            vec![Warning::MissingConsumer {
                family: "odometry::State".to_string(),
                topic: "odometry/state".to_string(),
                producers: vec!["odometry".to_string()],
            }]
        );
    }

    #[test]
    fn query_client_without_server_is_a_missing_producer() {
        let graph = vec![participant(
            "client",
            "y2026_1",
            vec![contract(
                "asset::GetRequest",
                "asset/get",
                Direction::QueryRequest,
            )],
        )];
        let report = check_graph(&graph, "y2026_1");
        assert_eq!(report.problems.len(), 1);
        assert!(matches!(
            &report.problems[0],
            Problem::MissingProducer { family, topic, .. }
                if family == "asset::GetRequest" && topic == "asset/get"
        ));
    }

    #[test]
    fn query_server_without_client_is_a_problem() {
        let graph = vec![participant(
            "asset",
            "y2026_1",
            vec![
                contract("asset::GetRequest", "asset/get", Direction::ServerRequest),
                contract("asset::GetResponse", "asset/get", Direction::ServerResponse),
            ],
        )];
        let report = check_graph(&graph, "y2026_1");

        assert_eq!(
            report.problems,
            vec![
                Problem::MissingConsumer {
                    family: "asset::GetRequest".to_string(),
                    topic: "asset/get".to_string(),
                    producers: vec!["asset".to_string()],
                },
                Problem::MissingConsumer {
                    family: "asset::GetResponse".to_string(),
                    topic: "asset/get".to_string(),
                    producers: vec!["asset".to_string()],
                }
            ]
        );
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn component_templates_expand_to_concrete_manifest_topics() {
        let participants = vec![
            participant(
                "motion",
                "y2026_1",
                vec![contract(
                    "component::MotorCommand",
                    "component/{instance}/motor/{capability}/command",
                    Direction::Publish,
                )],
            ),
            scoped_component_participant(
                "left-driver",
                "left_drive",
                vec![contract(
                    "component::MotorCommand",
                    "component/{instance}/motor/{capability}/command",
                    Direction::Subscribe,
                )],
            ),
        ];

        let report = check_graph_with_topology(&participants, "y2026_1", &robot_graph());

        assert!(report.problems.is_empty());
        assert_eq!(
            report.warnings,
            vec![Warning::MissingConsumer {
                family: "component::MotorCommand".to_string(),
                topic: "component/right_drive/motor/motor/command".to_string(),
                producers: vec!["motion".to_string()],
            }]
        );
    }

    #[test]
    fn component_templates_report_missing_concrete_driver_output() {
        let participants = vec![
            scoped_component_participant(
                "left-driver",
                "left_drive",
                vec![contract(
                    "component::EncoderSample",
                    "component/{instance}/encoder/{capability}/sample",
                    Direction::Publish,
                )],
            ),
            participant(
                "odometry",
                "y2026_1",
                vec![contract(
                    "component::EncoderSample",
                    "component/{instance}/encoder/{capability}/sample",
                    Direction::Subscribe,
                )],
            ),
        ];

        let report = check_graph_with_topology(&participants, "y2026_1", &robot_graph());

        assert_eq!(
            report.problems,
            vec![Problem::MissingProducer {
                family: "component::EncoderSample".to_string(),
                topic: "component/right_drive/encoder/encoder/sample".to_string(),
                consumers: vec!["odometry".to_string()],
            }]
        );
    }

    #[test]
    fn component_templates_never_match_literally_with_empty_components() {
        // BLOCKER regression: with no concrete component instances in the graph,
        // two participants whose only contracts are the SAME component template
        // must NOT satisfy each other by matching the placeholder topic literally.
        // Each unexpandable template is a hard problem; neither side is bound.
        let participants = vec![
            participant(
                "motion",
                "y2026_1",
                vec![contract(
                    "component::MotorCommand",
                    "component/{instance}/motor/{capability}/command",
                    Direction::Publish,
                )],
            ),
            participant(
                "phantom-driver",
                "y2026_1",
                vec![contract(
                    "component::MotorCommand",
                    "component/{instance}/motor/{capability}/command",
                    Direction::Subscribe,
                )],
            ),
        ];

        // Empty robot graph: no component instances/capabilities exist.
        let report = check_graph_with_topology(&participants, "y2026_1", &RobotGraph::default());

        // The literal placeholder topic must never appear as a satisfied edge,
        // and must never be reported as a missing producer/consumer either.
        assert_eq!(
            report,
            Report {
                problems: vec![
                    Problem::UnresolvedComponentTemplate {
                        artifact_id: "motion".to_string(),
                        template: "component/{instance}/motor/{capability}/command".to_string(),
                        family: "component::MotorCommand".to_string(),
                        missing: "no component instance provides a 'motor' capability in scope"
                            .to_string(),
                    },
                    Problem::UnresolvedComponentTemplate {
                        artifact_id: "phantom-driver".to_string(),
                        template: "component/{instance}/motor/{capability}/command".to_string(),
                        family: "component::MotorCommand".to_string(),
                        missing: "no component instance provides a 'motor' capability in scope"
                            .to_string(),
                    },
                ],
                warnings: vec![],
            }
        );
    }

    #[test]
    fn component_driver_template_missing_capability_is_a_hard_problem() {
        // A component driver scoped to an instance that lacks the capability the
        // template needs (here: encoder, while the instance only has a motor)
        // must produce a hard `UnresolvedComponentTemplate`, not a literal match.
        let graph = RobotGraph {
            component_capabilities: vec![ComponentCapability {
                instance: "left_drive".to_string(),
                capability: "motor".to_string(),
                kind: "motor".to_string(),
            }],
            motion_capabilities: BTreeSet::new(),
        };
        let participants = vec![scoped_component_participant(
            "ddsm115",
            "left_drive",
            vec![contract(
                "component::EncoderSample",
                "component/{instance}/encoder/{capability}/sample",
                Direction::Publish,
            )],
        )];

        let report = check_graph_with_topology(&participants, "y2026_1", &graph);

        assert_eq!(
            report.problems,
            vec![Problem::UnresolvedComponentTemplate {
                // Keyed by the concrete instance id, not the shared driver artifact.
                artifact_id: "left_drive".to_string(),
                template: "component/{instance}/encoder/{capability}/sample".to_string(),
                family: "component::EncoderSample".to_string(),
                missing: "component instance 'left_drive' has no 'encoder' capability".to_string(),
            }]
        );
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn missing_producer_lists_all_consumers_sorted_and_deduped() {
        let consume = || contract("odometry::State", "odometry/state", Direction::Subscribe);
        let graph = vec![
            participant("map", "y2026_1", vec![consume()]),
            participant("localize", "y2026_1", vec![consume(), consume()]),
        ];
        let report = check_graph(&graph, "y2026_1");
        assert_eq!(
            report.problems,
            vec![Problem::MissingProducer {
                family: "odometry::State".to_string(),
                topic: "odometry/state".to_string(),
                consumers: vec!["localize".to_string(), "map".to_string()],
            }]
        );
    }

    #[test]
    fn a_publisher_anywhere_satisfies_all_subscribers() {
        let graph = vec![
            participant(
                "odometry",
                "y2026_1",
                vec![contract(
                    "odometry::State",
                    "odometry/state",
                    Direction::Publish,
                )],
            ),
            participant(
                "localize",
                "y2026_1",
                vec![contract(
                    "odometry::State",
                    "odometry/state",
                    Direction::Subscribe,
                )],
            ),
            participant(
                "map",
                "y2026_1",
                vec![contract(
                    "odometry::State",
                    "odometry/state",
                    Direction::Subscribe,
                )],
            ),
        ];
        assert!(check_graph(&graph, "y2026_1").is_ok());
    }

    #[test]
    fn a_publisher_does_not_satisfy_a_query_client() {
        // Same (family, topic) but mismatched kinds: a pub/sub publish must not be
        // accepted as the server a query client requires. (In practice the api tree
        // never declares one topic as both pub/sub and query; this guards the engine
        // regardless.)
        let graph = vec![
            participant(
                "publisher",
                "y2026_1",
                vec![contract("x::Body", "x/topic", Direction::Publish)],
            ),
            participant(
                "client",
                "y2026_1",
                vec![contract("x::Body", "x/topic", Direction::QueryRequest)],
            ),
        ];
        let report = check_graph(&graph, "y2026_1");
        assert_eq!(
            report.problems,
            vec![Problem::MissingProducer {
                family: "x::Body".to_string(),
                topic: "x/topic".to_string(),
                consumers: vec!["client".to_string()],
            }]
        );
    }

    #[test]
    fn empty_graph_is_ok() {
        assert!(check_graph(&[], "y2026_1").is_ok());
    }
}
