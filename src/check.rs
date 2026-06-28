//! Graph validation for `phoxal check` (D59/D63).
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
    /// The artifact id (`emit-apis` `artifact.id`), e.g. `"drive"`.
    pub artifact_id: String,
    /// The API version the artifact reports (`emit-apis` `api_version`).
    pub api_version: String,
    /// The contracts the artifact participates in.
    pub contracts: Vec<Contract>,
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
}

/// The outcome of validating a graph: the problems found (empty == healthy).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub problems: Vec<Problem>,
}

impl Report {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Validate a robot graph: single-API-version (D59/D63) + topology cardinality.
///
/// `participants` is every normal participant's `emit-apis` report;
/// `root_api_version` is the manifest's root `api_version`. Problems are returned in
/// a stable order (api-version mismatches first, by artifact; then missing producers
/// by family+topic) so output and tests are deterministic.
#[must_use]
pub fn check_graph(participants: &[ParticipantApis], root_api_version: &str) -> Report {
    let mut problems = Vec::new();

    // 1. Single API version — report each disagreeing artifact, in artifact order.
    let mut mismatches: Vec<&ParticipantApis> = participants
        .iter()
        .filter(|p| p.api_version != root_api_version)
        .collect();
    mismatches.sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));
    for p in mismatches {
        problems.push(Problem::ApiVersionMismatch {
            artifact_id: p.artifact_id.clone(),
            expected: root_api_version.to_string(),
            found: p.api_version.clone(),
        });
    }

    // 2. Topology cardinality. Producers are keyed by (family, topic, direction) so
    // matching is by *kind*: a `subscribe` needs a `publish`, a query client needs a
    // server — a publisher cannot satisfy a query request. A contract is keyed by
    // (family, topic) — for dynamic per-instance topics this is the shared template,
    // the right graph-level granularity ("does some participant produce this contract").
    let mut producers: BTreeSet<(String, String, Direction)> = BTreeSet::new();
    for p in participants {
        for c in &p.contracts {
            if c.direction.is_producer() {
                producers.insert((c.family.clone(), c.topic.clone(), c.direction));
            }
        }
    }

    // Collect consumers per unmet contract, de-duplicated and ordered.
    let mut unmet: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for p in participants {
        for c in &p.contracts {
            if let Some(required) = c.direction.required_producer() {
                let needed = (c.family.clone(), c.topic.clone(), required);
                if !producers.contains(&needed) {
                    unmet
                        .entry((c.family.clone(), c.topic.clone()))
                        .or_default()
                        .insert(p.artifact_id.clone());
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

    Report { problems }
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
            artifact_id: id.to_string(),
            api_version: api.to_string(),
            contracts,
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
