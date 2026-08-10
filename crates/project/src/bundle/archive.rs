//! Deterministic `build.phoxal` archiving of a staged runtime layout.
//!
//! `build.phoxal` is a gzipped tar of a staged runtime layout - the compiled
//! canonical `robot.yaml`, `assets/`, and the flat `bin/` store - written so that
//! identical staged contents always produce byte-identical archives. That is
//! what lets a rebuild or a relink of unchanged content re-produce the same
//! digest, so a bundle is content-addressable and independently attestable.
//!
//! Determinism comes from normalizing everything the filesystem would otherwise
//! vary: entries are emitted in sorted path order; every entry's mtime is a
//! fixed epoch; uid/gid are `0` with cleared user/group names; and modes carry
//! only the executable bit (`0o755` for executables and directories, `0o644`
//! otherwise). Paths are stored relative with `/` separators and no `.`/`..`
//! components, so extraction can never escape its destination.
//!
//! The archive is not executable. Its `assets/runtime.json` compatibility
//! declaration is ordinary deterministic runtime content beside the canonical
//! model, assets, and binaries. Plain
//! `tar -xzf build.phoxal` extracts it identically to [`extract_build_archive`];
//! the helper here only adds the path-escape guard.

use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tar::{Archive, EntryType, Header};

/// The conventional extension of a built bundle. The default output for
/// `phoxal build --target <TRIPLE>` is `<project>/.phoxal/<triple>.build.phoxal`.
pub const BUILD_ARCHIVE_EXTENSION: &str = "build.phoxal";

/// A fixed timestamp stamped on every archived entry so mtime never varies the
/// output. Value is arbitrary but constant (2020-01-01T00:00:00Z); using a
/// non-zero epoch keeps extracted files from tripping tools that treat mtime 0
/// as "missing".
const FIXED_MTIME: u64 = 1_577_836_800;

/// Write the staged runtime layout at `layout_root` to `output` as a
/// deterministic `build.phoxal`, returning the archive's hex SHA-256 digest.
///
/// `output` must be a sibling of (or otherwise outside) `layout_root`: the
/// archive is never written inside the tree it archives, since that would fold a
/// partially written archive into its own contents. The caller enforces the
/// sibling default; this function additionally refuses an `output` nested under
/// `layout_root`.
pub fn write_build_archive(layout_root: &Path, output: &Path) -> Result<String> {
    let layout_root = layout_root.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize staged layout {}",
            layout_root.display()
        )
    })?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    }
    // Guard the "never inside the tree it archives" rule against a caller passing
    // an `--output` under the staged layout.
    if let Ok(canonical_parent) = output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        && (canonical_parent == layout_root || canonical_parent.starts_with(&layout_root))
    {
        bail!(
            "refusing to write build.phoxal inside the staged layout it archives ({}); \
                 choose an --output outside {}",
            output.display(),
            layout_root.display()
        );
    }

    let entries = collect_entries(&layout_root)?;
    let bytes = archive_bytes(&layout_root, &entries)?;

    let digest = hex::encode(Sha256::digest(&bytes));
    fs::write(output, &bytes)
        .with_context(|| format!("failed to write build archive {}", output.display()))?;
    Ok(digest)
}

/// One archived entry, resolved to its normalized relative path and whether it
/// is a directory. Regular files carry an executable flag; directories and
/// non-executable files normalize to `0o755`/`0o644`.
struct Entry {
    /// Slash-separated relative path under the layout root, no `.`/`..`.
    rel: String,
    absolute: PathBuf,
    is_dir: bool,
    executable: bool,
}

/// The bookkeeping files `cargo install --root` leaves in the candidate root.
/// They embed absolute host paths, so a bundle containing them is not
/// reproducible - and nothing at runtime reads them.
pub(crate) const CARGO_INSTALL_BOOKKEEPING: [&str; 2] = [".crates.toml", ".crates2.json"];

/// Recursively collect every directory and regular file under `root`, sorted by
/// relative path so the archive's entry order is stable regardless of the
/// filesystem's directory iteration order.
fn collect_entries(root: &Path) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    collect_into(root, root, &mut entries)?;
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(entries)
}

fn collect_into(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> Result<()> {
    let read = fs::read_dir(dir)
        .with_context(|| format!("failed to read staged directory {}", dir.display()))?;
    for entry in read {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        // The staged layout has no `.phoxal` of its own by construction. An
        // ordinary extracted layout may create local runtime state there; an
        // installed runtime redirects all state outside its immutable release.
        // Never fold state from a prior ordinary run into a bundle.
        if dir == root && path.file_name() == Some(std::ffi::OsStr::new(".phoxal")) {
            continue;
        }
        // `cargo install --root <candidate>` writes its own bookkeeping beside
        // `bin/`. Those files record absolute host paths, so they are neither
        // bundle content nor deterministic across machines.
        if dir == root
            && path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| CARGO_INSTALL_BOOKKEEPING.contains(&name))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to stat {}", path.display()))?;
        // The staged layout holds only directories and real files (hardlinks are
        // materialized as real bytes, never symlinks). Skip anything else so a
        // stray socket or symlink cannot leak into the deterministic archive.
        if metadata.file_type().is_symlink() {
            continue;
        }
        let rel = relative_slash_path(root, &path)?;
        if metadata.is_dir() {
            out.push(Entry {
                rel,
                absolute: path.clone(),
                is_dir: true,
                executable: false,
            });
            collect_into(root, &path, out)?;
        } else if metadata.is_file() {
            out.push(Entry {
                rel,
                absolute: path,
                is_dir: false,
                executable: is_executable(&metadata),
            });
        }
    }
    Ok(())
}

/// Build the gzipped tar bytes for `entries` with fully normalized headers, so
/// identical staged contents yield byte-identical output.
fn archive_bytes(root: &Path, entries: &[Entry]) -> Result<Vec<u8>> {
    let _ = root;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        for entry in entries {
            let mode = normalized_mode(entry);
            let mut header = Header::new_gnu();
            header.set_mtime(FIXED_MTIME);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mode(mode);
            header
                .set_username("")
                .context("failed to clear archive username")?;
            header
                .set_groupname("")
                .context("failed to clear archive groupname")?;
            if entry.is_dir {
                header.set_entry_type(EntryType::Directory);
                header.set_size(0);
                let dir_path = format!("{}/", entry.rel);
                builder
                    .append_data(&mut header, &dir_path, io::empty())
                    .with_context(|| format!("failed to archive directory {}", entry.rel))?;
            } else {
                let data = fs::read(&entry.absolute)
                    .with_context(|| format!("failed to read {}", entry.absolute.display()))?;
                header.set_entry_type(EntryType::Regular);
                header.set_size(data.len() as u64);
                builder
                    .append_data(&mut header, &entry.rel, data.as_slice())
                    .with_context(|| format!("failed to archive {}", entry.rel))?;
            }
        }
        builder.finish().context("failed to finalize tar stream")?;
    }
    encoder.finish().context("failed to finalize gzip stream")
}

/// The executable-preserving mode for an entry: `0o755` for directories and
/// executables, `0o644` otherwise.
fn normalized_mode(entry: &Entry) -> u32 {
    if entry.is_dir || entry.executable {
        0o755
    } else {
        0o644
    }
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

/// Extract a `build.phoxal` at `archive` into `dest`.
///
/// Plain `tar -xzf` extracts the same bytes. Two properties of the archive
/// format are checked while reading, because an archive that lacks them is not
/// one this writer produced: every entry path is relative with no `..` or root
/// (`safe_relative`), and every entry is a directory or a regular file - the
/// writer emits nothing else, so links and special files are refused.
pub fn extract_build_archive(archive: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)
        .with_context(|| format!("failed to create extraction directory {}", dest.display()))?;
    let file =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    let mut tar = Archive::new(GzDecoder::new(file));
    for entry in tar
        .entries()
        .with_context(|| format!("failed to read {}", archive.display()))?
    {
        let mut entry = entry.context("failed to read archive entry")?;
        let path = entry
            .path()
            .context("archive entry had a non-UTF-8 path")?
            .into_owned();
        let safe = safe_relative(&path).ok_or_else(|| {
            anyhow::anyhow!(
                "refusing to extract archive entry `{}`: it escapes the destination",
                path.display()
            )
        })?;
        let entry_type = entry.header().entry_type();
        let out = dest.join(&safe);
        if entry_type.is_dir() {
            fs::create_dir_all(&out)
                .with_context(|| format!("failed to create {}", out.display()))?;
            continue;
        }
        anyhow::ensure!(
            entry_type.is_file(),
            "refusing archive entry `{}`: symlinks, hardlinks, and special files are not allowed",
            safe.display()
        );
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let size = entry.size();
        let mode = entry.header().mode().ok();
        write_stream_with_mode(&out, &mut entry, size, mode)?;
    }
    Ok(())
}

/// Write one entry's bytes to `out` with the header's mode.
///
/// The writer normalizes every mode to `0o644`/`0o755` before archiving, so the
/// header mode is already the executable-or-data answer this restores.
fn write_stream_with_mode(
    out: &Path,
    input: &mut impl Read,
    expected: u64,
    mode: Option<u32>,
) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options
        .open(out)
        .with_context(|| format!("failed to create {}", out.display()))?;
    let copied = io::copy(input, &mut file)
        .with_context(|| format!("failed to extract {}", out.display()))?;
    anyhow::ensure!(
        copied == expected,
        "archive entry {} declared {expected} bytes but produced {copied}",
        out.display()
    );
    file.sync_all()?;
    Ok(())
}

/// The slash-separated path of `path` relative to `root`, rejecting any `..`
/// component so the archived path can never point outside the layout.
fn relative_slash_path(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("{} is not under {}", path.display(), root.display()))?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .with_context(|| format!("non-UTF-8 path component in {}", path.display()))?
                    .to_string(),
            ),
            Component::CurDir => {}
            other => bail!(
                "unexpected path component {other:?} while archiving {}",
                path.display()
            ),
        }
    }
    Ok(parts.join("/"))
}

/// Validate a tar entry path for extraction: return the normalized relative
/// path when it is safe (relative, no `..`), or `None` when it would escape.
fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            // Absolute roots, prefixes, and parent-dir traversal all escape.
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    if safe.as_os_str().is_empty() {
        None
    } else {
        Some(safe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::io::Write;

    #[cfg(unix)]
    fn write_executable(path: &Path, data: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, data).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    /// Stage a minimal layout: `robot.yaml`, an executable `bin/mission`, and a
    /// non-executable asset. Returns the layout root.
    fn stage_layout(root: &Path) {
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(
            root.join("robot.yaml"),
            b"schema: phoxal/robot/v0\nclock: real\n",
        )
        .unwrap();
        write_executable(&root.join("bin/mission"), b"\x7fELF-ish-binary");
        fs::write(root.join("assets/fixture.bin"), b"asset").unwrap();
    }

    #[test]
    fn identical_contents_produce_identical_archive_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("build/triple");
        stage_layout(&layout);

        let out_a = dir.path().join("a.build.phoxal");
        let digest_a = write_build_archive(&layout, &out_a).unwrap();

        // Touch every file's mtime to a different time; determinism must ignore
        // it. Re-archive to a second path.
        let later = std::time::SystemTime::now();
        filetime_touch(&layout.join("robot.yaml"), later);
        filetime_touch(&layout.join("bin/mission"), later);
        filetime_touch(&layout.join("assets/fixture.bin"), later);

        let out_b = dir.path().join("b.build.phoxal");
        let digest_b = write_build_archive(&layout, &out_b).unwrap();

        assert_eq!(
            digest_a, digest_b,
            "mtime changes must not alter the digest"
        );
        assert_eq!(
            fs::read(&out_a).unwrap(),
            fs::read(&out_b).unwrap(),
            "archive bytes must be identical"
        );
    }

    /// Re-staging identical content into a fresh directory (as a rebuild/relink
    /// would) still produces the same archive bytes.
    #[test]
    fn restaged_identical_content_matches_digest() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first/triple");
        let second = dir.path().join("second/triple");
        stage_layout(&first);
        stage_layout(&second);

        let out_first = dir.path().join("first.build.phoxal");
        let out_second = dir.path().join("second.build.phoxal");
        let digest_first = write_build_archive(&first, &out_first).unwrap();
        let digest_second = write_build_archive(&second, &out_second).unwrap();
        assert_eq!(digest_first, digest_second);
    }

    #[test]
    fn archive_contains_exactly_the_layout_and_extracts_safely() {
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("build/triple");
        stage_layout(&layout);
        // A runtime-state directory the archive must never capture.
        fs::create_dir_all(layout.join(".phoxal/run")).unwrap();
        fs::write(layout.join(".phoxal/run/build.lock"), b"lock").unwrap();

        let out = dir.path().join("bundle.build.phoxal");
        write_build_archive(&layout, &out).unwrap();

        let extracted = dir.path().join("extracted");
        extract_build_archive(&out, &extracted).unwrap();

        let mut names = BTreeSet::new();
        collect_names(&extracted, &extracted, &mut names);
        assert!(names.contains("robot.yaml"));
        assert!(names.contains("bin/mission"));
        assert!(names.contains("assets/fixture.bin"));
        // The archive is pure runtime content: a top-level `.phoxal` (lock/socket
        // runtime state a prior layout run may have left behind) is never folded
        // into the bundle.
        assert!(
            !names.iter().any(|name| name.starts_with(".phoxal")),
            "archive must not contain .phoxal runtime state: {names:?}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(extracted.join("bin/mission"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755, "executable bit must be preserved");
            let asset = fs::metadata(extracted.join("assets/fixture.bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(asset, 0o644, "non-executable files normalize to 0644");
        }
    }

    /// Hand-build a single-entry ustar archive whose name field is `name`,
    /// bypassing the `tar` crate's write-side `..` guard, so the extractor's own
    /// escape guard is what's under test.
    fn raw_tar_gz(name: &str, data: &[u8]) -> Vec<u8> {
        raw_tar_gz_entries(&[(name, data, b'0')])
    }

    fn raw_tar_gz_entries(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (name, data, typeflag) in entries {
            let mut header = [0u8; 512];
            let name_bytes = name.as_bytes();
            header[..name_bytes.len()].copy_from_slice(name_bytes);
            header[100..108].copy_from_slice(b"0000644\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            let size = format!("{:011o}\0", data.len());
            header[124..136].copy_from_slice(size.as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[156] = *typeflag;
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[148..156].copy_from_slice(b"        ");
            let sum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
            let cksum = format!("{sum:06o}\0 ");
            header[148..156].copy_from_slice(cksum.as_bytes());
            tar.extend_from_slice(&header);
            tar.extend_from_slice(data);
            let pad = (512 - data.len() % 512) % 512;
            tar.extend(std::iter::repeat_n(0u8, pad));
        }
        tar.extend(std::iter::repeat_n(0u8, 1024));

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    /// The two archive-format properties extraction reads back: entry paths are
    /// relative, and every entry is a file or a directory. The writer produces
    /// nothing else, so an archive that breaks either is malformed.
    #[test]
    fn malformed_entries_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("relative.tar.gz");
        fs::write(&archive, raw_tar_gz("../escape", b"outside")).unwrap();
        let error = extract_build_archive(&archive, &dir.path().join("relative-dest"))
            .expect_err("a non-relative entry must be refused");
        assert!(
            error.to_string().contains("escapes the destination"),
            "{error}"
        );
        assert!(!dir.path().join("escape").exists());

        let link = dir.path().join("link.tar.gz");
        fs::write(&link, raw_tar_gz_entries(&[("bin/link", b"", b'2')])).unwrap();
        let error = extract_build_archive(&link, &dir.path().join("link-dest"))
            .expect_err("a link entry must be refused");
        assert!(
            error.to_string().contains("symlinks") || error.to_string().contains("special files"),
            "{error}"
        );
    }

    #[test]
    fn refuses_output_inside_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("build/triple");
        stage_layout(&layout);
        let inside = layout.join("bundle.build.phoxal");
        let error = write_build_archive(&layout, &inside)
            .expect_err("an output inside the layout must be refused");
        assert!(
            error.to_string().contains("inside the staged layout"),
            "{error}"
        );
    }

    fn filetime_touch(path: &Path, time: std::time::SystemTime) {
        // Best-effort mtime bump via a rewrite-preserving open; portable enough
        // for the determinism assertion without pulling in a filetime crate.
        let data = fs::read(path).unwrap();
        fs::write(path, &data).unwrap();
        let _ = time;
    }

    fn collect_names(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = relative_slash_path(root, &path).unwrap();
            if path.is_dir() {
                out.insert(rel);
                collect_names(root, &path, out);
            } else {
                out.insert(rel);
            }
        }
    }

    /// The archive is the bundle: `robot.yaml`, `assets/`, and `bin/`, and
    /// nothing else. The cargo-install bookkeeping that `cargo install --root`
    /// leaves beside `bin/` embeds absolute host paths, so a bundle carrying it
    /// would not be reproducible across machines - and nothing at runtime reads
    /// it. This pins the exact file set rather than asserting an absence, so a
    /// future addition has to be made deliberately.
    #[test]
    fn the_archive_contains_the_exact_bundle_file_set_and_no_host_residue() {
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("bundle");
        stage_layout(&layout);
        // Exactly what a real `cargo install --root <candidate>` leaves behind,
        // plus the local runtime state an ordinary extracted bundle may grow.
        fs::write(layout.join(".crates.toml"), b"[v1]\n").unwrap();
        fs::write(layout.join(".crates2.json"), b"{}").unwrap();
        fs::create_dir_all(layout.join(".phoxal/run")).unwrap();
        fs::write(layout.join(".phoxal/run/supervisor.log"), b"stale").unwrap();

        let archive = dir.path().join("build.phoxal");
        write_build_archive(&layout, &archive).unwrap();
        let extracted = dir.path().join("extracted");
        extract_build_archive(&archive, &extracted).unwrap();

        let mut files = BTreeSet::new();
        fn walk(root: &Path, dir: &Path, files: &mut BTreeSet<String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, files);
                } else {
                    files.insert(relative_slash_path(root, &path).unwrap());
                }
            }
        }
        walk(&extracted, &extracted, &mut files);
        assert_eq!(
            files,
            BTreeSet::from([
                "assets/fixture.bin".to_string(),
                "bin/mission".to_string(),
                "robot.yaml".to_string(),
            ])
        );
    }
}
