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
