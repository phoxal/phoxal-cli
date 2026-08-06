//! Version identities, and the client-visible descriptor built from them.
//!
//! **A version identity is a serde enum, never a string.** The canonical text
//! exists exactly once - as the variant's serde rename - and the type system
//! carries it everywhere else. There is no `&str` constant to compare against,
//! because there is no comparison: a foreign version fails to *deserialize*,
//! and serde's own error already names the tag it found and the set it
//! expected.
//!
//! # Tags mirror routing
//!
//! Every document tag is `phoxal/<relative-topic-path>[/<role>]/v0`, so a tag
//! read out of a capture says exactly which endpoint produced it. Tags are
//! **unique per structure** - a request and its reply are different documents
//! and never share one - which is what makes a tag identifying rather than
//! merely descriptive. `tests::no_two_documents_share_a_tag` enforces that
//! globally, and `tests::every_documents_tag_mirrors_its_topic_path` derives
//! each expected tag from the topic's own relative path rather than restating
//! it.
//!
//! Tags are documents, not keys. Bus key segments are unchanged: the robot API
//! still routes on `v0.1/...` and this protocol on `supervisor/...`.
//!
//! Pre-v1 the single current variant is edited in place and every binary is
//! rebuilt.

use serde::{Deserialize, Serialize};

/// Declare one version identity from one literal.
///
/// The wire string appears exactly once, here, and `as_str` is generated from
/// the same token as the serde rename - so the two cannot drift, and the
/// `as_str_matches_the_serde_rename` test proves it for every declared version.
macro_rules! version {
    ($(#[$doc:meta])* $name:ident = $wire:literal) => {
        $(#[$doc])*
        #[derive(
            Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd,
            Serialize,
        )]
        pub enum $name {
            #[default]
            #[serde(rename = $wire)]
            V0,
        }

        impl $name {
            /// The one version this build speaks.
            pub const CURRENT: Self = Self::V0;

            /// The canonical wire text, for a diagnostic.
            ///
            /// Generated from the same literal as the serde rename; never a
            /// second constant.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                $wire
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

version! {
    /// The snapshot topic, named by its **stream** document.
    ///
    /// A topic that pushes state is identified by what it pushes: the stream is
    /// the primary document and the `current` query exists to bootstrap it.
    SnapshotSchema = "phoxal/supervisor/snapshot/v0"
}

version! {
    /// The command topic, named by its **reply** document.
    ///
    /// A query-only topic is identified by its reply: the reply is the document
    /// a client must understand to use the endpoint at all, and it is the one
    /// the supervisor authors.
    CommandSchema = "phoxal/supervisor/command/reply/v0"
}

version! {
    /// The bundle-file topic, named by its **reply** document - a query-only
    /// topic, so the reply is primary for the same reason as
    /// [`CommandSchema`].
    BundleGetSchema = "phoxal/supervisor/bundle/get/reply/v0"
}

version! {
    /// The log surface, named by its **snapshot reply** document.
    ///
    /// Logs have both a query and a follow stream. The snapshot reply is
    /// primary because it is what a client must decode first: a follow record
    /// is meaningless without the cursor the snapshot installs.
    LogsSchema = "phoxal/supervisor/logs/snapshot/v0"
}

version! {
    /// The telemetry surface, named by its **snapshot reply** document, for the
    /// same reason as [`LogsSchema`].
    TelemetrySchema = "phoxal/supervisor/telemetry/snapshot/v0"
}

version! {
    /// The bus wire ABI every typed topic rides on.
    ///
    /// Framework-owned identity, declared locally because `phoxal-bus` 0.54
    /// exposes it only as the `BUS_ABI` string constant, and pinned to that
    /// constant by `local_versions_match_the_framework_constants` so the two
    /// cannot drift. The framework owns the rename: it moves to
    /// `phoxal/bus-abi/v0` on the next train, at which point this local bridge
    /// is deleted rather than renamed here.
    BusAbi = "phoxal/bus/v0"
}

version! {
    /// The authored robot document grammar.
    ///
    /// This is the manifest grammar a client parses after fetching `robot.yaml`
    /// through `bundle/get` - a different axis from [`RobotApi`], which names
    /// the contract tree the robot-domain keys carry. A client needs both: one
    /// to read the manifest, one to speak to the graph.
    ///
    /// Framework-owned identity, declared locally for the same reason as
    /// [`BusAbi`] and pinned to `ROBOT_DOCUMENT_SCHEMA` by the same test.
    RobotDocumentSchema = "robot/v0"
}

/// The robot API revision the execution runs.
///
/// Namespaced like every other version identity, so the tag is self-describing
/// wherever it is read. **The key segment is unaffected**: robot-domain keys
/// still route on the bare revision (`phoxal/{execution}/v0.1/drive/state`),
/// which `phoxal-api` owns and this crate does not touch.
///
/// Framework-owned identity, declared locally because `phoxal-api` 0.54
/// publishes the revision only as `ApiVersion::ID`, a `&str`. Its variant is
/// `V0_1` rather than `V0` because a revision is dotted, which is why it is
/// declared here rather than through the `version!` macro.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum RobotApi {
    #[default]
    #[serde(rename = "phoxal/robot-api/v0.1")]
    V0_1,
}

impl RobotApi {
    /// The one revision this build speaks.
    pub const CURRENT: Self = Self::V0_1;

    /// The namespace every robot API revision tag carries. The revision itself
    /// belongs to `phoxal-api`, so it is never spelled out in this crate: the
    /// pin test composes this prefix with `ApiVersion::ID` and asserts the
    /// result is exactly the serde rename.
    pub const NAMESPACE: &'static str = "phoxal/robot-api/";

    /// The canonical wire text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        "phoxal/robot-api/v0.1"
    }
}

impl std::fmt::Display for RobotApi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Every version a remote client consumes, advertised in the connect reply.
///
/// Each field names its topic's **primary** document - the stream for a topic
/// that pushes state, the reply for a query-only topic - so the descriptor is a
/// list of endpoints rather than a list of structures. The per-structure tags
/// (request, reply, follow) are finer-grained than this and are validated by
/// the decoder that reads each document, not by this descriptor.
///
/// Deliberately client-visible only: the participant launch ABI and the
/// component/simulation document grammars are daemon and build validation
/// inputs, so they are not attachment negotiation fields.
///
/// Because every field is a version enum, **decoding this descriptor is the
/// compatibility check**. A supervisor on a different train produces a
/// descriptor this client cannot deserialize, and serde names the offending
/// version in the error. There is no range, no negotiation, and nothing for a
/// caller to remember to compare.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(deny_unknown_fields)]
pub struct SupervisorSchemas {
    /// The bus wire ABI every topic below rides on.
    pub bus: BusAbi,
    /// The authored robot document grammar, for the `robot.yaml` a client
    /// fetches through `bundle/get`. Distinct from the connect reply's `api`,
    /// which is the robot API revision.
    pub robot: RobotDocumentSchema,
    /// Primary: the pushed snapshot stream.
    pub snapshot: SnapshotSchema,
    /// Primary: the command reply.
    pub command: CommandSchema,
    /// Primary: the bundle-file reply.
    pub bundle_get: BundleGetSchema,
    /// Primary: the log snapshot reply.
    pub logs: LogsSchema,
    /// Primary: the telemetry snapshot reply.
    pub telemetry: TelemetrySchema,
}

impl SupervisorSchemas {
    /// The versions this build speaks. Every field has one variant, so this is
    /// the only value that exists.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            bus: BusAbi::CURRENT,
            robot: RobotDocumentSchema::CURRENT,
            snapshot: SnapshotSchema::CURRENT,
            command: CommandSchema::CURRENT,
            bundle_get: BundleGetSchema::CURRENT,
            logs: LogsSchema::CURRENT,
            telemetry: TelemetrySchema::CURRENT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_bus::{ApiVersion, BUS_ABI};
    use phoxal_runtime_contract::ROBOT_DOCUMENT_SCHEMA;

    /// `as_str` and the serde rename come from one literal per version; this
    /// proves the generated pair really does agree for every one of them.
    #[test]
    fn as_str_matches_the_serde_rename() {
        macro_rules! pin {
            ($($version:expr),* $(,)?) => {$(
                assert_eq!(
                    serde_json::to_value($version).unwrap(),
                    serde_json::Value::String($version.as_str().to_string()),
                );
            )*};
        }
        pin!(
            SnapshotSchema::CURRENT,
            CommandSchema::CURRENT,
            BundleGetSchema::CURRENT,
            LogsSchema::CURRENT,
            TelemetrySchema::CURRENT,
            BusAbi::CURRENT,
            RobotDocumentSchema::CURRENT,
            RobotApi::CURRENT,
        );
    }

    /// The three framework-owned identities are declared locally only because
    /// `phoxal-bus` / `phoxal-api` still publish them as `&str`. This pins each
    /// local rename to the framework's own value, so the day the framework
    /// ships the enums this crate cannot already have drifted from them.
    ///
    /// `RobotApi` is composed rather than compared: the revision itself is
    /// never spelled out here, only the namespace it hangs under.
    #[test]
    fn local_versions_match_the_framework_constants() {
        assert_eq!(BusAbi::CURRENT.as_str(), BUS_ABI);
        assert_eq!(RobotDocumentSchema::CURRENT.as_str(), ROBOT_DOCUMENT_SCHEMA);
        assert_eq!(
            RobotApi::CURRENT.as_str(),
            format!(
                "{}{}",
                RobotApi::NAMESPACE,
                <phoxal_api::latest::Api as ApiVersion>::ID
            )
        );
    }

    /// Each descriptor field names its topic's primary document, so its rename
    /// must be a tag some structure in this crate actually carries. Derived
    /// from the topic paths, so a retagged document fails here too.
    #[test]
    fn every_descriptor_field_names_a_real_document_tag() {
        let tags = crate::tests::document_tags();
        for (field, version) in [
            ("snapshot", SnapshotSchema::CURRENT.as_str()),
            ("command", CommandSchema::CURRENT.as_str()),
            ("bundle_get", BundleGetSchema::CURRENT.as_str()),
            ("logs", LogsSchema::CURRENT.as_str()),
            ("telemetry", TelemetrySchema::CURRENT.as_str()),
        ] {
            assert!(
                tags.iter().any(|(_, tag)| tag == version),
                "descriptor field `{field}` names `{version}`, which no document carries"
            );
        }
    }

    /// A supervisor on another train is rejected by the decoder, and the error
    /// names both what it found and what was expected - which is the whole
    /// diagnostic, produced without a single string comparison in this crate.
    #[test]
    fn a_foreign_version_fails_to_decode_and_serde_names_it() {
        let mut descriptor = serde_json::to_value(SupervisorSchemas::current()).unwrap();
        descriptor["snapshot"] = serde_json::json!("phoxal/supervisor/snapshot/v1");

        let error = serde_json::from_value::<SupervisorSchemas>(descriptor)
            .expect_err("a foreign snapshot version must not decode");
        let rendered = error.to_string();
        assert!(
            rendered.contains("phoxal/supervisor/snapshot/v1"),
            "{rendered}"
        );
        assert!(
            rendered.contains(SnapshotSchema::CURRENT.as_str()),
            "{rendered}"
        );
        assert!(rendered.contains("snapshot"), "{rendered}");
    }

    #[test]
    fn the_descriptor_round_trips_and_rejects_an_unknown_field() {
        let current = SupervisorSchemas::current();
        let json = serde_json::to_value(current).unwrap();
        assert_eq!(json["bus"], BUS_ABI);
        assert_eq!(
            serde_json::from_value::<SupervisorSchemas>(json.clone()).unwrap(),
            current
        );

        let mut extra = json;
        extra["catalog"] = serde_json::json!("phoxal/catalog/v0");
        assert!(serde_json::from_value::<SupervisorSchemas>(extra).is_err());
    }
}
