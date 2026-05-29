# phoxal-cli

Consumer CLI for the Phoxal robot framework. Owns the resolver, `robot.yaml` discovery, `phoxal.lock` generation, and the `validate` / `simulate` / `doctor` / `create` commands.

## Install

```sh
curl -sSfL https://raw.githubusercontent.com/phoxal/phoxal-cli/main/install.sh | sh
```

To pin a release, set `PHOXAL_CLI_VERSION` to a tag:

```sh
PHOXAL_CLI_VERSION=v0.0.1 sh install.sh
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

## Host layout

```text
~/.phoxal/cache/                    GitHub releases, component clones, downloaded tools - shared across projects.
~/.phoxal/worlds/                   Optional fallback for shared world files (see Simulate above).
~/.phoxal/config.yaml               Optional. Today only `zenoh_image: <ref>` overrides the compiled default.

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
