use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::AppContext;
use crate::shell;

const WEBOTS_FALLBACK: &str = "phoxal simulation webots run <world>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    Ok(String),
    Warn(String),
    Fail(HostError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostErrorCode {
    WebotsMissing,
    WebotsHomeUnresolved,
}

impl HostErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebotsMissing => "PHOXAL-E-HOST-WEBOTS-MISSING",
            Self::WebotsHomeUnresolved => "PHOXAL-E-HOST-WEBOTS-HOME-UNRESOLVED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    code: HostErrorCode,
    summary: String,
    fix: Vec<String>,
    fallback: Option<String>,
}

impl HostError {
    fn new(
        code: HostErrorCode,
        summary: impl Into<String>,
        fix: impl IntoIterator<Item = impl Into<String>>,
        fallback: Option<&str>,
    ) -> Self {
        Self {
            code,
            summary: summary.into(),
            fix: fix.into_iter().map(Into::into).collect(),
            fallback: fallback.map(ToString::to_string),
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "error[{}]", self.code.as_str())?;
        writeln!(formatter, "{}", self.summary)?;
        writeln!(formatter)?;
        writeln!(formatter, "Fix:")?;
        for step in &self.fix {
            writeln!(formatter, "  - {step}")?;
        }
        if let Some(fallback) = &self.fallback {
            writeln!(formatter)?;
            writeln!(formatter, "You can still run:")?;
            write!(formatter, "  {fallback}")?;
        }
        Ok(())
    }
}

impl std::error::Error for HostError {}

#[derive(Debug, Clone)]
struct WebotsExecutable {
    path: PathBuf,
    version: Option<String>,
    source: &'static str,
}

#[derive(Debug, Clone)]
struct WebotsHome {
    path: PathBuf,
    source: &'static str,
}

pub fn report(app: &AppContext) {
    for status in [
        probe_version(),
        probe_rust_tools(),
        probe_webots_executable(),
        probe_webots_home(),
        probe_webots_controller_lib_dir(),
    ] {
        print_status(app, status);
    }
}

pub fn preflight() -> Result<(), HostError> {
    require_ok(probe_rust_tools())?;
    require_ok(probe_webots_executable())?;
    require_ok(probe_webots_home())?;
    require_ok(probe_webots_controller_lib_dir())
}

pub fn webots_executable_path() -> Result<PathBuf, HostError> {
    detect_webots_executable().map(|webots| webots.path)
}

/// Detect the Webots installation directory (`WEBOTS_HOME`-equivalent), the
/// same resolution `preflight`/`probe_webots_home` use, exposed for callers
/// (simulator build staging) that want to set `WEBOTS_HOME` defensively for a
/// child `cargo build` when the process environment does not already carry
/// it.
pub fn webots_home_path() -> Result<PathBuf, HostError> {
    let executable = detect_webots_executable().ok();
    detect_webots_home(executable.as_ref()).map(|home| home.path)
}

pub fn probe_version() -> ProbeStatus {
    ProbeStatus::Ok(format!("phoxal {}", env!("CARGO_PKG_VERSION")))
}

pub fn probe_rust_tools() -> ProbeStatus {
    let rustup = shell::run_stdout("rustup", ["--version"], None);
    let cargo = shell::run_stdout("cargo", ["--version"], None);
    match (rustup, cargo) {
        (Ok(rustup), Ok(cargo)) => ProbeStatus::Ok(format!(
            "Rust tools: {}, {}",
            first_line(&rustup, "rustup installed"),
            first_line(&cargo, "cargo installed")
        )),
        (rustup, cargo) => {
            let mut missing = Vec::new();
            if rustup.is_err() {
                missing.push("rustup");
            }
            if cargo.is_err() {
                missing.push("cargo");
            }
            ProbeStatus::Warn(format!(
                "{} missing; user service and driver builds may fail - install Rust from https://rustup.rs and ensure rustup and cargo are on PATH",
                missing.join(" and ")
            ))
        }
    }
}

pub fn probe_webots_executable() -> ProbeStatus {
    match detect_webots_executable() {
        Ok(webots) => {
            let version = webots
                .version
                .as_deref()
                .map(|version| format!(" ({version})"))
                .unwrap_or_default();
            ProbeStatus::Ok(format!(
                "Webots executable: {} via {}{}",
                webots.path.display(),
                webots.source,
                version
            ))
        }
        Err(error) => ProbeStatus::Fail(error),
    }
}

pub fn probe_webots_home() -> ProbeStatus {
    let executable = detect_webots_executable().ok();
    match detect_webots_home(executable.as_ref()) {
        Ok(home) => ProbeStatus::Ok(format!(
            "WEBOTS_HOME: {} via {}",
            home.path.display(),
            home.source
        )),
        Err(error) => ProbeStatus::Fail(error),
    }
}

pub fn probe_webots_controller_lib_dir() -> ProbeStatus {
    let executable = detect_webots_executable().ok();
    let Ok(home) = detect_webots_home(executable.as_ref()) else {
        return ProbeStatus::Fail(webots_home_unresolved());
    };
    match detect_webots_controller_lib_dir(&home) {
        Ok(path) => ProbeStatus::Ok(format!("Webots controller library: {}", path.display())),
        Err(error) => ProbeStatus::Fail(error),
    }
}

fn require_ok(status: ProbeStatus) -> Result<(), HostError> {
    match status {
        ProbeStatus::Ok(_) | ProbeStatus::Warn { .. } => Ok(()),
        ProbeStatus::Fail(error) => Err(error),
    }
}

fn print_status(app: &AppContext, status: ProbeStatus) {
    match status {
        ProbeStatus::Ok(message) => app.ui.success(message),
        ProbeStatus::Warn(message) => app.ui.warn(message),
        ProbeStatus::Fail(error) => app.ui.warn(&error.summary),
    }
}

fn first_line<'a>(stdout: &'a str, fallback: &'a str) -> &'a str {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback)
}

fn detect_webots_executable() -> Result<WebotsExecutable, HostError> {
    // Prefer a known install's direct binary (e.g. macOS's
    // `<home>/Contents/MacOS/webots`) over whatever `webots` resolves to on
    // PATH. On macOS, Cyberbotics installs a `/usr/local/bin/webots` shim that
    // is just `open -a /Applications/Webots.app --args "$@"`: `open -a`
    // detaches and returns 0 immediately, so a child process spawned against
    // that shim exits at once while the real app keeps running detached. The
    // direct binary under the app bundle does not have this problem and is
    // also what a foreground-attached launch needs. Linux/Windows installs do
    // not carry this indirection, so this ordering is a no-op there whenever
    // no known install directory exists (the PATH fallback below still
    // applies for a custom, non-standard install location).
    if let Some(found) = first_working_executable(known_webots_executable_paths(), "known install")
    {
        return Ok(found);
    }

    if let Ok(version) = shell::run_stdout("webots", ["--version"], None) {
        return Ok(WebotsExecutable {
            path: executable_on_path("webots").unwrap_or_else(|| PathBuf::from("webots")),
            version: Some(first_line(&version, "version unavailable").to_string()),
            source: "PATH",
        });
    }

    Err(webots_missing())
}

/// Return the first candidate that exists as a file and responds to
/// `--version`, tagged with `source`. Pulled out of `detect_webots_executable`
/// so the known-install-first preference is directly unit-testable against a
/// fake install layout, without mutating process-global `PATH`/`WEBOTS_HOME`.
fn first_working_executable(
    candidates: Vec<PathBuf>,
    source: &'static str,
) -> Option<WebotsExecutable> {
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let Some(executable) = candidate.to_str() else {
            continue;
        };
        if let Ok(version) = shell::run_stdout(executable, ["--version"], None) {
            return Some(WebotsExecutable {
                path: candidate,
                version: Some(first_line(&version, "version unavailable").to_string()),
                source,
            });
        }
    }
    None
}

fn detect_webots_home(executable: Option<&WebotsExecutable>) -> Result<WebotsHome, HostError> {
    if let Some(home) = explicit_webots_home() {
        return Ok(WebotsHome {
            path: home,
            source: "WEBOTS_HOME",
        });
    }

    if let Some(webots) = executable
        && let Some(home) = infer_webots_home_from_executable(&webots.path)
    {
        return Ok(WebotsHome {
            path: home,
            source: webots.source,
        });
    }

    for home in known_webots_home_paths() {
        if home.is_dir() {
            return Ok(WebotsHome {
                path: home,
                source: "known install",
            });
        }
    }

    Err(webots_home_unresolved())
}

fn detect_webots_controller_lib_dir(home: &WebotsHome) -> Result<PathBuf, HostError> {
    for candidate in webots_controller_lib_candidates(&home.path) {
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    Err(webots_home_unresolved())
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    let names = executable_names(name);
    for directory in env::split_paths(&paths) {
        for executable_name in &names {
            let candidate = directory.join(executable_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_names(name: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        vec![
            format!("{name}.exe"),
            format!("{name}.bat"),
            format!("{name}.cmd"),
            name.to_string(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![name.to_string()]
    }
}

fn explicit_webots_home() -> Option<PathBuf> {
    env::var_os("WEBOTS_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn known_webots_executable_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = explicit_webots_home() {
        candidates.extend(webots_executable_candidates_for_home(&home));
    }
    candidates.extend(
        known_webots_home_paths()
            .into_iter()
            .flat_map(|home| webots_executable_candidates_for_home(&home)),
    );
    dedup_paths(candidates)
}

fn known_webots_home_paths() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    #[cfg(target_os = "macos")]
    {
        homes.push(PathBuf::from("/Applications/Webots.app"));
        homes.push(PathBuf::from("/Applications/Cyberbotics/Webots.app"));
    }
    #[cfg(target_os = "linux")]
    {
        homes.push(PathBuf::from("/usr/local/webots"));
        homes.push(PathBuf::from("/opt/webots"));
    }
    #[cfg(windows)]
    {
        if let Some(program_files) = env::var_os("ProgramFiles") {
            homes.push(PathBuf::from(program_files).join("Webots"));
        }
        if let Some(program_files) = env::var_os("ProgramFiles(x86)") {
            homes.push(PathBuf::from(program_files).join("Webots"));
        }
    }
    homes
}

fn webots_executable_candidates_for_home(home: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut candidates = vec![
            home.join("webots"),
            home.join("bin").join("webots"),
            home.join("Contents").join("MacOS").join("webots"),
        ];
        candidates.push(home.join("webots.exe"));
        candidates.push(
            home.join("msys64")
                .join("mingw64")
                .join("bin")
                .join("webots.exe"),
        );
        candidates
    }
    #[cfg(not(windows))]
    {
        vec![
            home.join("webots"),
            home.join("bin").join("webots"),
            home.join("Contents").join("MacOS").join("webots"),
        ]
    }
}

fn webots_controller_lib_candidates(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("lib").join("controller"),
        home.join("Contents").join("lib").join("controller"),
    ]
}

fn infer_webots_home_from_executable(executable: &Path) -> Option<PathBuf> {
    for ancestor in executable.ancestors() {
        if ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "Webots.app")
        {
            return Some(ancestor.to_path_buf());
        }
    }

    let parent = executable.parent()?;
    if parent.join("lib").join("controller").is_dir() {
        return Some(parent.to_path_buf());
    }
    if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "bin")
    {
        let home = parent.parent()?;
        if home.join("lib").join("controller").is_dir() {
            return Some(home.to_path_buf());
        }
    }
    None
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.contains(&path) {
            deduped.push(path);
        }
    }
    deduped
}

fn webots_missing() -> HostError {
    HostError::new(
        HostErrorCode::WebotsMissing,
        "Webots is required for live simulate because it launches the staged world and simulator controllers.",
        [
            "Install Webots from Cyberbotics.",
            "Put webots on PATH or install it in the standard platform location.",
        ],
        Some(WEBOTS_FALLBACK),
    )
}

fn webots_home_unresolved() -> HostError {
    HostError::new(
        HostErrorCode::WebotsHomeUnresolved,
        "WEBOTS_HOME must resolve to a Webots installation with controller libraries for live simulate.",
        [
            "Set WEBOTS_HOME to the Webots installation directory.",
            "Confirm the Webots controller library directory exists under WEBOTS_HOME.",
        ],
        Some(WEBOTS_FALLBACK),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_fake_executable(path: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, script).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn macos_home_candidates_include_the_direct_app_bundle_binary() {
        let home = Path::new("/Applications/Webots.app");
        let candidates = webots_executable_candidates_for_home(home);
        assert!(
            candidates.contains(&home.join("Contents").join("MacOS").join("webots")),
            "known-install candidates for a Webots.app-shaped home must include the direct app binary: {candidates:?}"
        );
    }

    /// Bug 2 regression test. A fake webots-home dir stands in for
    /// `/Applications/Webots.app`, with only its direct `Contents/MacOS/webots`
    /// binary present (no `<home>/webots`, matching the real app bundle
    /// layout). `first_working_executable` over that home's known-install
    /// candidates must find and select the direct binary. This is the exact
    /// selection `detect_webots_executable` now runs *before* ever falling
    /// back to whatever `webots` resolves to on PATH - which on a real macOS
    /// host is `/usr/local/bin/webots`, an `open -a` shim that exits 0
    /// instantly without actually launching anything attached. Preferring the
    /// known-install candidates first means the direct binary wins over that
    /// shim whenever a standard install is present.
    #[cfg(unix)]
    #[test]
    fn known_install_selection_finds_the_direct_binary_for_a_fake_webots_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("Webots.app");
        let direct_binary = home.join("Contents").join("MacOS").join("webots");
        write_fake_executable(
            &direct_binary,
            "#!/bin/sh\necho \"Webots R2026a\"\nexit 0\n",
        );

        let candidates = webots_executable_candidates_for_home(&home);
        let found = first_working_executable(candidates, "known install")
            .expect("the fake direct binary should be found and respond to --version");
        assert_eq!(found.path, direct_binary);
        assert_eq!(found.source, "known install");
    }

    /// Companion to the regression test above: even when a PATH-style
    /// candidate list is placed first (as `detect_webots_executable` used to
    /// do), `first_working_executable` just returns whatever is first that
    /// works - so the fix is entirely in the *order* `detect_webots_executable`
    /// now calls it: known-install candidates, then PATH. This test pins that
    /// an `open -a`-style wrapper (exits 0 instantly, no stdout) still counts
    /// as "working" by this helper's own criteria, which is exactly why the
    /// ordering (not the working-ness check) is what had to change.
    #[cfg(unix)]
    #[test]
    fn an_instant_exit_wrapper_still_looks_like_it_works_in_isolation() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper = temp.path().join("webots");
        write_fake_executable(&wrapper, "#!/bin/sh\nexit 0\n");

        let found = first_working_executable(vec![wrapper.clone()], "PATH")
            .expect("an instantly-exiting script still passes the --version probe");
        assert_eq!(found.path, wrapper);
    }
}
