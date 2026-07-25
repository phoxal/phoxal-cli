# phoxal-cli

Consumer CLI for the Phoxal robot framework. The repository is `phoxal-cli`; the
installed executable is `phoxal`.
You run it from a robot project: it reads `robot.yaml`, resolves the graph against a verified generated artifact suite when official artifacts are needed, and drives the develop and simulate loop.
It owns the resolver and `robot.yaml` discovery. Every robot project has a root
Cargo anchor package and committed `Cargo.lock`; the exact resolved `phoxal`
package selects the framework train. The git-ignored `.phoxal/` tree is only an
artifact transport cache and never selects compatibility.

## Commands

```sh
# Hand-author robot.yaml in a new project directory (see the framework repo's
# fixture/ robots for a working starting point).
cd rover

phoxal check                  # validate the graph's participants + config via phoxal::check
phoxal service suite          # print official services from the configured artifact suite
phoxal run                    # stage what is stale and supervise the graph (--watch to hot-swap edits)
phoxal start                  # run the graph headless (the robot-instance verb; no TUI)
phoxal build                  # stage a runtime layout and archive it as build.phoxal
phoxal deploy robot@host      # remote source build, then install atomically
sudo phoxal install build.phoxal
sudo phoxal rollback
phoxal simulation run default # resolve and report the simulation launch plan
phoxal logs -f                # stream participant bus logs from a reachable robot
phoxal status safety          # inspect the latest safety state over the robot bus

phoxal update                 # verify, activate, and prune project-local artifacts
```

| Command | What it does |
|---|---|
| `check` | Resolve `robot.yaml` and the locked Cargo workspace, stage every participant, extract embedded metadata, and validate the complete graph against `phoxal::check`. `--strict` additionally fails on coherence warnings. |
| `validate` | Lower-level `robot.yaml` structure and Cargo workspace runtime-discovery checks that back `check`. |
| `run [ROOT]` | Universal launch: build what is stale and run what is staged. On a source project it refreshes the host-triple staging under `.phoxal/build/<host-triple>/` (cargo-build stale crates, link the locked train's vendored officials, flatten `robot.yaml`, stage assets), then supervises the graph in the terminal UI; on an already-staged root or an extracted `build.phoxal` the build step is a no-op and the identical execution path runs it. `--watch` recompiles an immutable whole-plan revision and reconciles only changed participants. `run`/`start`/`build` never touch the network - a missing vendored artifact fails with "run `phoxal update`". |
| `start [ROOT]` | The headless robot-instance verb `phoxal.service` uses. Same pipeline as `run` without the TUI: interactively it returns after required readiness; under systemd (`NOTIFY_SOCKET` present) it stays the foreground `sd_notify` resident. |
| `build [PROJECT]` | Stage a runtime layout for a target and archive it as a deterministic `build.phoxal` (identical contents produce identical bytes). `--target <TRIPLE>` selects the target. `--builder local` compiles on this host; `--builder container` compiles in the pinned Rust image; `--builder ssh://user@host` snapshots source, compiles in a remote temporary directory, and pulls back the same archive. |
| `install <build.phoxal>` | Safely extract, validate, fsync, and atomically activate an immutable release under `/var/lib/phoxal/releases`; restart the one service and restore the prior symlink on readiness failure. |
| `rollback [--to RELEASE]` | Activate the immediately older sortable release, or an explicit release directory, with the same readiness and restoration gate. |
| `deploy <user@host> [PROJECT]` | Snapshot source, build remotely, and invoke the installer. `--build <archive>` skips the source/toolchain leg; the robot needs neither Cargo nor Git. |
| `simulation run <world>` | Resolve the robot and report or run the host-native simulation plan. `--watch` creates a new plan revision for source or project-manifest edits and re-checks driver metadata/substitutions without launching drivers. |
| `simulation join` | Reserved entry point for joining a running multi-robot simulation; currently reports that the workflow is not available yet. |
| `logs [participant]` | Stream participant bus log events from a reachable robot. `-f`/`--follow` keeps streaming; omit `participant` for every participant. |
| `status <safety|motion|localization>` | Inspect the latest domain state over the robot bus. `engage-estop` and `reset-estop` publish the robot-wide software emergency-stop request. |
| `service install\|uninstall\|status\|suite` | Manage exactly one `phoxal.service`, inspect it, or print official services from the configured artifact suite. Device-specific hardware provisioning remains explicit. |
| `update` | Fetch and verify the immutable suite for the locked train, atomically retarget cached artifacts, and prune inactive cached versions after successful activation. Supports `--dry-run`; use `cargo update -p phoxal` to change trains. |
| `doctor` | Check host prerequisites (Webots, Rust toolchain) without changing anything. |
| `version` | Print the CLI version, wire codec, and participant metadata section names. |
| `self upgrade` | Update the CLI binary itself. |

Interactive source sessions bind their infrastructure router at
`<project>/.phoxal/zenoh.sock`; the installed runtime uses
`/run/phoxal/zenoh.sock`. Router bootstrap readiness travels over
an inherited one-shot file descriptor; stdout and stderr are logs only. Every
launched graph process crosses the same environment-scrubbing `ManagedChild`
boundary and is registered with an out-of-process guardian, so killing the CLI
cannot leave its process graph behind. Shutdown drains graph participants
concurrently before host tools and budgets each phase by its slowest member.

Finite commands print append-only, pipe-friendly text. Live `run` and `simulation run`
sessions require a terminal and fail with an actionable error when redirected.

### Interactive sessions

On an interactive terminal, both `run` and live `simulation run` use the
same five fixed pages:

| Page | Purpose |
|---|---|
| **Overview** | Robot-runtime summary and direct lifecycle attention; host and simulation state stay visible in the persistent header. |
| **Runtimes** | User services, framework services, and drivers in separate lists, plus a portable lifecycle and contract-focused detail view. Standard tools and Webots internals stay hidden; per-process CPU/RSS is intentionally omitted because not every runtime executes as a host process. |
| **Logs** | One bounded stream with source, participant, severity, text, and follow filters. Runtime and tool logs share the same view. |
| **Bus** | Router throughput history, per-producer rates, topic rates/counts, and receive-time freshness. |
| **Input** | Devices on the left and read-only controller/command/tool state on the right. Selection never enables manual input. |

Navigation follows a menu stack: arrows move a soft cursor, `Enter` opens or
activates it, and `Esc` backs out one level. When no Help or Session Information
overlay is open, `1`-`5` open a page directly. `?` opens compact global help,
`i` opens read-only Session Information, and `q` or Ctrl-C stops.

The full interface needs a terminal of at least 44 columns by 18 rows. Smaller
terminals show a resize prompt instead of clipping selectable controls.

Page actions are scoped to the active page. **Overview** is a read-only summary.
On **Runtimes**, Up/Down chooses a runtime, `Enter` opens details, and `Esc`
closes details before another runtime can be selected; `l` opens Logs for that
runtime, and `r` restarts it. On **Logs**, Left/Right chooses a control
and `Enter` activates it; the Source control cycles through All, Runtimes, and
Tools. `/` edits text, `f` edits the participant filter, `s` changes severity,
Up scrolls older and pauses live output, Down scrolls toward newer retained
lines, `Space` pauses or follows, and `End` returns to live output. On **Bus**,
Left/Right chooses Filter, Sort, or
Internals, Up/Down scrolls topics, `/` filters, `s` changes sorting, and `a`
reveals internal topics. On **Input**, Up/Down chooses a device, `Enter` selects
it, `e` enables, `x` disables, and `r` rescans; input starts disabled and
selection never enables it. Switching into Input refreshes the device inventory
automatically. Rejected input actions are reported in Logs, not duplicated in
multiple Input fields; the latest acknowledgement appears once beside the live
Input state and remains available in Logs.

The first error in a session opens Logs automatically, closes any overlay or
runtime detail, resets the filters to all sources at Error severity, and resumes
live following so the new failure is visible immediately.

While editing a Logs or Bus filter, results update as you type. Use `Backspace`
to remove text; `Enter` or `Esc` finishes editing and keeps the current text.

The terminal title is `phoxal <robot-id> - <namespace>` for the session
lifetime.

### Project runtimes

The robot repository is a Cargo workspace. Its directory layout declares which
workspace members are runnable:

```text
services/    # each member has exactly one bin target
tools/       # each member has exactly one bin target
components/  # each member has zero or one bin target
```

`cargo metadata --locked` discovers these members. A component with a bin target
has a driver; a lib-only component carries assets without a driver. Its
`component.yaml` must live in that crate or in exactly one direct dependency.
Use normal Cargo path/git dependencies, thin wrapper crates, and `[patch]` for
local or remote reuse. A workspace participant whose embedded identity matches
an official runtime replaces the suite binary; the staged binary's embedded
kind and identity remain authoritative.

## Artifact Suite

`phoxal` consumes the framework-generated `phoxal.suite/v1` attached to the
exact locked train release. The suite is only the immutable byte inventory for
official package, train, and target combinations. The CLI release owns the
official Native runtime set; Webots adds its controller and supervisor. Every
runtime is per robot and required, apart from the router's internal
graph-recreation policy. Launch planning never consumes the suite's profile,
scope, or criticality fields. An official package unknown to this CLI fails
with an explicit instruction to update the CLI. Legacy `phoxal.suite/v0`
descriptors are rejected rather than guessed or upgraded in place.

For example:
`https://github.com/phoxal/framework/releases/download/v0.38.1/suite.json`.
Local development may use `--suite <path>`, `PHOXAL_SUITE=<path>`, or
the global `--suite` option; every override must still declare the locked train
version.
Use `--offline --suite <local-path>` (or the equivalent environment variables)
to disable network access and resolve from that immutable local descriptor plus
already verified vendored artifacts. Offline mode never fetches or reconstructs
the suite. `cargo update -p phoxal` is the explicit train-bump boundary.

Every per-robot `tool-device` receives the same bounded identity derived from
the canonical project root. Device samples remain attributed to their robot
roots, while co-hosted robots expose the shared identity so clients can join or
deduplicate those observations honestly.

## Install

```sh
curl -fsSL https://phoxal.com/install.sh | sh
```

To pin a release, set `PHOXAL_CLI_VERSION` to a tag:

```sh
curl -fsSL https://phoxal.com/install.sh | PHOXAL_CLI_VERSION=v0.4.0 sh
```

To update an existing install to the latest release:

```sh
phoxal self upgrade
```

## Releasing

`release-plz` owns the workspace version, `CHANGELOG.md`, and the single
automation-managed release PR. Normal changes on `main` update a
`release-plz-*` branch through the `phoxal-release-bot` GitHub App. The fixture
crate is excluded; `phoxal-cli`, `phoxal-cli-core`, and `phoxal-cli-ui` inherit
the one root workspace version, and the PR title tracks that version as
`chore(release): release vX.Y.Z`. Because none of those packages publish to a
Cargo registry, release-plz compares them with the latest immutable `vX.Y.Z`
tag checkout supplied as its local registry baseline.

Merging that managed PR is the only publication trigger. The repository-owned
release workflow builds the CLI for every supported target, produces sibling
SHA-256 files, and calls the shared retry-safe GitHub release seam only after
all builds succeed. `release-plz` does not publish crates, create tags, or
create GitHub Releases directly in this repository.

See [`.github/workflows/release-plz.yml`](.github/workflows/release-plz.yml),
[`.github/workflows/release.yml`](.github/workflows/release.yml), and
[`release-plz.toml`](release-plz.toml).

### Build from Source

```sh
cargo install --git https://github.com/phoxal/phoxal-cli --bin phoxal
```

The shipped binary is `phoxal`; the package keeps the `phoxal-cli` name.

## Simulate

```sh
phoxal simulation run <world>
```

`<world>` resolves to a `.wbt` file in this order:

1. `<project>/worlds/<world>.wbt`
2. `<project>/<world>` (path-as-given, e.g. `worlds/foo.wbt`)

Example: `phoxal simulation run default` finds `worlds/default.wbt` in the project.

## Live Split-Recovery Gate

`scripts/live-simulate-gate.sh` is the split-recovery smoke gate for
the separated repos. Official service and driver binaries resolve from the
framework's published artifact suite; run `phoxal update` once to vendor the
locked train (or pass a locally generated suite with `--suite` /
`PHOXAL_SUITE` when developing against an unreleased framework).

```sh
# from the phoxal-cli checkout; ROBOT_DIR defaults to the framework hello-rover example
scripts/live-simulate-gate.sh            # smoke: live resolve + dry-run report (CI-safe)
scripts/live-simulate-gate.sh --live     # full live run (needs Webots)
```

The smoke phase runs `simulation run default --dry-run` to resolve and report the
planned local launch without staging `.phoxal/build` or a release directory. It
needs no daemon of any kind. The `--live` phase additionally requires Webots on
`PATH`; run `phoxal update` first, then it runs `simulation run default` so you can confirm the router,
Webots, host tools, and bus connectivity.

## Host layout

```text
~/.phoxal/simulator.lock            Host-global simulation lease.

<project>/.phoxal/project.lock      Permanent per-project operation authority for run, build, and update.
<project>/.phoxal/artifacts/<provider>/<package>/versions/<version>/targets/<target>/  Unpacked target artifacts.
<project>/.phoxal/artifacts/<provider>/<package>/versions/<version>/assets/             Unpacked component assets.
<project>/.phoxal/artifacts/<provider>/<package>/active                                 Atomic selected-version symlink.
<project>/.phoxal/git/              Git-pinned checkouts.
<project>/.phoxal/webots/           Webots staging.
<project>/.phoxal/build/<triple>/   Staged runtime layout (compiled robot.yaml + flat bin/ + assets) per target, shared by `run` and live simulation.

/var/phoxal -> /var/lib/phoxal/releases/<utc>-<digest>  Active installed runtime.
/var/lib/phoxal/state/              Persistent installed lock and plan content.
/run/phoxal/                        Installed supervisor and router sockets.
```

See [Device preparation and deployment](docs/DEPLOYMENT.md) for the complete
service, permission, install, rollback, deploy, and power-loss contract.

To reset all generated project state while no Phoxal command is active, delete
`<project>/.phoxal/`. Deleting it during `run`, simulation, or update is
unsupported because that bypasses the CLI's active locks. Do not delete
`~/.phoxal/` while a simulation is active because it contains the host-global
simulator lease. The project lock inode is intentionally permanent; its
advisory lock is the authority, while its JSON metadata names the operation,
project, selected entry, and owning PID for diagnostics only. Process death
releases ownership without deleting or repairing the file.

## License

AGPL-3.0-only - see [LICENSE](LICENSE) for the full license text.
A commercial license is available for downstream products that cannot
comply with AGPL - see [COMMERCIAL.md](COMMERCIAL.md) and reach out via
<https://phoxal.com>.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). DCO sign-off required on every
commit.

The crate ownership and dependency rules are documented in
[ARCHITECTURE.md](ARCHITECTURE.md).

## Phoxal

- <https://phoxal.com>
- <https://github.com/phoxal>
