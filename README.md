# phoxal-cli

Consumer CLI for the Phoxal robot framework. Owns the resolver, `robot.yaml` discovery, `phoxal.sources.lock` generation, and the `validate` / `simulate` / `doctor` commands.

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
phoxal simulate <world>
```

`<world>` resolves to a `.wbt` file in this order:

1. `<project>/worlds/<world>.wbt`
2. `<project>/<world>` (path-as-given, e.g. `worlds/foo.wbt`)
3. `~/.phoxal/worlds/<world>.wbt` (shared worlds across robots)

Example: `phoxal simulate default` finds `worlds/default.wbt` in the project.

## Live split-recovery gate

`scripts/live-simulate-gate.sh` is the documented gate that proves the
separated repos run together end to end — `robot.yaml` → `phoxal-cli` → a real
`phoxal.sources.lock` (real GHCR digests) → generated `.phoxal/run/` → router → Webots
→ the mandatory API/channel runtime set. It is the live counterpart to recovery Gates 8–9
in `phoxal/organization`'s `REPOSITORIES.md`, and depends on real image digests
(#10) and a published runtime image set (`phoxal/framework#31`).

```sh
# from the phoxal-cli checkout; ROBOT_DIR defaults to ../robot-v1
scripts/live-simulate-gate.sh            # smoke: real-digest lock + locked compose (CI-safe)
scripts/live-simulate-gate.sh --live     # full live run (needs Docker daemon + Webots)
```

The smoke phase runs `update --pin-digests`, asserts every runtime image in
`phoxal.sources.lock` records a selected `image_ref` plus a real `sha256:…` digest, and
runs `simulate default --locked --dry-run` to verify locked resolution and that
the generated compose references those digest pins. It needs only
`docker buildx` (no daemon). The `--live` phase additionally requires a running
Docker daemon and Webots on `PATH`, then runs `simulate default --locked` so you
can confirm the router, the GHCR runtime services, Webots, and bus connectivity
(`tcp/router:7447`, host tools via `tcp/127.0.0.1:7447`). Failures are reported
with the actionable cause (missing image/digest, Docker not running, Webots
missing, runtime startup, or bus connection).

## Host layout

```text
~/.phoxal/cache/                    GitHub releases, component clones, downloaded tools - shared across projects.
~/.phoxal/worlds/                   Optional fallback for shared world files (see Simulate above).
~/.phoxal/config.yaml               Optional. Today only `zenoh_image: <ref>` replaces the compiled default.

<project>/.phoxal/run/              Generated compose + staged robot view (regenerated each simulate).
<project>/.phoxal/webots/           Generated Webots controllers + protos.
<project>/.phoxal/cache/state.yaml  Per-project process lifecycle ledger.
```

## License

AGPL-3.0-only — see [LICENSE](LICENSE) for the full license text.
A commercial license is available for downstream products that cannot
comply with AGPL — see [COMMERCIAL.md](COMMERCIAL.md) and reach out via
<https://phoxal.com>.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). DCO sign-off required on every
commit.

## Phoxal

- <https://phoxal.com>
- <https://github.com/phoxal>
