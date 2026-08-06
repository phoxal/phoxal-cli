//! Version identities, and the client-visible descriptor built from them.
//!
//! **A version identity is a serde enum, never a string.** The canonical text
//! exists exactly once - as the variant's serde rename - and the type system
//! carries it everywhere else. There is no `&str` constant to compare against,
//! because there is no comparison: a foreign version fails to *deserialize*,
//! and serde's own error already names the tag it found and the set it
//! expected.
//!
//! That makes compatibility checking structural rather than procedural. A
//! client cannot forget to check a field, cannot compare the wrong pair of
//! strings, and cannot accept a descriptor it only partly understands.
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

            /// The canonical wire text, for a key or a diagnostic.
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
    /// The connect exchange.
    ConnectSchema = "phoxal/supervisor-connect/v0"
}

version! {
    /// The snapshot document, shared by the stream and the current-query.
    SnapshotSchema = "phoxal/supervisor-snapshot/v0"
}

version! {
    /// The command exchange.
    CommandSchema = "phoxal/supervisor-command/v0"
}

version! {
    /// The bundle-file exchange.
    BundleGetSchema = "phoxal/supervisor-bundle-get/v0"
}

version! {
    /// The log snapshot and follow documents.
    LogsSchema = "phoxal/supervisor-logs/v0"
}

version! {
    /// The telemetry snapshot and follow documents.
    TelemetrySchema = "phoxal/supervisor-telemetry/v0"
}

version! {
    /// The bus wire ABI every typed topic rides on.
    ///
    /// Framework-owned identity, declared locally because `phoxal-bus` 0.54
    /// still exposes it only as the `BUS_ABI` string constant. The
    /// framework-owned enum replaces this on the next train; until then
    /// `local_versions_match_the_framework_constants` pins the rename to the
    /// framework's own value so the two cannot drift.
    BusAbi = "phoxal/bus/v0"
}

/// The robot API revision the execution runs.
///
/// Framework-owned identity, declared locally because `phoxal-api` 0.54 still
/// publishes it only as `ApiVersion::ID`, a `&str`; the framework-owned enum
/// replaces this on the next train, and
/// `local_versions_match_the_framework_constants` pins the rename to
/// `phoxal_api::latest::Api::ID` so the two cannot drift.
///
/// Its variant is `V0_1` rather than `V0` because a robot API revision is
/// dotted (`v0.1`), which is a different axis from the `<name>/v<n>` document
/// schemas above - hence its own declaration rather than the `version!` macro.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum RobotApi {
    #[default]
    #[serde(rename = "v0.1")]
    V0_1,
}

impl RobotApi {
    /// The one revision this build speaks.
    pub const CURRENT: Self = Self::V0_1;

    /// The canonical wire text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        "v0.1"
    }
}

impl std::fmt::Display for RobotApi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Every version a remote client consumes, advertised in the connect reply.
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
    pub bus: BusAbi,
    /// The robot API revision the execution runs.
    pub robot: RobotApi,
    pub snapshot: SnapshotSchema,
    pub command: CommandSchema,
    pub bundle_get: BundleGetSchema,
    pub logs: LogsSchema,
    pub telemetry: TelemetrySchema,
}

impl SupervisorSchemas {
    /// The versions this build speaks. Every field has one variant, so this is
    /// the only value that exists.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            bus: BusAbi::CURRENT,
            robot: RobotApi::CURRENT,
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
            ConnectSchema::CURRENT,
            SnapshotSchema::CURRENT,
            CommandSchema::CURRENT,
            BundleGetSchema::CURRENT,
            LogsSchema::CURRENT,
            TelemetrySchema::CURRENT,
            BusAbi::CURRENT,
            RobotApi::CURRENT,
        );
    }

    /// The two framework-owned identities are declared locally only because
    /// `phoxal-bus` / `phoxal-api` still publish them as `&str`. This pins each
    /// local rename to the framework's own value, so the day the framework
    /// ships the enums this crate cannot already have drifted from them.
    #[test]
    fn local_versions_match_the_framework_constants() {
        assert_eq!(BusAbi::CURRENT.as_str(), BUS_ABI);
        assert_eq!(
            RobotApi::CURRENT.as_str(),
            <phoxal_api::latest::Api as ApiVersion>::ID
        );
    }

    /// A supervisor on another train is rejected by the decoder, and the error
    /// names both what it found and what was expected - which is the whole
    /// diagnostic, produced without a single string comparison in this crate.
    #[test]
    fn a_foreign_version_fails_to_decode_and_serde_names_it() {
        let mut descriptor = serde_json::to_value(SupervisorSchemas::current()).unwrap();
        descriptor["snapshot"] = serde_json::json!("phoxal/supervisor-snapshot/v1");

        let error = serde_json::from_value::<SupervisorSchemas>(descriptor)
            .expect_err("a foreign snapshot version must not decode");
        let rendered = error.to_string();
        assert!(
            rendered.contains("phoxal/supervisor-snapshot/v1"),
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
