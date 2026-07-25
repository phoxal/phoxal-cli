//! Minimal fixture binary for `phoxal-cli`'s
//! `participant_metadata::extract_participant_metadata` end-to-end test
//! (`src/participant_metadata.rs`).
//!
//! The participant attribute embeds the derived API contracts and config
//! schema in a linker section at compile time. No behavior implementation is
//! needed: this fixture exists only as a real binary carrying that section.
//!
//! The field is written with the fully-qualified
//! `phoxal_api::v0_1::drive::Target` path rather than through a
//! `use phoxal_api::v0_1 as api;` alias: the
//! macro records the type exactly as written in source, verbatim, without
//! resolving `use` aliases - so this is what makes the embedded metadata's
//! `contract` string come out version-qualified
//! (`"phoxal_api::v0_1::drive::Target"`)
//! instead of `"api::drive::Target"`.

use phoxal::prelude::*;

#[derive(serde::Deserialize, phoxal::Config)]
#[allow(dead_code)]
struct Config {
    gain: f64,
}

#[derive(phoxal::Api)]
#[allow(dead_code)]
struct Api {
    target: CommandPublisher<phoxal_api::v0_1::drive::Target>,
}

#[phoxal::service(id = "avoid")]
#[allow(dead_code)]
struct Avoid;

fn main() {}
