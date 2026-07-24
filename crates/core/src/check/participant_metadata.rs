//! Compile-time participant metadata extraction (X-tools slice).
//!
//! A `#[phoxal::service]`/driver/tool/simulator attribute embeds one JSON
//! manifest per participant binary in a dedicated linker section -
//! `__DATA,__phoxal_meta` on Mach-O,
//! `.phoxal_api_meta` everywhere else (`phoxal-macros/src/authoring.rs`'s
//! `link_section_attrs`). `phoxal-cli` no longer executes a built artifact's
//! `emit-apis` subcommand to learn its contract surface (that runtime
//! subcommand is gone): it reads the section's bytes straight out of the
//! object file, without ever executing the artifact. This module mirrors the
//! framework's own `xtask::release::metadata` reference implementation
//! (`phoxal/framework` `xtask/src/release/metadata.rs`) and is format- and
//! architecture-agnostic (via the `object` crate).
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use object::{Object, ObjectSection};
pub use phoxal::participant::metadata::{ParticipantMeta, ParticipantMetaContract};

/// The linker section names a participant attribute places its metadata
/// static under, tried in order. `object`'s generic [`Object::section_by_name`]
/// matches on the section name alone (Mach-O segment qualification is not
/// part of the match), so no per-format branching is needed here - the two
/// candidate names are simply disjoint across the object formats this
/// framework ships binaries for.
pub const SECTION_NAMES: [&str; 2] = [".phoxal_api_meta", "__phoxal_meta"];

/// Parses `object_bytes` as an object file and returns the bytes of its
/// participant metadata section, trying each candidate section
/// name in [`SECTION_NAMES`] in turn. `Ok(None)` means the object file parsed
/// fine but carries no such section at all - the expected, valid shape for a
/// binary with no participant attribute. A malformed/unrecognized *object
/// file* is still a hard error. `describe` names the source (a file path) for
/// error messages.
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
/// nothing. A binary with no section at all (see [`read_meta_section`]) parses
/// as an empty contract list and no-config schema, not an error.
pub fn extract_participant_metadata_from_bytes(
    object_bytes: &[u8],
    describe: &str,
) -> Result<ParticipantMeta> {
    let Some(bytes) = read_meta_section(object_bytes, describe)? else {
        return Ok(ParticipantMeta {
            participant_api: "()".to_string(),
            contracts: Vec::new(),
            config_schema: serde_json::json!({ "type": "null" }),
        });
    };
    phoxal::participant::metadata::parse_participant_metadata(&bytes).with_context(|| {
        format!("phoxal participant metadata section in {describe} is not valid JSON")
    })
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
/// [`std::env::consts::ARCH`]. A layout run only launches binaries built for
/// the host, so a selected binary's architecture is compared against this
/// before it is ever spawned. Unknown/exotic host arches map to
/// [`object::Architecture::Unknown`], which compares equal to nothing and so
/// disables the arch gate rather than rejecting every binary.
#[must_use]
pub fn host_architecture() -> object::Architecture {
    match std::env::consts::ARCH {
        "x86_64" => object::Architecture::X86_64,
        "x86" => object::Architecture::I386,
        "aarch64" => object::Architecture::Aarch64,
        "arm" => object::Architecture::Arm,
        "riscv64" => object::Architecture::Riscv64,
        "riscv32" => object::Architecture::Riscv32,
        "powerpc64" => object::Architecture::PowerPc64,
        "powerpc" => object::Architecture::PowerPc,
        "s390x" => object::Architecture::S390x,
        "loongarch64" => object::Architecture::LoongArch64,
        "mips64" => object::Architecture::Mips64,
        "mips" => object::Architecture::Mips,
        _ => object::Architecture::Unknown,
    }
}

/// Fails when `object_bytes` is an object file built for an architecture this
/// host cannot execute, so a foreign-architecture binary (e.g. an
/// `aarch64-unknown-linux-gnu` bundle unpacked on an `x86_64` host) is rejected
/// at inspection with a precise diagnostic rather than crashing later with an
/// exec-format error. Reads and parses only; never executes the binary. When
/// the host architecture is not one this mapping knows, the gate is skipped
/// (returns `Ok`) rather than rejecting a binary it cannot reason about.
pub fn ensure_host_architecture(object_bytes: &[u8], describe: &str) -> Result<()> {
    let file = object::File::parse(object_bytes)
        .with_context(|| format!("{describe} is not a recognized object file (ELF/Mach-O/...)"))?;
    let host = host_architecture();
    let binary = file.architecture();
    if host == object::Architecture::Unknown || binary == host {
        return Ok(());
    }
    anyhow::bail!(
        "{describe} is built for {binary:?}, but this host runs {host:?}; \
         stage or build a runtime layout for the host architecture before running it"
    )
}

/// Reads `binary_path`, verifies it is executable on the host architecture,
/// and returns its embedded participant metadata - the two off-disk
/// inspections a layout run performs on a selected binary, in one read.
pub fn inspect_selected_binary(binary_path: &Path) -> Result<ParticipantMeta> {
    let data = fs::read(binary_path)
        .with_context(|| format!("failed to read {}", binary_path.display()))?;
    let describe = binary_path.display().to_string();
    ensure_host_architecture(&data, &describe)?;
    extract_participant_metadata_from_bytes(&data, &describe)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    /// End-to-end proof (mirrors the framework's own acceptance test in
    /// `xtask/src/release/metadata.rs`): build a real participant binary -
    /// `tests/fixtures/api-fixture`, a workspace member with a real
    /// `#[derive(phoxal::Api)]` struct publishing
    /// `phoxal_api::v0_1::drive::Target` (the fixture writes the fully
    /// qualified path rather than a `use ... as api;` alias, since the macro
    /// records the contract type verbatim as written in source) - and
    /// extract its section from the actual built artifact on disk, asserting
    /// the parsed contracts match what the fixture's `main.rs` declares.
    /// Proves the reader against the real linker section a genuine
    /// participant emits, not hand-rolled JSON.
    #[test]
    fn extracts_real_fixture_binary_metadata() -> Result<()> {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let package_name = "phoxal-cli-test-api-fixture";
        let status = Command::new("cargo")
            .args(["build", "--quiet", "-p", package_name])
            .current_dir(&workspace_root)
            .status()
            .with_context(|| format!("failed to spawn cargo build for {package_name}"))?;
        assert!(status.success(), "cargo build -p {package_name} failed");

        let binary_path = workspace_root
            .join("target")
            .join("debug")
            .join(format!("{package_name}{}", std::env::consts::EXE_SUFFIX));
        assert!(
            binary_path.is_file(),
            "expected built binary at {}",
            binary_path.display()
        );

        let meta = extract_participant_metadata(&binary_path)?;
        assert_eq!(meta.participant_api, "Api");
        assert_eq!(
            meta.config_schema,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "Config",
                "type": "object",
                "properties": { "gain": { "type": "number", "format": "double" } },
                "required": ["gain"]
            })
        );
        assert_eq!(
            meta.contracts,
            vec![ParticipantMetaContract {
                role: "publish".to_string(),
                version: "v0.1".to_string(),
                contract: "drive::Target".to_string(),
                external: false,
            }]
        );
        Ok(())
    }

    /// A privileged tool defaults to `Api = ()`, so it never derives
    /// `#[derive(phoxal::Api)]` and its binary carries no metadata section at
    /// all - a valid, expected shape (zero contracts and a no-config schema), not
    /// an extraction error. Proven end-to-end against the CLI's own compiled
    /// `phoxal` binary, which has no participant attribute.
    #[test]
    fn a_real_binary_with_no_section_parses_as_zero_contracts() -> Result<()> {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let status = Command::new("cargo")
            .args(["build", "--quiet", "--bin", "phoxal"])
            .current_dir(&manifest_dir)
            .status()
            .context("failed to spawn cargo build for the phoxal binary")?;
        assert!(status.success(), "cargo build --bin phoxal failed");

        let binary_path = manifest_dir
            .join("target")
            .join("debug")
            .join(format!("phoxal{}", std::env::consts::EXE_SUFFIX));
        assert!(
            binary_path.is_file(),
            "expected built binary at {}",
            binary_path.display()
        );

        let meta = extract_participant_metadata(&binary_path)?;
        assert!(meta.contracts.is_empty());
        Ok(())
    }

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
        use object::write::Object;
        let mut obj = Object::new(format, arch, object::Endianness::Little);
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
        let payload =
            br#"{"participant_api":"Api","contracts":[{"role":"publish","version":"v0.1","contract":"drive::Target","external":false}],"config_schema":{"type":"null"}}"#;
        let expected = vec![ParticipantMetaContract {
            role: "publish".to_string(),
            version: "v0.1".to_string(),
            contract: "drive::Target".to_string(),
            external: false,
        }];

        // aarch64 ELF (Linux robot / release binary shape), `.phoxal_api_meta`.
        let elf = synthesize_object(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            b".phoxal_api_meta",
            b"",
            payload,
        );
        let from_elf = extract_participant_metadata_from_bytes(&elf, "synthetic aarch64 ELF")?;
        assert_eq!(from_elf.contracts, expected);

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
        assert_eq!(from_macho.contracts, expected);
        Ok(())
    }

    #[test]
    fn host_arch_binary_passes_and_a_foreign_arch_binary_is_rejected() -> Result<()> {
        // Pick a concrete arch that is NOT the host's, so the assertion holds
        // on any runner. The paired host-arch object must pass.
        let foreign = if host_architecture() == object::Architecture::X86_64 {
            object::Architecture::Aarch64
        } else {
            object::Architecture::X86_64
        };
        let host_object = synthesize_object(
            object::BinaryFormat::Elf,
            host_architecture(),
            b".phoxal_api_meta",
            b"",
            b"payload",
        );
        ensure_host_architecture(&host_object, "synthetic host ELF")?;

        let foreign_object = synthesize_object(
            object::BinaryFormat::Elf,
            foreign,
            b".phoxal_api_meta",
            b"",
            b"payload",
        );
        let error = ensure_host_architecture(&foreign_object, "bin/phoxal-service-drive")
            .expect_err("a foreign-arch binary must be rejected");
        let message = error.to_string();
        assert!(message.contains("bin/phoxal-service-drive"), "{message}");
        assert!(message.contains("built for"), "{message}");
        Ok(())
    }

    #[test]
    fn foreign_object_without_section_is_zero_contracts() -> Result<()> {
        let elf = synthesize_object(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            b".some_other_section",
            b"",
            b"unrelated",
        );
        let meta = extract_participant_metadata_from_bytes(&elf, "synthetic aarch64 ELF")?;
        assert!(meta.contracts.is_empty());
        Ok(())
    }
}
