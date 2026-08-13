//! Project participant metadata: reading a binary's embedded contract, and
//! validating it against the framework the project selected.
//!
//! A `#[phoxal::brain]`/`service`/`driver`/`simulator` attribute embeds one JSON
//! manifest per participant binary in a dedicated linker section -
//! `__DATA,__phoxal_meta` on Mach-O,
//! `.phoxal_meta` everywhere else (`phoxal-macros/src/authoring.rs`'s
//! `link_section_attrs`). The CLI reads the section's bytes straight out of
//! the object file without ever executing the artifact. This module targets
//! the same linker-section shape `phoxal-macros` embeds and is format- and
//! architecture-agnostic through the `object` crate.
//!
//! Reading and judging are two separate steps here.
//! [`extract_participant_metadata`] answers only what a binary claims, with no
//! policy at all; [`ensure_built_for_project`] is the whole compatibility
//! decision, and its authority is the framework the *project* selected in its
//! lockfile. The CLI's own linked train is not an input: a robot project
//! chooses its framework, and an installed CLI product version is not a
//! compatibility identity for anything the project builds.
//!
//! This is authority and diagnostics, not cross-line build support. The
//! toolchain still stages and launches through one CLI and one sibling
//! `phoxald`, and those speak their own linked train, so the lines a project
//! can actually be *run* on remain the CLI's native one. What changes is that
//! a mismatch is now stated against the project's own selection, in terms an
//! operator can act on in the project.
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use object::{Object, ObjectSection};
use phoxal_runtime_contract::metadata::{ParticipantContract, ParticipantMetadata};
use phoxal_runtime_contract::version::FrameworkVersion;

/// One binary's embedded contract: the tagged `V0` document destructured into
/// the fields callers actually branch on.
///
/// The document's whole compatibility claim is the framework train it was
/// built from, and that claim is settled by the line that train belongs to.
/// Holding this value proves only that the *grammar* is one this CLI reads;
/// whether the binary belongs to a given project is
/// [`ensure_built_for_project`].
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
    // The parse settles the document grammar: a schema tag, framework
    // spelling, or field set this CLI does not implement fails here, naming
    // what it found. That is a statement about the reader, not about any
    // project: the context below adds only the two things serde cannot know -
    // which file it was, and what an operator does about it.
    let metadata = ParticipantMetadata::from_bytes(&bytes).with_context(|| {
        format!(
            "{describe} carries a phoxal participant metadata document this CLI cannot \
             read.\n\nIf the binary is older, update the project dependency and rebuild:\n    \
             cargo update -p phoxal\n\nIf it is newer, update the phoxal CLI to a release that \
             reads this document."
        )
    })?;
    Ok(metadata.contract().clone())
}

/// Extracts and parses `binary_path`'s embedded participant metadata: reads
/// the compiled-in linker section straight off the object file, never
/// executing it. Policy-free - see [`ensure_built_for_project`] for the
/// compatibility decision.
pub fn extract_participant_metadata(binary_path: &Path) -> Result<ParticipantMeta> {
    let data = fs::read(binary_path)
        .with_context(|| format!("failed to read {}", binary_path.display()))?;
    extract_participant_metadata_from_bytes(&data, &binary_path.display().to_string())
}

/// The whole build-time compatibility decision for one participant binary:
/// the train it was built from must be on the line the project targets.
///
/// The authority is the project, never the CLI. A robot project selects its
/// framework in its own Cargo graph, so `project_target` is the framework that
/// project's committed `Cargo.lock` resolved; the version of the `phoxal`
/// product doing the reading says nothing about whether these contracts fit
/// together.
///
/// Comparing every selected binary against this one target is also the
/// project-side proof that the finalized participant graph shares a single
/// compatibility line: agreeing with the project's line is transitive, so a
/// bundle that passes cannot be mixed. `phoxald` re-derives the same rule over
/// a bundle it opens; this check is what makes the disagreement arrive while
/// the operator is still building, naming the binary that carries it.
pub fn ensure_built_for_project(
    contract: &ParticipantMeta,
    describe: &str,
    project_target: FrameworkVersion,
) -> Result<()> {
    anyhow::ensure!(
        contract.framework.is_compatible_with(project_target),
        "{describe} was built from phoxal framework {}, which is not on the {} line this project \
         targets (phoxal {project_target} in Cargo.lock). Run `cargo update -p phoxal` in the \
         project or rebuild the binary.",
        contract.framework,
        project_target.compatibility_line(),
    );
    Ok(())
}

/// The framework train every synthesized participant fixture in this crate is
/// built from, and the train its fixture projects lock.
///
/// Fixtures state a train explicitly instead of borrowing the one this CLI
/// links, because that train is not an authority for anything a project
/// builds - a fixture that reached for it would quietly reintroduce the
/// dependency these checks exist to remove.
#[cfg(test)]
pub(crate) const FIXTURE_FRAMEWORK: FrameworkVersion = FrameworkVersion::new(0, 42, 0);

/// [`extract_participant_metadata`] followed by [`ensure_built_for_project`]:
/// what a binary claims, judged against what the project selected.
pub fn extract_participant_metadata_for_project(
    binary_path: &Path,
    project_target: FrameworkVersion,
) -> Result<ParticipantMeta> {
    let describe = binary_path.display().to_string();
    let contract = extract_participant_metadata(binary_path)?;
    ensure_built_for_project(&contract, &describe, project_target)?;
    Ok(contract)
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
/// explicitly: deferring format to the OS exec would let a
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
/// precise diagnostic instead of an exec-format crash.
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
/// failure until spawn time.
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
///
/// Deliberately no framework judgement: this serves bundle verification, and a
/// finalized bundle carries its own compatibility statement. Its document is
/// constructible only when every selected artifact shares one line, and the
/// caller proves each binary's embedded contract equals the one recorded for
/// it, so the bundle answers the question by itself - including for an
/// extracted archive, where there is no project to ask.
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

    /// The exact document a role macro embeds, for a binary built from the
    /// train the fixture project targets. Written through the framework's own
    /// serialize twin, so the fixture cannot drift from the parser it is meant
    /// to satisfy.
    fn current_record(id: &str) -> serde_json::Value {
        serde_json::to_value(
            phoxal_runtime_contract::emit::ParticipantMetadataRecord::V0 {
                contract: phoxal_runtime_contract::emit::ParticipantContractRecord {
                    framework: FIXTURE_FRAMEWORK,
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
        // on any runner. The host path validates format too, so the
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

    /// A same-CPU wrong-OS binary (a Mach-O x86_64 offered for
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

    /// A triple the validator cannot authoritatively map is rejected
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

    /// Extraction is policy-free: a well-formed record from ANY train reads
    /// back exactly as written, because what a binary claims is a separate
    /// question from whether it belongs to a project.
    #[test]
    fn extraction_reports_whatever_train_a_binary_claims() {
        let mut record = current_record("cleaning");
        record["framework"] = serde_json::json!("9.9.9");
        let elf = synthesize_object(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            b".phoxal_meta",
            b"",
            serde_json::to_vec(&record).unwrap().as_slice(),
        );
        let contract = extract_participant_metadata_from_bytes(&elf, "bin/cleaning")
            .expect("reading a binary's claim never judges it");
        assert_eq!(contract.framework, FrameworkVersion::new(9, 9, 9));
    }

    /// A binary off the line the PROJECT targets is rejected before its config
    /// schema is trusted, and the diagnostic is written from the project's
    /// point of view: the train the binary carries, the line the project
    /// targets, the exact `phoxal` its lockfile selected, and what to do in
    /// the project.
    #[test]
    fn a_binary_off_the_project_line_is_rejected_with_a_project_authored_diagnostic() {
        let mut record = current_record("cleaning");
        record["framework"] = serde_json::json!("9.9.9");
        let elf = synthesize_object(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            b".phoxal_meta",
            b"",
            serde_json::to_vec(&record).unwrap().as_slice(),
        );
        let contract = extract_participant_metadata_from_bytes(&elf, "bin/cleaning")
            .expect("the document parses");
        let message = format!(
            "{:#}",
            ensure_built_for_project(&contract, "bin/cleaning", FIXTURE_FRAMEWORK)
                .expect_err("a binary off the project's line must be rejected")
        );
        assert_eq!(
            message,
            "bin/cleaning was built from phoxal framework 9.9.9, which is not on the 0.42.x line \
             this project targets (phoxal 0.42.0 in Cargo.lock). Run `cargo update -p phoxal` in \
             the project or rebuild the binary."
        );
    }

    /// A binary from a different train on the project's line is accepted:
    /// trains on one line speak the same contracts, so a rebuild is not what
    /// the operator owes here.
    #[test]
    fn a_binary_from_another_train_on_the_project_line_is_accepted() {
        let neighbour = FrameworkVersion::new(
            FIXTURE_FRAMEWORK.major(),
            FIXTURE_FRAMEWORK.minor(),
            FIXTURE_FRAMEWORK.patch().wrapping_add(1),
        );
        assert_ne!(neighbour, FIXTURE_FRAMEWORK);
        let mut record = current_record("cleaning");
        record["framework"] = serde_json::json!(neighbour.to_string());
        let elf = synthesize_object(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            b".phoxal_meta",
            b"",
            serde_json::to_vec(&record).unwrap().as_slice(),
        );
        let contract = extract_participant_metadata_from_bytes(&elf, "bin/cleaning")
            .expect("a train on this line is accepted");
        assert_eq!(contract.framework, neighbour);
        ensure_built_for_project(&contract, "bin/cleaning", FIXTURE_FRAMEWORK)
            .expect("a neighbouring train on the project's line belongs to the project");
    }

    /// A framework version spelled any way but the canonical SemVer string is
    /// not a document this CLI reads at all, so it fails at the parse rather
    /// than at the comparison. That failure is about the reader, so it is the
    /// one place a remediation may still mention the CLI.
    #[test]
    fn a_non_canonical_framework_spelling_is_rejected_by_the_parse() {
        for unsupported in ["v0.56.2", "0.56", "0.56.2-rc.1"] {
            let mut record = current_record("drive");
            record["framework"] = serde_json::json!(unsupported);
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
                    .expect_err("a non-canonical framework spelling must be rejected")
            );
            assert!(message.contains(unsupported), "{message}");
            assert!(message.contains("this CLI cannot read"), "{message}");
        }
    }

    /// An unknown metadata schema tag, a missing framework claim, and an
    /// unknown field are all rejected by the tagged document itself: there is
    /// no post-hoc string comparison the CLI could get wrong.
    #[test]
    fn an_unsupported_metadata_document_is_rejected() {
        let mut without_framework = current_record("drive");
        without_framework
            .as_object_mut()
            .expect("the record is an object")
            .remove("framework");
        for record in [
            serde_json::json!({
                "schema": "phoxal/participant-metadata/v1",
                "framework": FIXTURE_FRAMEWORK.to_string(),
                "id": "drive",
                "kind": "service",
                "config_schema": null,
            }),
            without_framework,
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
