# phoxal-cli

Consumer CLI for the Phoxal robot framework.
You run it from a robot project: it reads `robot.yaml`, resolves the graph against a verified generated artifact catalog when official artifacts are needed, and drives the develop, simulate, and deploy loop.
It owns the resolver and `robot.yaml` discovery.
There is no lockfile: catalog revisions, host-tool versions, and component commits resolve live or from the local cache on every run.
Production reproducibility belongs to the future `deploy build` release artifact.

## Commands

```sh
phoxal-cli robot new rover        # scaffold a robot project
cd rover

phoxal-cli check                  # validate target generation + topology via emit-apis
phoxal-cli generations status     # inspect catalog readiness for the robot target
phoxal-cli service add brain      # scaffold a user service crate, register it in robot.yaml
phoxal-cli service run brain      # build and run that service host-native against the dev bus
phoxal-cli simulate default       # resolve and report the simulation launch plan

phoxal-cli pull                   # refresh the artifact catalog cache + host tools
phoxal-cli outdated               # report cached artifacts with newer remote digests
phoxal-cli deploy build           # write an immutable, digest-pinned deployment artifact
```

| Command | What it does |
|---|---|
| `robot new <name>` | Scaffold a D5 robot project (`robot.yaml`, `structure.urdf`, default world, `runtimes/`). New manifests omit root `api_version`; optional `phoxal_artifacts.generation` pins the target generation. |
| `check` | Resolve `robot.yaml`, then run each participant's `emit-apis` and fail if participants sharing a contract disagree on its `schema_id` (wire shape) or the producer/consumer topology is unsatisfied. Mixed participant `api_version`s are allowed as long as shared contracts' `schema_id`s agree. Official artifact readiness comes from the generated catalog; git component commits resolve live unless pinned to a commit SHA in `robot.yaml`. `--pull` refreshes the catalog and host tools first; `--service <name>` scopes the build to one user service. |
| `generations status` | Report readiness for a catalog generation on the robot target, including changed contracts and per-target artifact status. Use `--generation <g>` to inspect a specific generation. |
| `simulate <world>` | Resolve the robot and report the host-native simulation launch plan without writing a local launch directory. |
| `service add\|run\|catalog` | Scaffold a user service, run one host-native, or print official services from the configured artifact catalog. |
| `pull` / `outdated` | Refresh, or report drift in, cached artifact metadata and host tools for the selected `(target_generation, channel)`. |
| `deploy build` | Reserved for the native systemd release artifact. |
| `validate` | Lower-level `robot.yaml` structure and user-service phoxal-dependency checks that back `check`. |
| `doctor` | Check host prerequisites (Docker, Webots) without changing anything. |
| `self upgrade` | Update the CLI binary itself. |

Commands that emit machine-readable state accept `--message-format human|json`.

## Artifact Catalog

`phoxal-cli` consumes the framework-generated `phoxal.artifact-catalog/v0` JSON catalog. Local development and tests use `--catalog <path>`, `PHOXAL_ARTIFACT_CATALOG=<path>`, or `phoxal_artifacts.catalog` in `robot.yaml`; local paths are read directly and verified on every run. HTTPS catalog sources and the future default stable URL shape, `https://catalog.phoxal.com/artifact-catalog/v0/stable/latest.json`, are cached at `~/.phoxal/cache/catalog/phoxal-artifact-catalog.json`.

Without `--pull`, commands use a verified local override or the last verified cache entry. `--pull` is the explicit refresh boundary. There are no published catalog revisions or native release assets yet, so commands that require the public catalog fail with the native-pending diagnostic unless you point them at a generated catalog, for example framework `cargo xtask catalog generate --metadata-only` output.

> `v0` is pre-stable: artifacts built at different times may not interoperate.
> Pin digests with `phoxal-cli deploy build`.

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
3. `~/.phoxal/worlds/<world>.wbt` (shared worlds across robots)

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
scripts/live-simulate-gate.sh --live     # full live run (needs Docker daemon + Webots)
```

The smoke phase runs `simulate default --dry-run` to resolve and report the
planned local launch without writing `.phoxal/run` or a release directory. It
needs no Docker daemon. The `--live` phase additionally requires Webots on
`PATH`, then runs `simulate default --pull` so you can confirm the router,
Webots, host tools, and bus connectivity. Until native release assets publish,
official-service launch failures should surface as catalog or native-pending
diagnostics rather than as missing static catalog entries.

## Host layout

```text
~/.phoxal/cache/                    GitHub releases, component clones, downloaded tools - shared across projects.
~/.phoxal/cache/catalog/            Verified generated artifact catalog cache.
~/.phoxal/worlds/                   Optional fallback for shared world files (see Simulate above).
~/.phoxal/config.yaml               Optional. Today only `zenoh_image: <ref>` replaces the compiled default.

<project>/.phoxal/                  Reserved for caches and future generated simulation assets.
<project>/.phoxal/cache/state.yaml  Per-project process lifecycle ledger.
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
