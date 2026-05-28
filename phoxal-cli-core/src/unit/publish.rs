use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::AppContext;
use crate::unit::Unit;
use crate::unit::container::{BuildPlatform, ManagedImagePlan, ManagedImageSource, TopologyPlan};
use crate::unit::doctor::{
    cargo_target_linker_env_name, configured_or_default_target_linker,
    validate_rust_build_toolchain,
};

#[derive(Debug, Clone)]
pub struct BuildImages {
    pub topology: TopologyPlan,
    pub extra_images: Vec<ManagedImagePlan>,
    pub platform: BuildPlatform,
    pub bundle_dir: PathBuf,
    pub no_cache: bool,
}

#[derive(Debug, Clone)]
pub struct PushImages {
    pub topology: TopologyPlan,
    pub extra_images: Vec<ManagedImagePlan>,
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

fn hash_inputs<'a>(inputs: impl IntoIterator<Item = &'a str>) -> String {
    let mut hasher = Sha256::new();
    for input in inputs {
        hasher.update(input.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn hash_fingerprint_path(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat fingerprint input {}", path.display()))?;
    if metadata.is_dir() {
        hash_directory(path)
    } else {
        hash_file(path)
    }
}

fn hash_directory(path: &Path) -> Result<String> {
    fn stable_relative_path(path: &Path) -> String {
        path.components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    fn collect_files(
        root: &Path,
        directory: &Path,
        files: &mut Vec<(String, PathBuf)>,
    ) -> Result<()> {
        for entry in fs::read_dir(directory).with_context(|| {
            format!(
                "failed to read fingerprint directory {}",
                directory.display()
            )
        })? {
            let entry = entry.with_context(|| {
                format!(
                    "failed to read fingerprint directory entry in {}",
                    directory.display()
                )
            })?;
            let entry_path = entry.path();
            let file_type = entry.file_type().with_context(|| {
                format!(
                    "failed to inspect fingerprint directory entry {}",
                    entry_path.display()
                )
            })?;

            if file_type.is_dir() {
                if entry.file_name() == "target" {
                    continue;
                }
                collect_files(root, &entry_path, files)?;
            } else if file_type.is_file() {
                let relative_path = entry_path.strip_prefix(root).with_context(|| {
                    format!(
                        "failed to make fingerprint path {} relative to {}",
                        entry_path.display(),
                        root.display()
                    )
                })?;
                files.push((stable_relative_path(relative_path), entry_path));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    collect_files(path, path, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    for (relative_path, absolute_path) in files {
        let file_hash = hash_file(&absolute_path).with_context(|| {
            format!(
                "failed to hash Docker context fingerprint file {}",
                absolute_path.display()
            )
        })?;
        hasher.update(relative_path.as_bytes());
        hasher.update([0]);
        hasher.update(file_hash.as_bytes());
        hasher.update([0]);
    }

    Ok(hex::encode(hasher.finalize()))
}

fn hash_fingerprint_paths(paths: &[PathBuf]) -> Result<String> {
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        entries.push(hash_fingerprint_path(path).with_context(|| {
            format!(
                "failed to hash Docker context fingerprint input {}",
                path.display()
            )
        })?);
    }
    Ok(hash_inputs(entries.iter().map(String::as_str)))
}

fn image_fingerprint<'a>(inputs: impl IntoIterator<Item = &'a str>) -> String {
    hash_inputs(inputs)
}

enum LocalImageFingerprint {
    Present(String),
    MissingLabel,
    MissingImage,
}

fn is_missing_docker_image_error(stderr: &str) -> bool {
    stderr.contains("No such object") || stderr.contains("No such image")
}

fn inspect_local_image_fingerprint(image_name: &str) -> Result<LocalImageFingerprint> {
    let mut command = Command::new("docker");
    command
        .arg("image")
        .arg("inspect")
        .arg("--format")
        .arg("{{ index .Config.Labels \"org.robot_framework.xtask.fingerprint\" }}")
        .arg(image_name);

    let output = command
        .output()
        .with_context(|| format!("failed to execute docker inspect for {image_name}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_missing_docker_image_error(&stderr) {
            return Ok(LocalImageFingerprint::MissingImage);
        }
        bail!(
            "docker image inspect failed for {} with status {}: {}",
            image_name,
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8(output.stdout)
        .context("docker inspect output is not valid utf-8")?
        .trim()
        .to_string();

    if stdout.is_empty() || stdout == "<no value>" {
        Ok(LocalImageFingerprint::MissingLabel)
    } else {
        Ok(LocalImageFingerprint::Present(stdout))
    }
}

impl Unit for BuildImages {
    type Output = ();

    fn name(&self) -> &'static str {
        "Build Images"
    }

    fn execute(&self, app: &AppContext) -> Result<Self::Output> {
        let managed_images = self.managed_images();

        let packages = managed_images
            .iter()
            .filter_map(|image| match &image.source {
                ManagedImageSource::RustBinary { package_name, .. } => Some(package_name.clone()),
                ManagedImageSource::DockerContext { .. } => None,
            })
            .collect::<BTreeSet<_>>();

        if !packages.is_empty() {
            validate_rust_build_toolchain(&self.platform.target_triple)?;
            let mut command = Command::new("cargo");
            command
                .current_dir(app.project.root())
                .arg("build")
                .arg("--release");
            if let Some(linker) = configured_or_default_target_linker(&self.platform.target_triple)
            {
                command.env(
                    cargo_target_linker_env_name(&self.platform.target_triple),
                    linker,
                );
            }
            command.arg("--target").arg(&self.platform.target_triple);
            for package in packages {
                command.arg("-p").arg(package);
            }

            let title = "Building Rust binaries for container images";
            app.ui.step(title, || {
                let status = app.ui.command_status(&mut command)?;
                if !status.success() {
                    bail!("cargo build failed with status {status}");
                }
                Ok(())
            })?;
        }

        for image in &managed_images {
            let current_fingerprint = match &image.source {
                ManagedImageSource::RustBinary { binary_name, .. } => {
                    let binary_source = app
                        .project
                        .root()
                        .join("target")
                        .join(&self.platform.target_triple)
                        .join("release")
                        .join(binary_name);
                    if !binary_source.is_file() {
                        bail!("expected built binary at {}", binary_source.display());
                    }

                    let binary_hash = hash_file(&binary_source).with_context(|| {
                        format!("failed to hash built binary {}", binary_source.display())
                    })?;
                    let dockerfile_hash = hash_file(&image.dockerfile_path).with_context(|| {
                        format!(
                            "failed to hash Dockerfile {}",
                            image.dockerfile_path.display()
                        )
                    })?;

                    image_fingerprint([
                        binary_hash.as_str(),
                        dockerfile_hash.as_str(),
                        self.platform.target_triple.as_str(),
                        self.platform.docker_platform.as_str(),
                    ])
                }
                ManagedImageSource::DockerContext {
                    fingerprint_paths, ..
                } => {
                    let dockerfile_hash = hash_file(&image.dockerfile_path).with_context(|| {
                        format!(
                            "failed to hash Dockerfile {}",
                            image.dockerfile_path.display()
                        )
                    })?;
                    let context_hash = hash_fingerprint_paths(fingerprint_paths)?;

                    image_fingerprint([
                        dockerfile_hash.as_str(),
                        context_hash.as_str(),
                        self.platform.docker_platform.as_str(),
                    ])
                }
            };

            if !self.no_cache {
                match inspect_local_image_fingerprint(&image.image)? {
                    LocalImageFingerprint::Present(f) if f == current_fingerprint => {
                        app.ui.info(format!(
                            "Skipping image {} (local image exists and fingerprint matched)",
                            image.image
                        ));
                        continue;
                    }
                    LocalImageFingerprint::Present(f) => {
                        app.ui.info(format!(
                            "Rebuilding image {} (fingerprint mismatch: local={}, expected={})",
                            image.image, f, current_fingerprint
                        ));
                    }
                    LocalImageFingerprint::MissingLabel => {
                        app.ui
                            .info(format!("Rebuilding image {} (missing label)", image.image));
                    }
                    LocalImageFingerprint::MissingImage => {
                        app.ui.info(format!(
                            "Rebuilding image {} (missing local image)",
                            image.image
                        ));
                    }
                }
            }

            match &image.source {
                ManagedImageSource::RustBinary { binary_name, .. } => {
                    let binary_source = app
                        .project
                        .root()
                        .join("target")
                        .join(&self.platform.target_triple)
                        .join("release")
                        .join(binary_name);
                    if !binary_source.is_file() {
                        bail!("expected built binary at {}", binary_source.display());
                    }
                    let (build_context, binary_source_arg) =
                        rust_binary_image_build_source(&binary_source)?;

                    let mut build = Command::new("docker");
                    build
                        .current_dir(app.project.root())
                        .arg("build")
                        .arg("--platform")
                        .arg(&self.platform.docker_platform)
                        .arg("-f")
                        .arg(&image.dockerfile_path)
                        .arg("-t")
                        .arg(&image.image);
                    if self.no_cache {
                        build.arg("--no-cache");
                    }
                    build
                        .arg("--label")
                        .arg(format!(
                            "org.robot_framework.xtask.fingerprint={}",
                            current_fingerprint
                        ))
                        .arg("--build-arg")
                        .arg(format!("BINARY_NAME={binary_name}"))
                        .arg("--build-arg")
                        .arg(format!("BINARY_SOURCE={}", binary_source_arg.display()))
                        .arg(build_context);

                    let title = format!("Building image {}", image.image);
                    app.ui.step(&title, || {
                        let status = app.ui.command_status(&mut build)?;
                        if !status.success() {
                            bail!(
                                "docker build failed for {} with status {status}",
                                image.image
                            );
                        }
                        Ok(())
                    })?;
                }
                ManagedImageSource::DockerContext { build_context, .. } => {
                    let mut build = Command::new("docker");
                    build
                        .current_dir(app.project.root())
                        .arg("build")
                        .arg("--platform")
                        .arg(&self.platform.docker_platform)
                        .arg("-f")
                        .arg(&image.dockerfile_path)
                        .arg("-t")
                        .arg(&image.image);
                    if self.no_cache {
                        build.arg("--no-cache");
                    }
                    build
                        .arg("--label")
                        .arg(format!(
                            "org.robot_framework.xtask.fingerprint={}",
                            current_fingerprint
                        ))
                        .arg(build_context);

                    let title = format!("Building image {}", image.image);
                    app.ui.step(&title, || {
                        let status = app.ui.command_status(&mut build)?;
                        if !status.success() {
                            bail!(
                                "docker build failed for {} with status {status}",
                                image.image
                            );
                        }
                        Ok(())
                    })?;
                }
            }
        }

        let mut bundle_build = Command::new("docker");
        bundle_build
            .arg("build")
            .arg("--platform")
            .arg(&self.platform.docker_platform)
            .arg("-f")
            .arg(&self.topology.bundle.dockerfile_path)
            .arg("-t")
            .arg(&self.topology.bundle.image);
        if self.no_cache {
            bundle_build.arg("--no-cache");
        }
        bundle_build.arg(
            self.bundle_dir
                .parent()
                .context("bundle directory must have a parent build context")?,
        );
        let bundle_title = format!("Building bundle image {}", self.topology.bundle.image);
        app.ui.step(&bundle_title, || {
            let status = app.ui.command_status(&mut bundle_build)?;
            if !status.success() {
                bail!(
                    "docker build failed for {} with status {status}",
                    self.topology.bundle.image
                );
            }
            Ok(())
        })?;

        Ok(())
    }
}

impl BuildImages {
    fn managed_images(&self) -> Vec<ManagedImagePlan> {
        self.topology
            .managed_images()
            .into_iter()
            .cloned()
            .chain(self.extra_images.clone())
            .chain(self.topology.firmware_managed_image())
            .collect()
    }
}

impl Unit for PushImages {
    type Output = ();

    fn name(&self) -> &'static str {
        "Push Images"
    }

    fn execute(&self, app: &AppContext) -> Result<Self::Output> {
        let mut images = self
            .topology
            .managed_images()
            .into_iter()
            .map(|image| image.image.clone())
            .chain(std::iter::once(self.topology.bundle.image.clone()))
            .chain(
                self.topology
                    .firmware
                    .iter()
                    .map(|image| image.image.clone()),
            )
            .chain(self.extra_images.iter().map(|image| image.image.clone()))
            .collect::<Vec<_>>();
        images.sort();
        images.dedup();

        for image in images {
            let mut push = Command::new("docker");
            push.arg("push").arg(&image);
            let title = format!("Pushing image {image}");
            app.ui.step(&title, || {
                let status = app.ui.command_status(&mut push)?;
                if !status.success() {
                    bail!("docker push failed for {image} with status {status}");
                }
                Ok(())
            })?;
        }

        Ok(())
    }
}

fn rust_binary_image_build_source(binary_source: &Path) -> Result<(PathBuf, PathBuf)> {
    let build_context = binary_source
        .parent()
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "built binary path {} must have a parent build context",
                binary_source.display()
            )
        })?;
    let binary_source_arg = binary_source
        .file_name()
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "built binary path {} must have a file name",
                binary_source.display()
            )
        })?;

    Ok((build_context, binary_source_arg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(name: &str) -> PathBuf {
        let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_nanos(),
            Err(error) => panic!("system clock must be after unix epoch: {error}"),
        };
        std::env::temp_dir().join(format!(
            "robot-framework-phoxal-cli-publish-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            panic!("test parent directory should be created: {error}");
        }
        if let Err(error) = fs::write(path, contents) {
            panic!("test file should be written: {error}");
        }
    }

    fn remove_test_dir(path: PathBuf) {
        if let Err(error) = fs::remove_dir_all(&path) {
            panic!(
                "test directory {} should be removed: {error}",
                path.display()
            );
        }
    }

    #[test]
    fn test_image_fingerprint_changes_with_binary_hash() {
        let f1 = image_fingerprint(["bin1", "docker1", "target1", "plat1"]);
        let f2 = image_fingerprint(["bin2", "docker1", "target1", "plat1"]);
        assert_ne!(f1, f2);
    }

    #[test]
    fn test_image_fingerprint_changes_with_dockerfile_hash() {
        let f1 = image_fingerprint(["bin1", "docker1", "target1", "plat1"]);
        let f2 = image_fingerprint(["bin1", "docker2", "target1", "plat1"]);
        assert_ne!(f1, f2);
    }

    #[test]
    fn test_image_fingerprint_changes_with_target_triple() {
        let f1 = image_fingerprint(["bin1", "docker1", "target1", "plat1"]);
        let f2 = image_fingerprint(["bin1", "docker1", "target2", "plat1"]);
        assert_ne!(f1, f2);
    }

    #[test]
    fn test_image_fingerprint_changes_with_docker_platform() {
        let f1 = image_fingerprint(["bin1", "docker1", "target1", "plat1"]);
        let f2 = image_fingerprint(["bin1", "docker1", "target1", "plat2"]);
        assert_ne!(f1, f2);
    }

    #[test]
    fn test_image_fingerprint_deterministic() {
        let f1 = image_fingerprint(["bin1", "docker1", "target1", "plat1"]);
        let f2 = image_fingerprint(["bin1", "docker1", "target1", "plat1"]);
        assert_eq!(f1, f2);
    }

    #[test]
    fn detects_missing_image_errors_from_docker() {
        assert!(is_missing_docker_image_error(
            "Error response from daemon: No such image: local/phoxal-runtime-asset:dev"
        ));
        assert!(is_missing_docker_image_error(
            "Error response from daemon: No such object: local/phoxal-runtime-asset:dev"
        ));
        assert!(!is_missing_docker_image_error(
            "permission denied while trying to connect to the Docker daemon socket"
        ));
    }

    #[test]
    fn rust_binary_image_build_source_uses_binary_directory_as_context() {
        let binary_source = Path::new("/workspace/target/aarch64-unknown-linux-gnu/release/app");
        let (build_context, binary_source_arg) = match rust_binary_image_build_source(binary_source)
        {
            Ok(source) => source,
            Err(error) => panic!("binary image build source should resolve: {error}"),
        };

        assert_eq!(
            build_context,
            PathBuf::from("/workspace/target/aarch64-unknown-linux-gnu/release")
        );
        assert_eq!(binary_source_arg, PathBuf::from("app"));
    }

    #[test]
    fn hash_fingerprint_paths_handles_directory() {
        let directory = temp_test_dir("handles-directory");
        write_file(
            &directory.join("src/lib.rs"),
            "pub fn value() -> u8 { 1 }\n",
        );
        write_file(
            &directory.join("orb/system.cc"),
            "int system() { return 1; }\n",
        );

        let hash = match hash_fingerprint_paths(std::slice::from_ref(&directory)) {
            Ok(hash) => hash,
            Err(error) => panic!("directory fingerprint should hash: {error}"),
        };

        assert!(!hash.is_empty());
        remove_test_dir(directory);
    }

    #[test]
    fn directory_hash_changes_when_a_file_changes() {
        let directory = temp_test_dir("file-change");
        let source = directory.join("src/lib.rs");
        write_file(&source, "pub fn value() -> u8 { 1 }\n");
        write_file(&directory.join("src/main.rs"), "fn main() {}\n");

        let before = match hash_fingerprint_paths(std::slice::from_ref(&directory)) {
            Ok(hash) => hash,
            Err(error) => panic!("directory fingerprint should hash: {error}"),
        };
        write_file(&source, "pub fn value() -> u8 { 2 }\n");
        let after = match hash_fingerprint_paths(std::slice::from_ref(&directory)) {
            Ok(hash) => hash,
            Err(error) => panic!("directory fingerprint should hash after file change: {error}"),
        };

        assert_ne!(before, after);
        remove_test_dir(directory);
    }

    #[test]
    fn directory_hash_is_order_independent() {
        let first = temp_test_dir("order-first");
        write_file(&first.join("src/lib.rs"), "pub fn lib() {}\n");
        write_file(&first.join("build.rs"), "fn main() {}\n");

        let second = temp_test_dir("order-second");
        write_file(&second.join("build.rs"), "fn main() {}\n");
        write_file(&second.join("src/lib.rs"), "pub fn lib() {}\n");

        let first_hash = match hash_directory(&first) {
            Ok(hash) => hash,
            Err(error) => panic!("first directory should hash: {error}"),
        };
        let second_hash = match hash_directory(&second) {
            Ok(hash) => hash,
            Err(error) => panic!("second directory should hash: {error}"),
        };

        assert_eq!(first_hash, second_hash);
        remove_test_dir(first);
        remove_test_dir(second);
    }

    #[test]
    fn hash_fingerprint_paths_still_handles_file() {
        let directory = temp_test_dir("file-path");
        let source = directory.join("Dockerfile");
        write_file(&source, "FROM scratch\n");

        let hash = match hash_fingerprint_paths(&[source]) {
            Ok(hash) => hash,
            Err(error) => panic!("single file fingerprint should still hash: {error}"),
        };

        assert!(!hash.is_empty());
        remove_test_dir(directory);
    }

    #[test]
    fn directory_hash_skips_target_dir() {
        let directory = temp_test_dir("skips-target");
        write_file(
            &directory.join("src/lib.rs"),
            "pub fn value() -> u8 { 1 }\n",
        );

        let before = match hash_directory(&directory) {
            Ok(hash) => hash,
            Err(error) => panic!("directory should hash: {error}"),
        };
        write_file(
            &directory.join("target/debug/artifact"),
            "ignored build output\n",
        );
        let after = match hash_directory(&directory) {
            Ok(hash) => hash,
            Err(error) => panic!("directory should hash after target write: {error}"),
        };

        assert_eq!(before, after);
        remove_test_dir(directory);
    }

    #[test]
    fn actual_localize_docker_context_fingerprint_dirs_hash() -> Result<()> {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let cli_workspace_root = manifest_dir
            .parent()
            .context("phoxal-cli-core manifest directory must be under workspace root")?;
        let workspace_root = cli_workspace_root
            .parent()
            .map(|phoxal_root| phoxal_root.join("framework"))
            .filter(|framework_root| framework_root.join("runtimes/localize").is_dir())
            .unwrap_or_else(|| cli_workspace_root.to_path_buf());
        let fingerprint_paths = [
            workspace_root.join("runtimes/localize/orb-slam3-sys"),
            workspace_root.join("runtimes/localize/src"),
        ];

        hash_fingerprint_paths(&fingerprint_paths)?;

        Ok(())
    }
}
