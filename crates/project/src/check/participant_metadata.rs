//! Project participant metadata extraction.
//!
//! A `#[phoxal::brain]`/`service`/`driver`/`simulator` attribute embeds one JSON
//! manifest per participant binary in a dedicated linker section -
//! `__DATA,__phoxal_meta` on Mach-O,
//! `.phoxal_meta` everywhere else (`phoxal-macros/src/authoring.rs`'s
//! `link_section_attrs`). The CLI reads the section's bytes straight out of
//! the object file without ever executing the artifact. This module targets
//! the same linker-section shape `phoxal-macros` embeds and is format- and
//! architecture-agnostic through the `object` crate.
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use object::{Object, ObjectSection};
#[cfg(test)]
use phoxal_runtime_contract::metadata::ParticipantSchemas;
use phoxal_runtime_contract::metadata::{ParticipantContract, ParticipantMetadata};
#[cfg(test)]
use phoxal_runtime_contract::version::{BusAbi, LaunchAbi, RuntimeSchema};

/// The version set this CLI build speaks.
///
/// Every field is a one-variant enum, so this is the *only* value of its type
/// that exists on this train - which is exactly why nothing compares against
/// it: a binary that disagrees fails to parse. It is named here so a test that
/// synthesizes a participant binary can emit the same document a role macro
/// does, without restating a single version string.
#[cfg(test)]
pub const CURRENT_SCHEMAS: ParticipantSchemas = ParticipantSchemas {
    bus: BusAbi::V0,
    launch: LaunchAbi::V0,
    runtime: RuntimeSchema::V0,
};

/// One binary's accepted embedded compatibility record: the tagged `V0`
/// document destructured into the fields callers actually branch on.
///
/// Every version identity in it is a `phoxal-runtime-contract` enum with one
/// variant per version this train speaks, so **holding this value is already
/// the compatibility proof**. There is no set to compare it against and no
/// check a caller could forget: a binary from another train carries a token
/// none of those enums has a variant for, and it fails at
/// [`phoxal_runtime_contract::parse_participant_metadata`] - where serde names
/// both the token it found and the one it expected.
pub type ParticipantMeta = ParticipantContract;

/// The linker section names a participant attribute places its metadata
/// static under, tried in order. `object`'s generic [`Object::section_by_name`]
/// matches on the section name alone (Mach-O segment qualification is not
/// part of the match), so no per-format branching is needed here - the two
/// candidate names are simply disjoint across the object formats this
/// framework ships binaries for.
pub const SECTION_NAMES: [&str; 2] = [".phoxal_meta", "__phoxal_meta"];

/// Parses `object_bytes` as an object file and returns the bytes of its
/// participant metadata section, trying each candidate section
/// name in [`SECTION_NAMES`] in turn. `Ok(None)` means the object file parsed
/// fine but carries no such section at all. Every binary this module is asked
/// to inspect is expected to be a compiled `#[phoxal::brain]`/`service`/
/// `driver`/`simulator` participant, so a missing section is NOT a valid
/// "no participant attribute" shape here - see
/// [`extract_participant_metadata_from_bytes`], which turns `None` into a
/// hard error rather than a synthesized identity. A malformed/unrecognized
/// *object file* is still a hard error. `describe` names the source (a file
/// path) for error messages.
fn read_meta_section(object_bytes: &[u8], describe: &str) -> Result<Option<Vec<u8>>> {
    let file = object::File::parse(object_bytes)
        .with_context(|| format!("{describe} is not a recognized object file (ELF/Mach-O/...)"))?;

    for name in SECTION_NAMES {
        if let Some(section) = file.section_by_name(name) {
            let bytes = section
                .data()
                .with_context(|| format!("failed to read section '{name}' data from {describe}"))?;
            return Ok(Some(bytes.to_vec()));
        }
    }

    Ok(None)
}

/// Parses the embedded participant metadata out of an in-memory object file
/// (an ELF/Mach-O binary of any target architecture). Reads nothing, runs
/// nothing.
///
/// A binary with no metadata section at all is a hard error, not a
/// synthesized identity: every caller of this function inspects a binary it
/// expects to be a compiled phoxal participant (the root brain, a service, a
/// driver, or a simulator), and that participant's own declared `id` is what an identity
/// check compares against an expected artifact/participant identity
/// afterward. Silently returning a placeholder `id: "()"` here used to let a
/// binary with no section at all sail through that check, because the
/// placeholder was never compared against anything real.
pub fn extract_participant_metadata_from_bytes(
    object_bytes: &[u8],
    describe: &str,
) -> Result<ParticipantMeta> {
    let bytes = read_meta_section(object_bytes, describe)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{describe} carries no phoxal participant metadata section ({}); it is not a \
             compiled #[phoxal::brain]/#[phoxal::service]/#[phoxal::driver]/#[phoxal::simulator] \
             participant binary, or it is stale and needs rebuilding",
            SECTION_NAMES.join(" or ")
        )
    })?;
    // The parse IS the compatibility check. Every version identity in the
    // document is a one-variant enum, so a binary from another train fails
    // here, naming the token it carries and the token this CLI expects. The
    // context below adds only the two things serde cannot know: which file it
    // was, and what an operator does about it.
    let metadata = ParticipantMetadata::from_bytes(&bytes).with_context(|| {
        format!(
            "{describe} was built against a different phoxal train than this CLI.\n\nIf the \
                 binary is older, update the project dependency and rebuild:\n    cargo update -p \
                 phoxal\n\nIf it is newer, update the phoxal CLI to the release that ships this \
                 contract."
        )
    })?;
    let contract = metadata.contract();
    let expected_api = phoxal_runtime_contract::version::RobotApiVersion::new(0, 1);
    anyhow::ensure!(
        contract.api == expected_api,
        "{describe} was built against robot API {}, but this CLI executes {}. Update the project dependency and rebuild with `cargo update -p phoxal`, or update the CLI to the matching release.",
        contract.api,
        expected_api,
    );
    Ok(contract.clone())
}

/// Extracts and parses `binary_path`'s embedded participant metadata: reads
/// the compiled-in linker section straight off the object file, never
/// executing it.
pub fn extract_participant_metadata(binary_path: &Path) -> Result<ParticipantMeta> {
    let data = fs::read(binary_path)
        .with_context(|| format!("failed to read {}", binary_path.display()))?;
    extract_participant_metadata_from_bytes(&data, &binary_path.display().to_string())
}

/// The [`object::Architecture`] this CLI process runs on, mapped from
/// [`std::env::consts::ARCH`]. Unknown/exotic host arches map to
/// [`object::Architecture::Unknown`].
#[must_use]
pub fn host_architecture() -> object::Architecture {
    architecture_for_token(std::env::consts::ARCH)
}

/// The object container format binaries for THIS host use: Mach-O on Apple
/// hosts and ELF on Linux. Host layout validation checks it
/// explicitly (): deferring format to the OS exec would let a
/// same-CPU foreign-OS bundle (an aarch64 Linux bundle on an Apple Silicon
/// host) validate and then crash at spawn with an exec-format error.
#[must_use]
pub fn host_binary_format() -> object::BinaryFormat {
    if cfg!(target_os = "macos") {
        object::BinaryFormat::MachO
    } else {
        object::BinaryFormat::Elf
    }
}

/// Map an architecture token to its [`object::Architecture`]. The token is a
/// `std::env::consts::ARCH` value or the leading component of a Rust target
/// triple (`aarch64-unknown-linux-gnu` yields `aarch64`). Unknown/exotic tokens
/// map to [`object::Architecture::Unknown`].
fn architecture_for_token(token: &str) -> object::Architecture {
    match token {
        "x86_64" => object::Architecture::X86_64,
        "aarch64" => object::Architecture::Aarch64,
        _ => object::Architecture::Unknown,
    }
}

/// The endianness a target-triple arch token compiles for. Every arch phoxal
/// targets is little-endian except the classic big-endian families, which are
/// listed explicitly so an `*el`/`le` little-endian variant is not misread as
/// big. Returns `None` for a token whose endianness this mapping cannot
/// authoritatively decide, which forces [`expected_target_for_triple`] to reject
/// the triple rather than guess.
fn endianness_for_token(token: &str) -> Option<object::Endianness> {
    match token {
        "x86_64" | "aarch64" => Some(object::Endianness::Little),
        _ => None,
    }
}

/// The object [`object::BinaryFormat`] the OS component of a Rust target triple
/// produces: Linux triples are ELF and Apple triples Mach-O.
/// Returns `None` for an OS this mapping cannot decide, forcing
/// [`expected_target_for_triple`] to reject the triple.
fn format_for_triple(triple: &str) -> Option<object::BinaryFormat> {
    match triple {
        "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu" => Some(object::BinaryFormat::Elf),
        "aarch64-apple-darwin" => Some(object::BinaryFormat::MachO),
        _ => None,
    }
}

/// The authoritative object-file signature a binary for a declared target must
/// have: its container format (ELF/Mach-O/PE, from the triple's OS), its CPU
/// architecture, and its endianness. Validating all three rejects a same-CPU
/// wrong-OS binary (a Mach-O x86_64 offered for `x86_64-unknown-linux-gnu`) and
/// a wrong-endian binary, which a CPU-only check would wave through.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedTarget {
    pub format: object::BinaryFormat,
    pub architecture: object::Architecture,
    pub endianness: object::Endianness,
}

/// The [`ExpectedTarget`] a Rust target triple compiles for. `phoxal build
/// --target <TRIPLE>` inspects a cross-compiled bundle against this rather than
/// the host's, so a foreign-but-correct bundle validates while a wrong-format,
/// wrong-arch, or wrong-endian binary for the declared target still fails. A
/// triple this validator cannot authoritatively map (unknown OS, arch, or
/// endianness) is REJECTED with a precise "cannot validate target" error rather
/// than silently passing every binary.
pub fn expected_target_for_triple(triple: &str) -> Result<ExpectedTarget> {
    let arch_token = triple.split('-').next().unwrap_or("");
    let format = format_for_triple(triple);
    let architecture = architecture_for_token(arch_token);
    let endianness = endianness_for_token(arch_token);
    match (format, architecture, endianness) {
        (Some(format), architecture, Some(endianness))
            if architecture != object::Architecture::Unknown =>
        {
            Ok(ExpectedTarget {
                format,
                architecture,
                endianness,
            })
        }
        _ => anyhow::bail!(
            "cannot validate target `{triple}`: the CLI cannot authoritatively map its object-file \
             format, CPU architecture, and endianness, so it refuses to inspect a bundle for it \
             rather than pass a possibly incompatible binary. Build for a supported triple (e.g. \
             aarch64-unknown-linux-gnu, x86_64-unknown-linux-gnu, or aarch64-apple-darwin)."
        ),
    }
}

/// The [`ExpectedTarget`] for the host this CLI process runs on, from
/// `std::env::consts` and the host's native endianness. Used by an in-place
/// `run`/`start`: a staged/extracted layout only ever runs on the host it was
/// prepared for, so its selected binaries must match the host signature -
/// format included, so a same-CPU foreign-OS bundle fails at inspection with a
/// precise diagnostic instead of an exec-format crash ().
#[must_use]
pub fn expected_target_for_host() -> ExpectedTarget {
    let endianness = if cfg!(target_endian = "big") {
        object::Endianness::Big
    } else {
        object::Endianness::Little
    };
    ExpectedTarget {
        format: host_binary_format(),
        architecture: host_architecture(),
        endianness,
    }
}

/// Fails when `object_bytes` is not an object file matching `expected`'s format,
/// architecture, and endianness, so a wrong-format (Mach-O for a Linux target),
/// wrong-arch, or wrong-endian binary is rejected at inspection with a precise
/// diagnostic rather than crashing later with an exec-format error. Reads and
/// parses only; never executes the binary. An [`object::Architecture::Unknown`]
/// expectation (an exotic host this mapping does not know) is a precise error,
/// not a skipped gate: no supported CLI/framework release target can produce a
/// complete layout for such a host, so validating nothing would only defer the
/// failure to spawn time ().
pub fn ensure_target(object_bytes: &[u8], describe: &str, expected: &ExpectedTarget) -> Result<()> {
    if expected.architecture == object::Architecture::Unknown {
        anyhow::bail!(
            "cannot validate {describe}: this host's CPU architecture ({}) is not one the CLI can              authoritatively inspect binaries for, and no supported release target produces a              runtime layout for it",
            std::env::consts::ARCH
        );
    }
    let file = object::File::parse(object_bytes)
        .with_context(|| format!("{describe} is not a recognized object file (ELF/Mach-O/...)"))?;
    let format = file.format();
    let architecture = file.architecture();
    let endianness = file.endianness();
    if format != expected.format {
        anyhow::bail!(
            "{describe} is a {format:?} object file, but the selected target expects \
             {expected_format:?}; stage or build a runtime layout for the target platform \
             before running it",
            expected_format = expected.format
        );
    }
    if architecture != expected.architecture {
        anyhow::bail!(
            "{describe} is built for {architecture:?}, but the selected target expects \
             {expected_arch:?}; stage or build a runtime layout for the {expected_arch:?} \
             architecture before running it",
            expected_arch = expected.architecture
        );
    }
    if endianness != expected.endianness {
        anyhow::bail!(
            "{describe} is {endianness:?}-endian, but the selected target expects \
             {expected_endian:?}-endian; stage or build a runtime layout for the target platform \
             before running it",
            expected_endian = expected.endianness
        );
    }
    Ok(())
}

/// Reads `binary_path`, verifies it matches the `expected` target signature, and
/// returns its embedded participant metadata in one read.
pub fn inspect_selected_binary_for_target(
    binary_path: &Path,
    expected: &ExpectedTarget,
) -> Result<ParticipantMeta> {
    let data = fs::read(binary_path)
        .with_context(|| format!("failed to read {}", binary_path.display()))?;
    let describe = binary_path.display().to_string();
    ensure_target(&data, &describe, expected)?;
    extract_participant_metadata_from_bytes(&data, &describe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_runtime_contract::metadata::ParticipantKind;
    use phoxal_runtime_contract::version::RobotApiVersion;

    #[test]
    fn malformed_object_file_fails_with_a_clear_error() -> Result<()> {
        let dir = tempfile::tempdir().context("create tempdir")?;
        let not_an_object_file = dir.path().join("not-a-binary");
        fs::write(&not_an_object_file, b"not an object file")?;

        let err = extract_participant_metadata(&not_an_object_file).unwrap_err();
        assert!(
            err.to_string().contains("not a recognized object file"),
            "{err}"
        );
        Ok(())
    }

    /// Synthesizes an object file of a given format/architecture carrying the
    /// phoxal metadata section, so the reader is exercised against object
    /// shapes that are NOT the test host's native one.
    fn synthesize_object(
        format: object::BinaryFormat,
        arch: object::Architecture,
        section_name: &[u8],
        segment_name: &[u8],
        payload: &[u8],
    ) -> Vec<u8> {
        synthesize_object_endian(
            format,
            arch,
            object::Endianness::Little,
            section_name,
            segment_name,
            payload,
        )
    }

    /// [`synthesize_object`] with an explicit endianness, so a wrong-endian
    /// binary can be produced for the endianness gate.
    fn synthesize_object_endian(
        format: object::BinaryFormat,
        arch: object::Architecture,
        endianness: object::Endianness,
        section_name: &[u8],
        segment_name: &[u8],
        payload: &[u8],
    ) -> Vec<u8> {
        use object::write::Object;
        let mut obj = Object::new(format, arch, endianness);
        let section = obj.add_section(
            segment_name.to_vec(),
            section_name.to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        obj.append_section_data(section, payload, 1);
        obj.write().expect("synthesize object file")
    }

    /// The exact document a role macro embeds, for a binary that agrees with
    /// this CLI on every process-boundary contract. Written through the
    /// framework's own serialize twin, so the fixture cannot drift from the
    /// parser it is meant to satisfy.
    fn current_record(id: &str) -> serde_json::Value {
        serde_json::to_value(
            phoxal_runtime_contract::emit::ParticipantMetadataRecord::V0 {
                contract: phoxal_runtime_contract::emit::ParticipantContractRecord {
                    api: RobotApiVersion::new(0, 1),
                    schemas: CURRENT_SCHEMAS,
                    id,
                    kind: ParticipantKind::Service,
                    requirement: None,
                    config_schema: serde_json::json!({"type": "null"}),
                },
            },
        )
        .expect("the typed record serializes")
    }

    /// [`current_record`] as the bytes a linker section carries.
    fn current_record_bytes(id: &str) -> Vec<u8> {
        serde_json::to_vec(&current_record(id)).expect("a JSON value serializes")
    }

    #[test]
    fn extracts_metadata_from_foreign_format_and_arch_object_files() -> Result<()> {
        let payload = &current_record_bytes("drive");

        // aarch64 ELF (Linux robot / release binary shape), `.phoxal_meta`.
        let elf = synthesize_object(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            b".phoxal_meta",
            b"",
            payload,
        );
        let from_elf = extract_participant_metadata_from_bytes(&elf, "synthetic aarch64 ELF")?;
        assert_eq!(from_elf.id.as_str(), "drive");

        // x86_64 Mach-O (Apple release binary shape), `__DATA,__phoxal_meta`.
        let macho = synthesize_object(
            object::BinaryFormat::MachO,
            object::Architecture::X86_64,
            b"__phoxal_meta",
            b"__DATA",
            payload,
        );
        let from_macho =
            extract_participant_metadata_from_bytes(&macho, "synthetic x86_64 Mach-O")?;
        assert_eq!(from_macho.id.as_str(), "drive");
        Ok(())
    }

    #[test]
    fn host_target_binary_passes_and_a_foreign_arch_binary_is_rejected() -> Result<()> {
        let host = expected_target_for_host();
        // Pick a concrete arch that is NOT the host's, so the assertion holds
        // on any runner. The host path validates format too (), so the
        // synthetic host object must use the host's own container format and
        // section naming.
        let foreign = if host.architecture == object::Architecture::X86_64 {
            object::Architecture::Aarch64
        } else {
            object::Architecture::X86_64
        };
        let (section, segment): (&[u8], &[u8]) = match host.format {
            object::BinaryFormat::MachO => (b"__phoxal_meta", b"__DATA"),
            _ => (b".phoxal_meta", b""),
        };
        let host_object = synthesize_object_endian(
            host.format,
            host.architecture,
            host.endianness,
            section,
            segment,
            b"payload",
        );
        ensure_target(&host_object, "synthetic host object", &host)?;

        // Host format, foreign CPU: the arch gate (not the format gate) must
        // reject it.
        let foreign_object = synthesize_object_endian(
            host.format,
            foreign,
            host.endianness,
            section,
            segment,
            b"payload",
        );
        let error = ensure_target(&foreign_object, "bin/phoxal-service-drive", &host)
            .expect_err("a foreign-arch binary must be rejected");
        let message = error.to_string();
        assert!(message.contains("bin/phoxal-service-drive"), "{message}");
        assert!(message.contains("built for"), "{message}");
        Ok(())
    }

    /// Finding C: a same-CPU wrong-OS binary (a Mach-O x86_64 offered for
    /// `x86_64-unknown-linux-gnu`) is rejected on the container format, not
    /// waved through by a CPU-only check.
    #[test]
    fn a_same_cpu_wrong_os_binary_is_rejected() -> Result<()> {
        let linux = expected_target_for_triple("x86_64-unknown-linux-gnu")?;
        assert_eq!(linux.format, object::BinaryFormat::Elf);
        // Right CPU, wrong container format (Mach-O, an Apple binary).
        let macho = synthesize_object(
            object::BinaryFormat::MachO,
            object::Architecture::X86_64,
            b"__phoxal_meta",
            b"__DATA",
            b"payload",
        );
        let error = ensure_target(&macho, "bin/phoxal-service-drive", &linux)
            .expect_err("a Mach-O binary for a Linux target must be rejected");
        let message = error.to_string();
        assert!(message.contains("MachO"), "{message}");
        assert!(message.contains("Elf"), "{message}");
        Ok(())
    }

    /// Finding C: a triple the validator cannot authoritatively map is rejected
    /// outright with a "cannot validate target" error rather than silently
    /// passing every binary (the old CPU-only gate disabled itself on Unknown).
    #[test]
    fn an_unmappable_triple_is_rejected() {
        for triple in [
            "sparc64-unknown-linux-gnu", // arch not mapped
            "x86_64-unknown-haiku",      // OS not mapped
            "not-a-triple",              // nonsense
        ] {
            let error = expected_target_for_triple(triple)
                .expect_err("an unmappable triple must be rejected");
            assert!(
                error.to_string().contains("cannot validate target"),
                "{triple}: {error}"
            );
        }
        // The supported officials still map cleanly.
        assert!(expected_target_for_triple("aarch64-unknown-linux-gnu").is_ok());
        assert!(expected_target_for_triple("x86_64-unknown-linux-gnu").is_ok());
    }

    /// A missing metadata section must be a clear error, not a synthesized
    /// identity: a placeholder would let a binary with no section at all pass
    /// an identity check without supplying any real evidence.
    #[test]
    fn foreign_object_without_section_is_a_clear_error() {
        let elf = synthesize_object(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            b".some_other_section",
            b"",
            b"unrelated",
        );
        let error =
            extract_participant_metadata_from_bytes(&elf, "synthetic aarch64 ELF").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("synthetic aarch64 ELF"), "{message}");
        assert!(
            message.contains("no phoxal participant metadata section"),
            "{message}"
        );
    }

    /// A binary from another train is rejected before its config schema is
    /// trusted. The rejection is the *parse*, not a comparison: the diagnostic
    /// therefore names the token the binary carries, the token this CLI
    /// expects, and - from this module's own context - the fix for either
    /// direction.
    #[test]
    fn a_binary_on_another_api_revision_is_rejected_with_an_actionable_diagnostic() {
        let mut record = current_record("cleaning");
        record["api"] = serde_json::json!("phoxal/robot-api/v9.9");
        let elf = synthesize_object(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            b".phoxal_meta",
            b"",
            serde_json::to_vec(&record).unwrap().as_slice(),
        );
        let error = extract_participant_metadata_from_bytes(&elf, "bin/cleaning")
            .expect_err("a foreign API revision must be rejected");
        let message = format!("{error:#}");
        assert!(message.contains("bin/cleaning"), "{message}");
        assert!(message.contains("phoxal/robot-api/v9.9"), "{message}");
        assert!(message.contains("phoxal/robot-api/v0.1"), "{message}");
        assert!(message.contains("cargo update -p phoxal"), "{message}");
        assert!(message.contains("update the CLI"), "{message}");
    }

    /// The same for the persisted runtime grammar.
    #[test]
    fn a_binary_on_another_document_schema_is_rejected_naming_the_contract() {
        for unsupported in ["runtime/v0", "phoxal/runtime/v9"] {
            let mut record = current_record("drive");
            record["schemas"]["runtime"] = serde_json::json!(unsupported);
            let elf = synthesize_object(
                object::BinaryFormat::Elf,
                object::Architecture::Aarch64,
                b".phoxal_meta",
                b"",
                serde_json::to_vec(&record).unwrap().as_slice(),
            );
            let message = format!(
                "{:#}",
                extract_participant_metadata_from_bytes(&elf, "bin/phoxal-service-drive")
                    .expect_err("a foreign robot document schema must be rejected")
            );
            assert!(message.contains(unsupported), "{message}");
            assert!(message.contains("phoxal/runtime-bundle/v0"), "{message}");
        }
    }

    /// An unknown metadata schema tag and an unknown field are both rejected by
    /// the tagged document itself: there is no post-hoc string comparison the
    /// CLI could get wrong.
    #[test]
    fn an_unsupported_metadata_document_is_rejected() {
        let mut with_framework = current_record("drive");
        with_framework["framework"] = serde_json::json!("0.54.0");
        for record in [
            serde_json::json!({
                "schema": "phoxal/participant-metadata/v1",
                "id": "drive",
                "kind": "service",
                "config_schema": null,
            }),
            with_framework,
        ] {
            let bytes = serde_json::to_vec(&record).unwrap();
            let elf = synthesize_object(
                object::BinaryFormat::Elf,
                object::Architecture::Aarch64,
                b".phoxal_meta",
                b"",
                &bytes,
            );
            assert!(
                extract_participant_metadata_from_bytes(&elf, "bin/phoxal-service-drive").is_err(),
                "unsupported metadata document must be rejected: {record}",
            );
        }
    }
}
