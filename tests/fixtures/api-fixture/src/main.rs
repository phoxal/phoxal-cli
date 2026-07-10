//! Minimal fixture binary for `phoxal-cli`'s
//! `participant_metadata::extract_participant_metadata` end-to-end test
//! (`src/participant_metadata.rs`).
//!
//! `#[derive(phoxal::Api)]` embeds this struct's contract manifest in a
//! linker section at compile time (`phoxal-macros::authoring`); the derive
//! only needs the `Api` struct's field types to be syntactically recognized
//! handle types (`Publisher<T>`, `Subscriber<T>`, ...) - it never constructs
//! an instance, so this fixture needs no `#[phoxal::service]`/behavior
//! machinery, just a real compiled binary carrying a real embedded section to
//! extract from.
//!
//! The field is written with the fully-qualified `y2026_1::drive::Target`
//! path rather than through a `use phoxal_api::y2026_1 as api;` alias: the
//! macro records the type exactly as written in source, verbatim, without
//! resolving `use` aliases - so this is what makes the embedded metadata's
//! `contract` string come out generation-qualified (`"y2026_1::drive::Target"`)
//! instead of `"api::drive::Target"`.

use phoxal::prelude::*;

#[derive(phoxal::Api)]
#[allow(dead_code)]
struct Api {
    target: Publisher<phoxal_api::y2026_1::drive::Target>,
}

fn main() {}
