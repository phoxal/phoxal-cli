# phoxal-cli

Consumer CLI for the Phoxal robot framework.
You run it from a robot project: it reads `robot.yaml`, resolves the graph against a verified generated artifact suite when official artifacts are needed, and drives the develop, simulate, and deploy loop.
It owns the resolver and `robot.yaml` discovery. Every robot project has a root
Cargo anchor package and committed `Cargo.lock`; the exact resolved `phoxal`
package selects the framework train. The git-ignored `.phoxal/` tree is only an
artifact transport cache and never selects compatibility.

## Commands

```sh
# Hand-author robot.yaml in a new project directory (see the framework repo's
# fixture/ robots for a working starting point).
cd rover

phoxal-cli check                  # validate the graph's participants + config via phoxal::check
phoxal-cli service suite          # print official services from the configured artifact suite
phoxal-cli run --watch            # supervise the graph and hot-swap checked local edits
phoxal-cli simulation run default # resolve and report the simulation launch plan
phoxal-cli logs -f                # stream participant bus logs from a reachable robot
phoxal-cli status safety          # inspect the latest safety state over the robot bus

phoxal-cli update                 # verify, activate, and prune project-local artifacts
phoxal-cli deploy robot@host      # build, render, sync, restart, and report systemd health
phoxal-cli deploy --dry-run --target aarch64  # hostless render + cross-build validation
```

| Command | What it does |
|---|---|
| `check` | Resolve `robot.yaml`, stage participants, extract their embedded metadata sections, and validate the graph against `phoxal::check`. `--service <name>` scopes user-service selection. `--strict` additionally fails on coherence warnings. |
| `validate` | Lower-level `robot.yaml` structure and user-service phoxal-dependency checks that back `check`. |
| `run` | Supervise the resolved host-native graph in the terminal UI. `--watch` rebuilds changed local participants, re-runs the graph proof, and swaps the checked process in place. |
| `simulation run <world>` | Resolve the robot and report or run the host-native simulation plan. `--watch` hot-swaps service edits and re-checks driver metadata/substitutions without launching drivers. |
| `simulation join` | Reserved entry point for joining a running multi-robot simulation; currently reports that the workflow is not available yet. |
| `logs [participant]` | Stream participant bus log events from a reachable robot. `-f`/`--follow` keeps streaming; omit `participant` for every participant. |
| `status <safety|motion|localization>` | Inspect the latest domain state over the robot bus. `engage-estop` and `reset-estop` publish the robot-wide software emergency-stop request. |
| `service suite` | Print official services from the configured artifact suite. |
| `update` | Fetch and verify the immutable suite for the locked train, atomically retarget cached artifacts, and prune inactive cached versions after successful activation. Supports `--dry-run`; use `cargo update -p phoxal` to change trains. |
| `deploy <user@host>` | Probe the robot arch, resolve/check the graph, cross-build local source artifacts for musl, render native systemd units/env/release record, sync to `/opt/phoxal` and `/etc/systemd/system`, restart `phoxal.target`, and report systemd readiness. Prints the v0 pre-stable warning. `--dry-run --target <arch>` renders hostless for validation. |
| `doctor` | Check host prerequisites (Webots, Rust toolchain) without changing anything. |
| `version` | Print the CLI version, wire codec, and participant metadata section names. |
| `self upgrade` | Update the CLI binary itself. |

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

The terminal title is `phoxal-cli <robot-id> - <namespace>` for the session
lifetime.

### Dev Path Overrides

Local artifact source overrides use exact provider-qualified package IDs under `artifacts.pins`. Paths that stay inside the project are allowed in the base `robot.yaml`; absolute paths and paths that escape the project are legal only in dev overlays such as `robot.dev.yaml`:

```yaml
artifacts:
  pins:
    phoxal/service-drive:
      path: ../framework/service/drive
    phoxal/component-ddsm115:
      path: ../framework/component/ddsm115
```

Load escaping overrides with `--env dev`. Base `robot.yaml` remains fail-closed for absolute or escaping `{ path: ... }` pins so production manifests stay suite/release based. Unknown or unused pin keys are errors.

## Artifact Suite

`phoxal-cli` consumes the framework-generated `phoxal.suite/v0` attached to the
exact locked train release, for example
`https://github.com/phoxal/framework/releases/download/v0.36.0/suite.json`.
Local development may use `--suite <path>`, `PHOXAL_SUITE=<path>`, or
`artifacts.suite`; every override must still declare the locked train version.
Use `--offline --suite <local-path>` (or the equivalent environment variables)
to disable network access and resolve from that immutable local descriptor plus
already verified vendored artifacts. Offline mode never fetches or reconstructs
the suite. `cargo update -p phoxal` is the explicit train-bump boundary.

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
phoxal-cli self upgrade
```

## Releasing

`release-plz` owns the workspace version, `CHANGELOG.md`, and the single
automation-managed release PR. Normal changes on `main` update a
`release-plz-*` branch through the `phoxal-release-bot` GitHub App. The fixture
crate is excluded; `phoxal-cli`, `phoxal-cli-core`, and `phoxal-cli-ui` inherit
the one root workspace version. Because none of those packages publish to a
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
cargo install --git https://github.com/phoxal/phoxal-cli
```

## Simulate

```sh
phoxal-cli simulation run <world>
```

`<world>` resolves to a `.wbt` file in this order:

1. `<project>/worlds/<world>.wbt`
2. `<project>/<world>` (path-as-given, e.g. `worlds/foo.wbt`)

Example: `phoxal-cli simulation run default` finds `worlds/default.wbt` in the project.

## Live Split-Recovery Gate

`scripts/live-simulate-gate.sh` is the split-recovery smoke gate for
the separated repos. The D5 native artifact path now resolves official service
and driver metadata from the generated artifact suite; published native
release assets are still pending. For local development, generate a metadata
suite from the framework checkout and pass it with `--suite` or `PHOXAL_SUITE`.

```sh
# from the phoxal-cli checkout; ROBOT_DIR defaults to the framework hello-rover example
scripts/live-simulate-gate.sh            # smoke: live resolve + dry-run report (CI-safe)
scripts/live-simulate-gate.sh --live     # full live run (needs Webots)
```

The smoke phase runs `simulation run default --dry-run` to resolve and report the
planned local launch without writing `.phoxal/run` or a release directory. It
needs no daemon of any kind. The `--live` phase additionally requires Webots on
`PATH`; run `phoxal update` first, then it runs `simulation run default` so you can confirm the router,
Webots, host tools, and bus connectivity. Until native release assets publish,
official-service launch failures should surface as suite or native-pending
diagnostics rather than as missing static suite entries.

## Host layout

```text
~/.phoxal/simulator.lock            Host-global simulation lease.

<project>/.phoxal/project.lock      Permanent per-project operation authority for run, update, install, and deploy materialization.
<project>/.phoxal/artifacts/<provider>/<package>/versions/<version>/targets/<target>/  Unpacked target artifacts.
<project>/.phoxal/artifacts/<provider>/<package>/versions/<version>/assets/             Unpacked component assets.
<project>/.phoxal/artifacts/<provider>/<package>/active                                 Atomic selected-version symlink.
<project>/.phoxal/git/              Git-pinned checkouts.
<project>/.phoxal/build/            Cross-build state.
<project>/.phoxal/webots/           Webots staging.
<project>/.phoxal/run/robot/        Atomic resolved robot root shared by `run` and live simulation.
```

To reset all generated project state while no Phoxal command is active, delete
`<project>/.phoxal/`. Deleting it during `run`, simulation, deploy, or update is
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
