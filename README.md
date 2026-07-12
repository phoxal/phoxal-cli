# phoxal-cli

Consumer CLI for the Phoxal robot framework.
You run it from a robot project: it reads `robot.yaml`, resolves the graph against a verified generated artifact catalog when official artifacts are needed, and drives the develop, simulate, and deploy loop.
It owns the resolver and `robot.yaml` discovery.
There is no lockfile or global artifact cache. The project's git-ignored `.phoxal/` tree is the vendored set; `phoxal update` is the explicit channel-head mutation boundary.
Production reproducibility comes from explicit `artifacts.pins` plus the deployed `phoxal-release.json` record.

## Commands

```sh
# Hand-author robot.yaml in a new project directory (see the framework repo's
# fixture/ robots for a working starting point).
cd rover

phoxal-cli check                  # validate the graph's participants + config via phoxal::check
phoxal-cli service catalog        # print official services from the configured artifact catalog
phoxal-cli run --watch            # supervise the graph and hot-swap checked local edits
phoxal-cli simulate default       # resolve and report the simulation launch plan
phoxal-cli logs -f                # stream participant bus logs from a reachable robot
phoxal-cli status                 # print the local supervisor board snapshot

phoxal-cli update                 # verify downloads and atomically retarget active versions
phoxal-cli deploy robot@host      # build, render, sync, restart, and report systemd health
phoxal-cli deploy --dry-run --target aarch64  # hostless render + cross-build validation
```

| Command | What it does |
|---|---|
| `check` | Resolve `robot.yaml`, stage participants, extract their embedded metadata sections, and validate the graph against `phoxal::check`. `--service <name>` scopes user-service selection. `--strict` additionally fails on coherence warnings. |
| `validate` | Lower-level `robot.yaml` structure and user-service phoxal-dependency checks that back `check`. |
| `run` | Supervise the resolved host-native graph. `--watch` rebuilds changed local participants, re-runs the graph proof, and swaps the checked process in place. `--message-format json` prints exact participant launch command lines and env. |
| `simulate <world>` | Resolve the robot and report or run the host-native simulation plan. `--watch` hot-swaps service edits and re-checks driver metadata/substitutions without launching drivers. |
| `logs [participant]` | Stream participant bus log events from a reachable robot. `-f`/`--follow` keeps streaming; omit `participant` for every participant. |
| `status [release|resume <participant>]` | Print the local supervisor board, or explicitly release a managed child for manual/debugger execution and later resume it under supervision. |
| `service catalog` | Print official services from the configured artifact catalog. |
| `update` | Resolve stable/nightly heads, verify downloads, and retarget `active`. Supports `--dry-run` and JSON; cleanup remains explicit through `cache clean --artifacts`. |
| `cache clean` | Remove selected project-local `artifacts`, `build`, `git`, or `webots` state, with size reporting and `--dry-run`. |
| `deploy <user@host>` | Probe the robot arch, resolve/check the graph, cross-build local source artifacts for musl, render native systemd units/env/release record, sync to `/opt/phoxal` and `/etc/systemd/system`, restart `phoxal.target`, and report systemd readiness. Prints the v0 pre-stable warning. `--dry-run --target <arch>` renders hostless for validation. |
| `doctor` | Check host prerequisites (Webots, Rust toolchain) without changing anything. |
| `version` | Print the CLI version, wire codec, and participant metadata section names. |
| `self upgrade` | Update the CLI binary itself. |

Commands that emit machine-readable state accept `--message-format human|json`.

### Dev Path Overrides

Local artifact source overrides use exact provider-qualified package IDs under `artifacts.pins` and are legal only in dev overlays such as `robot.dev.yaml`:

```yaml
artifacts:
  pins:
    phoxal/service-drive:
      path: ../framework/service/drive
    phoxal/component-ddsm115:
      path: ../framework/component/ddsm115
```

Load them with `--env dev`. Base `robot.yaml` is fail-closed for `{ path: ... }` pins so production manifests stay catalog/release based. Unknown or unused pin keys are errors.

## Artifact Catalog

`phoxal-cli` consumes the framework-generated `phoxal.catalog/v0` JSON catalog. The default is `https://github.com/phoxal/framework/releases/latest/download/catalog.json`. Local development may use `--catalog <path>`, `PHOXAL_ARTIFACT_CATALOG=<path>`, or `artifacts.catalog`; non-default sources are frozen single-catalog sources.

Stable resolution follows `heads.stable` to that release's frozen catalog; nightly follows `heads.nightly`. Normal commands retain project-vendored `active` versions and warn about head drift. Use `--offline` or `PHOXAL_OFFLINE=1` to skip catalog probes. Fresh CI can run `phoxal update && phoxal check --strict`; deterministic CI should use explicit version/SHA pins.

> `v0` is pre-stable: artifacts built at different times may not interoperate.
> Pin exact artifact versions in `robot.yaml` when you need to re-deploy a previously recorded `phoxal-release.json` set.

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

### Build from Source

```sh
cargo install --git https://github.com/phoxal/phoxal-cli
```

## Simulate

```sh
phoxal-cli simulate <world>
```

`<world>` resolves to a `.wbt` file in this order:

1. `<project>/worlds/<world>.wbt`
2. `<project>/<world>` (path-as-given, e.g. `worlds/foo.wbt`)

Example: `phoxal-cli simulate default` finds `worlds/default.wbt` in the project.

## Live Split-Recovery Gate

`scripts/live-simulate-gate.sh` is the historical split-recovery smoke gate for
the separated repos. The D5 native artifact path now resolves official service
and driver metadata from the generated artifact catalog; published native
release assets are still pending. For local development, generate a metadata
catalog from the framework checkout and pass it with `--catalog` or
`PHOXAL_ARTIFACT_CATALOG`.

```sh
# from the phoxal-cli checkout; ROBOT_DIR defaults to ../robot-v1
scripts/live-simulate-gate.sh            # smoke: live resolve + dry-run report (CI-safe)
scripts/live-simulate-gate.sh --live     # full live run (needs Webots)
```

The smoke phase runs `simulate default --dry-run` to resolve and report the
planned local launch without writing `.phoxal/run` or a release directory. It
needs no daemon of any kind. The `--live` phase additionally requires Webots on
`PATH`; run `phoxal update` first, then it runs `simulate default` so you can confirm the router,
Webots, host tools, and bus connectivity. Until native release assets publish,
official-service launch failures should surface as catalog or native-pending
diagnostics rather than as missing static catalog entries.

## Host layout

```text
~/.phoxal/simulator.lock            The only host-global item.

<project>/.phoxal/artifacts/<provider>/<package>/versions/<version>/targets/<target>/  Unpacked target artifacts.
<project>/.phoxal/artifacts/<provider>/<package>/versions/<version>/assets/             Unpacked component assets.
<project>/.phoxal/artifacts/<provider>/<package>/active                                 Atomic selected-version symlink.
<project>/.phoxal/git/              Git-pinned checkouts.
<project>/.phoxal/build/            Cross-build state.
<project>/.phoxal/webots/           Webots staging.
<project>/.phoxal/run/              Supervisor state.
```

## License

AGPL-3.0-only - see [LICENSE](LICENSE) for the full license text.
A commercial license is available for downstream products that cannot
comply with AGPL - see [COMMERCIAL.md](COMMERCIAL.md) and reach out via
<https://phoxal.com>.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). DCO sign-off required on every
commit.

## Phoxal

- <https://phoxal.com>
- <https://github.com/phoxal>
