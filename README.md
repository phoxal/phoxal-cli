# phoxal-cli

Rust workspace for `phoxal`, the project and session CLI, and `phoxald`, the
robot execution supervisor.

Phoxal is pre-1.0 and evolving. Installation and command documentation is
published at <https://phoxal.com>; the installed commands also provide local
help.

```sh
phoxal --help
phoxald --help
```

## Develop

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution requirements.

## License

AGPL-3.0-only. See [LICENSE](LICENSE) and [COMMERCIAL.md](COMMERCIAL.md).
