# phoxal-cli

The `phoxal` command-line tool for developing, running, simulating, and
deploying Phoxal robot projects.

Public installation, command, and deployment documentation is published at
<https://phoxal.com>. The command itself is the local reference:

```sh
phoxal --help
```

## Install

```sh
curl -fsSL https://phoxal.com/install.sh | sh
```

## Develop

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Repository tests are deterministic unit checks. Host E2E validation is run
separately against built artifacts and is not part of this repository's Cargo
test suite.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). DCO sign-off is required for every
commit.

## License

AGPL-3.0-only. See [LICENSE](LICENSE) and [COMMERCIAL.md](COMMERCIAL.md).
