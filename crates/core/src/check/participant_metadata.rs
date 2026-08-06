//! Compile-time participant metadata extraction.
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

use anyhow::{Context, Result, bail};
use object::{Object, ObjectSection};
use phoxal_runtime_contract::{
    ApiId, COMPONENT_DOCUMENT_SCHEMA, LAUNCH_ABI, ParticipantKind, ParticipantMetadata,
    ParticipantSchemas, ROBOT_DOCUMENT_SCHEMA, SIMULATION_DOCUMENT_SCHEMA, SchemaId,
};

/// The process-boundary contracts this `phoxal` release speaks.
///
/// Every identifier is imported from the crate that owns it - there is no
/// second spelling of any of these strings anywhere in this repository, and no
/// framework SemVer anywhere in the set. A binary agrees with this CLI exactly
/// when its embedded record matches every field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibilitySet {
    pub api: ApiId,
    pub schemas: ParticipantSchemas,
}

impl CompatibilitySet {
    /// The set this CLI release was built against.
    #[must_use]
    pub fn current() -> Self {
        Self {
            api: ApiId::new(<phoxal_api::v0_1::Api as phoxal_bus::ApiVersion>::ID),
            schemas: ParticipantSchemas {
                bus: SchemaId::new(phoxal_bus::BUS_ABI),
                launch: SchemaId::new(LAUNCH_ABI),
                robot: SchemaId::new(ROBOT_DOCUMENT_SCHEMA),
                component: SchemaId::new(COMPONENT_DOCUMENT_SCHEMA),
                simulation: SchemaId::new(SIMULATION_DOCUMENT_SCHEMA),
            },
        }
    }

    /// Reject a binary that does not speak every contract in this set, naming
    /// what mismatched and which side has to be updated.
    fn verify(&self, meta: &ParticipantMeta, describe: &str) -> Result<()> {
        if meta.api != self.api {
            bail!(
                "participant `{id}` uses robot API {found},\nbut this phoxal CLI requires \
                 {expected}.\n\nUpdate the project dependency and rebuild:\n    cargo update -p \
                 phoxal\n\n({describe})",
                id = meta.id,
                found = meta.api,
                expected = self.api,
            );
        }
        for (contract, found, expected, owner) in [
            ("bus ABI", &meta.schemas.bus, &self.schemas.bus, Side::Cli),
            (
                "participant launch ABI",
                &meta.schemas.launch,
                &self.schemas.launch,
                Side::Cli,
            ),
            (
                "robot document schema",
                &meta.schemas.robot,
                &self.schemas.robot,
                Side::Project,
            ),
            (
                "component document schema",
                &meta.schemas.component,
                &self.schemas.component,
                Side::Project,
            ),
            (
                "simulation document schema",
                &meta.schemas.simulation,
                &self.schemas.simulation,
                Side::Project,
            ),
        ] {
            if found != expected {
                bail!(
                    "participant `{id}` speaks {contract} {found},\nbut this phoxal CLI requires \
                     {expected}.\n\n{fix}\n\n({describe})",
                    id = meta.id,
                    fix = owner.fix(),
                );
            }
        }
        Ok(())
    }
}

/// Which side of a contract mismatch has to move.
#[derive(Clone, Copy, Debug)]
enum Side {
    /// The CLI is older than the binary: the operator updates the tool.
    Cli,
    /// The project crate is older than the CLI: the operator updates the crate.
    Project,
}

impl Side {
    const fn fix(self) -> &'static str {
        match self {
            Self::Cli => {
                "The built binary is newer than this CLI. Update the phoxal CLI to the release \
                 that ships this contract."
            }
            Self::Project => {
                "Update the project dependency and rebuild:\n    cargo update -p phoxal"
            }
        }
    }
}

/// One binary's accepted embedded compatibility record: the tagged `V0`
/// document destructured into the fields callers actually branch on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParticipantMeta {
    pub api: ApiId,
    pub schemas: ParticipantSchemas,
    pub id: String,
    pub kind: ParticipantKind,
    pub config_schema: serde_json::Value,
}

impl From<ParticipantMetadata> for ParticipantMeta {
    fn from(metadata: ParticipantMetadata) -> Self {
        let ParticipantMetadata::V0 {
            api,
            schemas,
            id,
            kind,
            config_schema,
        } = metadata;
        Self {
            api,
            schemas,
            id,
            kind,
            config_schema,
        }
    }
}

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
    let metadata =
        phoxal_runtime_contract::parse_participant_metadata(&bytes).with_context(|| {
            format!("phoxal participant metadata section in {describe} is not valid JSON")
        })?;
    let meta = ParticipantMeta::from(metadata);
    CompatibilitySet::current().verify(&meta, describe)?;
    Ok(meta)
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
/// hosts, PE on Windows, ELF elsewhere. Host layout validation checks it
/// explicitly (#936): deferring format to the OS exec would let a
/// same-CPU foreign-OS bundle (an aarch64 Linux bundle on an Apple Silicon
/// host) validate and then crash at spawn with an exec-format error.
#[must_use]
pub fn host_binary_format() -> object::BinaryFormat {
    if cfg!(target_os = "macos") {
        object::BinaryFormat::MachO
    } else if cfg!(target_os = "windows") {
        object::BinaryFormat::Pe
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
        "x86" | "i386" | "i586" | "i686" => object::Architecture::I386,
        "aarch64" | "arm64" => object::Architecture::Aarch64,
        "arm" | "armv7" | "armv7a" | "armv6" | "thumbv7neon" => object::Architecture::Arm,
        "riscv64" | "riscv64gc" => object::Architecture::Riscv64,
        "riscv32" | "riscv32gc" | "riscv32imac" | "riscv32imc" => object::Architecture::Riscv32,
        "powerpc64" | "powerpc64le" => object::Architecture::PowerPc64,
        "powerpc" => object::Architecture::PowerPc,
        "s390x" => object::Architecture::S390x,
        "loongarch64" => object::Architecture::LoongArch64,
        "mips64" | "mips64el" => object::Architecture::Mips64,
        "mips" | "mipsel" => object::Architecture::Mips,
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
        // Unambiguously little-endian.
        "x86_64" | "x86" | "i386" | "i586" | "i686" | "aarch64" | "arm" | "armv7" | "armv7a"
        | "armv6" | "thumbv7neon" | "riscv64" | "riscv64gc" | "riscv32" | "riscv32gc"
        | "riscv32imac" | "riscv32imc" | "loongarch64" | "powerpc64le" | "mips64el" | "mipsel" => {
            Some(object::Endianness::Little)
        }
        // Classic big-endian families.
        "powerpc64" | "powerpc" | "s390x" | "mips64" | "mips" => Some(object::Endianness::Big),
        _ => None,
    }
}

/// The object [`object::BinaryFormat`] the OS component of a Rust target triple
/// produces: Linux triples are ELF, Apple triples Mach-O, Windows triples PE.
/// Returns `None` for an OS this mapping cannot decide, forcing
/// [`expected_target_for_triple`] to reject the triple.
fn format_for_triple(triple: &str) -> Option<object::BinaryFormat> {
    if triple.contains("-linux-") || triple.ends_with("-linux") {
        Some(object::BinaryFormat::Elf)
    } else if triple.contains("-apple-") || triple.contains("-darwin") {
        Some(object::BinaryFormat::MachO)
    } else if triple.contains("-windows-") || triple.contains("-pc-windows") {
        Some(object::BinaryFormat::Pe)
    } else {
        None
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
             aarch64-unknown-linux-gnu or x86_64-unknown-linux-gnu)."
        ),
    }
}

/// The [`ExpectedTarget`] for the host this CLI process runs on, from
/// `std::env::consts` and the host's native endianness. Used by an in-place
/// `run`/`start`: a staged/extracted layout only ever runs on the host it was
/// prepared for, so its selected binaries must match the host signature -
/// format included, so a same-CPU foreign-OS bundle fails at inspection with a
/// precise diagnostic instead of an exec-format crash (#936).
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
/// failure to spawn time (#936).
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
/// returns its embedded participant metadata - the two off-disk inspections a
/// layout run performs on a selected binary, in one read.
/// [`inspect_selected_binary`] is the host variant used by an in-place run;
/// `phoxal build --target` passes the declared target's signature instead.
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

/// Reads `binary_path`, verifies it matches the host target signature, and
/// returns its embedded participant metadata - the two off-disk inspections a
/// layout run performs on a selected binary, in one read.
pub fn inspect_selected_binary(binary_path: &Path) -> Result<ParticipantMeta> {
    inspect_selected_binary_for_target(binary_path, &expected_target_for_host())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn extracts_metadata_from_foreign_format_and_arch_object_files() -> Result<()> {
        let payload = br#"{"schema":"phoxal/participant-metadata/v0","id":"drive","kind":"service","config_schema":{"type":"null"}}"#;

        // aarch64 ELF (Linux robot / release binary shape), `.phoxal_meta`.
        let elf = synthesize_object(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            b".phoxal_meta",
            b"",
            payload,
        );
        let from_elf = extract_participant_metadata_from_bytes(&elf, "synthetic aarch64 ELF")?;
        assert_eq!(from_elf.id, "drive");

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
        assert_eq!(from_macho.id, "drive");
        Ok(())
    }

    #[test]
    fn host_target_binary_passes_and_a_foreign_arch_binary_is_rejected() -> Result<()> {
        let host = expected_target_for_host();
        // Pick a concrete arch that is NOT the host's, so the assertion holds
        // on any runner. The host path validates format too (#936), so the
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

    /// Finding C: a wrong-endian binary is rejected against the declared target's
    /// endianness.
    #[test]
    fn a_wrong_endian_binary_is_rejected() -> Result<()> {
        // s390x is authoritatively big-endian; a little-endian s390x object is
        // therefore wrong for the declared target.
        let s390x = expected_target_for_triple("s390x-unknown-linux-gnu")?;
        assert_eq!(s390x.endianness, object::Endianness::Big);
        let little = synthesize_object_endian(
            object::BinaryFormat::Elf,
            object::Architecture::S390x,
            object::Endianness::Little,
            b".phoxal_meta",
            b"",
            b"payload",
        );
        let error = ensure_target(&little, "bin/phoxal-service-drive", &s390x)
            .expect_err("a wrong-endian binary must be rejected");
        assert!(error.to_string().contains("endian"), "{error}");

        // The matching big-endian object passes.
        let big = synthesize_object_endian(
            object::BinaryFormat::Elf,
            object::Architecture::S390x,
            object::Endianness::Big,
            b".phoxal_meta",
            b"",
            b"payload",
        );
        ensure_target(&big, "bin/phoxal-service-drive", &s390x)?;
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
}
