//! Deterministic `build.phoxal` archiving of a staged runtime layout (#936).
//!
//! `build.phoxal` is a gzipped tar of a staged runtime layout - the compiled
//! `robot.yaml`, the flat `bin/` store, and runtime assets - written so that
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
//! The archive is not executable. Its `phoxal.runtime.json` compatibility
//! declaration is ordinary deterministic runtime content, next to the compiled
//! manifest and binaries. Plain
//! `tar -xzf build.phoxal` extracts it identically to [`extract_build_archive`];
//! the helper here only adds the path-escape guard.

use std::collections::HashSet;
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
/// `phoxal build --target <TRIPLE>` is `<project>/.phoxal/build/<triple>.build.phoxal`.
pub const BUILD_ARCHIVE_EXTENSION: &str = "build.phoxal";

/// A fixed timestamp stamped on every archived entry so mtime never varies the
/// output. Value is arbitrary but constant (2020-01-01T00:00:00Z); using a
/// non-zero epoch keeps extracted files from tripping tools that treat mtime 0
/// as "missing".
const FIXED_MTIME: u64 = 1_577_836_800;
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 16_384;

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

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

/// Extract a `build.phoxal` at `archive` into `dest`, rejecting any entry whose
/// path would escape `dest`. Plain `tar -xzf` extracts the same bytes; this adds
/// two layered escape guards so a maliciously crafted bundle - or a malicious
/// pre-existing symlink in `dest` - cannot write outside the destination:
///
/// 1. **Lexical:** every entry path is validated relative with no `..`/root
///    (`safe_relative`).
/// 2. **Physical:** `dest` must be a newly created or already empty real
///    directory (never a symlink), and every path component is created and
///    checked with `symlink_metadata` during extraction - a symlinked ancestor
///    or a symlinked final target is rejected, and the final file is opened with
///    `O_NOFOLLOW` so a race that swaps in a symlink still cannot be followed.
///
/// The lexical guard alone is insufficient: `create_dir_all`/`open` follow
/// symlink ancestors, so a `dest/foo` symlink pointing outside would let entry
/// `foo/bar` write outside despite a clean lexical path (#936, finding E).
pub fn extract_build_archive(archive: &Path, dest: &Path) -> Result<()> {
    let archive_len = fs::metadata(archive)
        .with_context(|| format!("failed to inspect {}", archive.display()))?
        .len();
    anyhow::ensure!(
        archive_len <= MAX_ARCHIVE_BYTES,
        "build archive is {archive_len} bytes; limit is {MAX_ARCHIVE_BYTES}"
    );
    ensure_fresh_destination(dest)?;
    let file =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    let mut tar = Archive::new(GzDecoder::new(file));
    let mut seen = HashSet::new();
    let mut extracted_bytes = 0_u64;
    for (index, entry) in tar
        .entries()
        .with_context(|| format!("failed to read {}", archive.display()))?
        .enumerate()
    {
        anyhow::ensure!(
            index < MAX_ENTRIES,
            "build archive contains more than {MAX_ENTRIES} entries"
        );
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
        anyhow::ensure!(
            seen.insert(safe.clone()),
            "build archive contains duplicate entry `{}`",
            safe.display()
        );
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            create_dirs_no_symlink(dest, &safe)?;
            continue;
        }
        anyhow::ensure!(
            entry_type.is_file(),
            "refusing archive entry `{}`: symlinks, hardlinks, and special files are not allowed",
            safe.display()
        );
        let size = entry.size();
        anyhow::ensure!(
            size <= MAX_ENTRY_BYTES,
            "archive entry `{}` is {size} bytes; per-file limit is {MAX_ENTRY_BYTES}",
            safe.display()
        );
        extracted_bytes = extracted_bytes
            .checked_add(size)
            .context("archive extracted size overflow")?;
        anyhow::ensure!(
            extracted_bytes <= MAX_EXTRACTED_BYTES,
            "archive expands beyond the {MAX_EXTRACTED_BYTES}-byte limit"
        );
        let out = if let Some(parent) = safe.parent() {
            create_dirs_no_symlink(dest, parent)?.join(
                safe.file_name()
                    .context("archive file entry has no file name")?,
            )
        } else {
            dest.join(&safe)
        };
        // Belt-and-suspenders: the final path must not already be a symlink
        // (`dest` is empty and we create no symlinks, but a concurrent writer
        // could race one in). The `O_NOFOLLOW` open below is the authoritative
        // guard; this gives a clearer diagnostic.
        if let Ok(meta) = fs::symlink_metadata(&out)
            && meta.file_type().is_symlink()
        {
            bail!(
                "refusing to extract archive entry `{}`: {} is a symlink in the destination",
                safe.display(),
                out.display()
            );
        }
        let mode = entry.header().mode().ok();
        write_stream_with_mode(&out, &mut entry, size, mode)?;
    }
    Ok(())
}

fn write_stream_with_mode(
    out: &Path,
    input: &mut impl Read,
    expected: u64,
    mode: Option<u32>,
) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW);
        if let Some(mode) = mode {
            // Archives select only executable versus data. Never reproduce
            // setuid/setgid/sticky or group/world-writable bits from an
            // untrusted tar header into a root-owned installed release.
            options.mode(if mode & 0o111 == 0 { 0o644 } else { 0o755 });
        }
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
    #[cfg(not(unix))]
    if let Some(_mode) = mode {}
    file.sync_all()?;
    Ok(())
}

/// Require `dest` to be a fresh extraction root: a newly created directory, or an
/// existing real directory that is empty. A symlink, a non-directory, or a
/// non-empty directory is rejected - so extraction never inherits a pre-existing
/// symlink an attacker planted inside `dest` (#936, finding E).
fn ensure_fresh_destination(dest: &Path) -> Result<()> {
    match fs::symlink_metadata(dest) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "refusing to extract into {}: it is a symlink; extract into a new directory",
                    dest.display()
                );
            }
            if !metadata.is_dir() {
                bail!(
                    "refusing to extract into {}: it is not a directory; extract into a new, empty \
                     directory",
                    dest.display()
                );
            }
            if fs::read_dir(dest)
                .with_context(|| format!("failed to read {}", dest.display()))?
                .next()
                .is_some()
            {
                bail!(
                    "refusing to extract into {}: it is not empty; extract into a new, empty \
                     directory",
                    dest.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(dest)
            .with_context(|| format!("failed to create extraction directory {}", dest.display())),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", dest.display())),
    }
}

/// Create every component of `relative` under `dest`, creating each real
/// directory and rejecting any component that already exists as a symlink or a
/// non-directory. Returns the resolved directory. This never follows a symlink,
/// so a crafted archive cannot tunnel through one to write outside `dest`.
fn create_dirs_no_symlink(dest: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = dest.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                current.push(part);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            bail!(
                                "refusing to extract through {}: it is a symlink or a file in the \
                                 destination path, not a real directory",
                                current.display()
                            );
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(&current)
                            .with_context(|| format!("failed to create {}", current.display()))?
                    }
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("failed to inspect {}", current.display()));
                    }
                }
            }
            other => bail!("unexpected path component {other:?} while extracting"),
        }
    }
    Ok(current)
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
        fs::create_dir_all(root.join("model")).unwrap();
        fs::write(root.join("robot.yaml"), b"schema: robot/v0\n").unwrap();
        #[cfg(unix)]
        write_executable(&root.join("bin/mission"), b"\x7fELF-ish-binary");
        #[cfg(not(unix))]
        fs::write(root.join("bin/mission"), b"\x7fELF-ish-binary").unwrap();
        fs::write(root.join("model/robot.urdf"), b"<robot/>").unwrap();
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
        filetime_touch(&layout.join("model/robot.urdf"), later);

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
        fs::create_dir_all(layout.join(".phoxal")).unwrap();
        fs::write(layout.join(".phoxal/project.lock"), b"lock").unwrap();

        let out = dir.path().join("bundle.build.phoxal");
        write_build_archive(&layout, &out).unwrap();

        let extracted = dir.path().join("extracted");
        extract_build_archive(&out, &extracted).unwrap();

        let mut names = BTreeSet::new();
        collect_names(&extracted, &extracted, &mut names);
        assert!(names.contains("robot.yaml"));
        assert!(names.contains("bin/mission"));
        assert!(names.contains("model/robot.urdf"));
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
            let asset = fs::metadata(extracted.join("model/robot.urdf"))
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

    #[test]
    fn a_crafted_escaping_entry_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("evil.tar.gz");
        fs::write(&archive, raw_tar_gz("../escape", b"pwned")).unwrap();

        let dest = dir.path().join("dest");
        let error =
            extract_build_archive(&archive, &dest).expect_err("an escaping entry must be rejected");
        assert!(
            error.to_string().contains("escapes the destination"),
            "{error}"
        );
        assert!(
            !dir.path().join("escape").exists(),
            "the escaping file must not be written outside the destination"
        );
    }

    #[test]
    fn duplicate_entries_and_links_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let duplicate = dir.path().join("duplicate.tar.gz");
        fs::write(
            &duplicate,
            raw_tar_gz_entries(&[
                ("robot.yaml", b"first", b'0'),
                ("robot.yaml", b"second", b'0'),
            ]),
        )
        .unwrap();
        let error = extract_build_archive(&duplicate, &dir.path().join("duplicate-dest"))
            .expect_err("duplicate paths must be rejected");
        assert!(error.to_string().contains("duplicate"), "{error}");

        let link = dir.path().join("link.tar.gz");
        fs::write(&link, raw_tar_gz_entries(&[("bin/escape", b"", b'2')])).unwrap();
        let error = extract_build_archive(&link, &dir.path().join("link-dest"))
            .expect_err("links must be rejected");
        assert!(
            error.to_string().contains("symlinks") || error.to_string().contains("special files"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extracted_modes_strip_privilege_and_writable_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("runtime");
        write_stream_with_mode(&executable, &mut b"binary".as_slice(), 6, Some(0o6777)).unwrap();
        assert_eq!(
            fs::metadata(executable).unwrap().permissions().mode() & 0o7777,
            0o755
        );
    }

    /// Finding E: a pre-existing symlinked ancestor in the destination cannot be
    /// tunneled through - `create_dir_all`/`open` would have followed it, writing
    /// outside `dest`. The empty-destination requirement plus per-component
    /// symlink checks reject it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_in_the_destination_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("build/triple");
        stage_layout(&layout);
        let out = dir.path().join("bundle.build.phoxal");
        write_build_archive(&layout, &out).unwrap();

        // The archive has a `model/` directory entry. Plant `dest/model` as a
        // symlink pointing outside `dest` before extraction. Because `dest` must
        // be empty, this is only reachable by racing it in - but the empty-dir
        // requirement itself already rejects a non-empty dest, and the
        // per-component check rejects the symlink. Prove both: a dest whose
        // `model` is a symlink is non-empty AND symlinked.
        let escape = dir.path().join("escape");
        fs::create_dir_all(&escape).unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        std::os::unix::fs::symlink(&escape, dest.join("model")).unwrap();

        let error = extract_build_archive(&out, &dest)
            .expect_err("a symlinked parent in the destination must be rejected");
        assert!(
            error.to_string().contains("not empty") || error.to_string().contains("symlink"),
            "{error}"
        );
        // Nothing was written through the symlink into the outside directory.
        assert!(!escape.join("robot.urdf").exists());
    }

    /// Finding E: a symlinked final target is not followed - the file is not
    /// written through it to the outside.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_final_target_is_rejected() {
        // A single-entry archive writing `robot.yaml`.
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("one.tar.gz");
        fs::write(&archive, raw_tar_gz("robot.yaml", b"schema: robot/v0\n")).unwrap();

        // Extract once into a fresh dest to establish a normal baseline.
        let good = dir.path().join("good");
        extract_build_archive(&archive, &good).unwrap();
        assert!(good.join("robot.yaml").is_file());

        // Now a dest that is non-empty because `robot.yaml` is a symlink to an
        // outside file: extraction refuses the non-empty/symlinked destination
        // and never writes through the link.
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, b"original").unwrap();
        let dest = dir.path().join("evil");
        fs::create_dir_all(&dest).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("robot.yaml")).unwrap();

        let error = extract_build_archive(&archive, &dest)
            .expect_err("a symlinked final target must be rejected");
        assert!(
            error.to_string().contains("not empty") || error.to_string().contains("symlink"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "original");
    }

    /// Finding E: a non-empty destination is rejected - extraction requires a
    /// fresh (new or empty) directory so it never inherits planted state.
    #[test]
    fn a_non_empty_destination_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("build/triple");
        stage_layout(&layout);
        let out = dir.path().join("bundle.build.phoxal");
        write_build_archive(&layout, &out).unwrap();

        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("pre-existing"), b"stuff").unwrap();

        let error = extract_build_archive(&out, &dest)
            .expect_err("a non-empty destination must be rejected");
        assert!(error.to_string().contains("not empty"), "{error}");

        // A fresh, never-created destination extracts fine.
        let fresh = dir.path().join("fresh");
        extract_build_archive(&out, &fresh).unwrap();
        assert!(fresh.join("robot.yaml").is_file());
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
}
