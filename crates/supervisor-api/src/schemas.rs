//! The client-visible schema descriptor and this build's authoritative values.
//!
//! Compatibility here is contract-based, never framework-SemVer-based
//! (organization#978): a client validates the API revision plus every
//! client-visible schema it intends to consume, and nothing else. This is
//! current-schema introspection for precise diagnostics, not range
//! negotiation - there is exactly one current value per surface, and pre-v1 it
//! is edited in place.

use phoxal_bus::{ApiVersion, BUS_ABI};
use phoxal_runtime_contract::{ApiId, ROBOT_DOCUMENT_SCHEMA, SchemaId};
use serde::{Deserialize, Serialize};

/// The connect request/reply document schema.
pub const CONNECT_SCHEMA: &str = "phoxal/supervisor-connect/v0";
/// The snapshot document schema, shared by the stream and the current-query.
pub const SNAPSHOT_SCHEMA: &str = "phoxal/supervisor-snapshot/v0";
/// The command request/reply document schema.
pub const COMMAND_SCHEMA: &str = "phoxal/supervisor-command/v0";
/// The bundle-file request/reply document schema.
pub const BUNDLE_GET_SCHEMA: &str = "phoxal/supervisor-bundle-get/v0";
/// The log snapshot/follow document schema.
pub const LOGS_SCHEMA: &str = "phoxal/supervisor-logs/v0";
/// The telemetry snapshot/follow document schema.
pub const TELEMETRY_SCHEMA: &str = "phoxal/supervisor-telemetry/v0";

/// Every schema a remote client consumes, advertised in the connect reply.
///
/// Deliberately client-visible only: the participant launch ABI and the
/// component/simulation document grammars are daemon and build validation
/// inputs, so they are not attachment negotiation fields.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorSchemas {
    /// The bus wire ABI every typed topic below rides on.
    #[serde(with = "schema_id")]
    pub bus: SchemaId,
    /// The finalized robot document a client fetches through `bundle/get`.
    #[serde(with = "schema_id")]
    pub robot: SchemaId,
    #[serde(with = "schema_id")]
    pub snapshot: SchemaId,
    #[serde(with = "schema_id")]
    pub command: SchemaId,
    #[serde(with = "schema_id")]
    pub bundle_get: SchemaId,
    #[serde(with = "schema_id")]
    pub logs: SchemaId,
    #[serde(with = "schema_id")]
    pub telemetry: SchemaId,
}

impl SupervisorSchemas {
    /// The values this build speaks.
    #[must_use]
    pub fn current() -> Self {
        Self {
            bus: SchemaId::new(BUS_ABI),
            robot: SchemaId::new(ROBOT_DOCUMENT_SCHEMA),
            snapshot: SchemaId::new(SNAPSHOT_SCHEMA),
            command: SchemaId::new(COMMAND_SCHEMA),
            bundle_get: SchemaId::new(BUNDLE_GET_SCHEMA),
            logs: SchemaId::new(LOGS_SCHEMA),
            telemetry: SchemaId::new(TELEMETRY_SCHEMA),
        }
    }

    /// This descriptor's value for one named surface.
    #[must_use]
    pub fn get(&self, surface: SchemaSurface) -> &SchemaId {
        match surface {
            SchemaSurface::Bus => &self.bus,
            SchemaSurface::Robot => &self.robot,
            SchemaSurface::Snapshot => &self.snapshot,
            SchemaSurface::Command => &self.command,
            SchemaSurface::BundleGet => &self.bundle_get,
            SchemaSurface::Logs => &self.logs,
            SchemaSurface::Telemetry => &self.telemetry,
        }
    }
}

/// One field of [`SupervisorSchemas`], named so a mismatch diagnostic can say
/// which surface disagreed instead of dumping the whole descriptor.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SchemaSurface {
    Bus,
    Robot,
    Snapshot,
    Command,
    BundleGet,
    Logs,
    Telemetry,
}

impl SchemaSurface {
    /// Every surface, in diagnostic order.
    pub const ALL: &'static [Self] = &[
        Self::Bus,
        Self::Robot,
        Self::Snapshot,
        Self::Command,
        Self::BundleGet,
        Self::Logs,
        Self::Telemetry,
    ];

    /// The connect-reply field name, as an operator reads it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bus => "bus",
            Self::Robot => "robot",
            Self::Snapshot => "snapshot",
            Self::Command => "command",
            Self::BundleGet => "bundle_get",
            Self::Logs => "logs",
            Self::Telemetry => "telemetry",
        }
    }
}

impl std::fmt::Display for SchemaSurface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// The robot API revision this build speaks, read from the API tree itself
/// rather than repeated as a literal.
#[must_use]
pub fn current_api() -> ApiId {
    ApiId::new(<phoxal_api::latest::Api as ApiVersion>::ID)
}

/// `SchemaId` crosses this contract in both directions, but the framework type
/// is `Deserialize`-only (it was introduced for the embedded participant
/// metadata record, which has exactly one writer). Until it gains `Serialize`,
/// the descriptor writes it through its rendered form - which is the same
/// string the framework would emit - so no second identifier type is invented
/// here. See NEEDED-FROM-FRAMEWORK in this crate's docs.
pub(crate) mod schema_id {
    use phoxal_runtime_contract::SchemaId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        value: &SchemaId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.as_str())
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SchemaId, D::Error> {
        SchemaId::deserialize(deserializer)
    }
}

/// [`ApiId`]'s counterpart to [`schema_id`], for the same reason.
pub(crate) mod api_id {
    use phoxal_runtime_contract::ApiId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        value: &ApiId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.as_str())
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ApiId, D::Error> {
        ApiId::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_takes_shared_identifiers_from_their_owning_crates() {
        let current = SupervisorSchemas::current();
        assert_eq!(current.bus, *BUS_ABI);
        assert_eq!(current.robot, *ROBOT_DOCUMENT_SCHEMA);
        assert_eq!(current_api(), *"v0.1");
    }

    #[test]
    fn the_descriptor_round_trips_through_its_rendered_identifiers() {
        let current = SupervisorSchemas::current();
        let json = serde_json::to_value(&current).unwrap();
        assert_eq!(json["bus"], BUS_ABI);
        assert_eq!(json["bundle_get"], BUNDLE_GET_SCHEMA);
        assert_eq!(
            serde_json::from_value::<SupervisorSchemas>(json).unwrap(),
            current
        );
    }

    #[test]
    fn every_surface_is_reachable_by_name() {
        let current = SupervisorSchemas::current();
        for surface in SchemaSurface::ALL {
            assert!(!current.get(*surface).as_str().is_empty(), "{surface}");
        }
        assert_eq!(SchemaSurface::ALL.len(), 7);
    }
}
