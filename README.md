# phoxal-cli

Rust workspace for `phoxal`, the project, build, deployment, and session CLI.
Robot releases carry the `phoxal-supervisor` from their selected framework
train.

Phoxal is pre-1.0 and evolving.
Use `phoxal --help` for current command usage and this repository for current CLI workflow details.
See <https://phoxal.com> for the project vision and public introduction.

```sh
phoxal --help
```

## Develop

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution requirements.

### Running against an unpublished framework train

Every official package - the services, component drivers, supervisor, and Webots adapter host tools - is normally materialized from the `phoxal` registry at the exact framework train selected by the workflow.
A train that is still being written is not installable, so one environment variable redirects that materialization at the framework checkout instead:

```sh
export PHOXAL_FRAMEWORK_PATH=/path/to/framework
phoxal simulation start /path/to/world.yaml
```

`PHOXAL_FRAMEWORK_PATH` builds every official package with `cargo install --path <checkout>/<crate>` (`services/<name>`, `components/<name>`, `supervisor`, and `simulators/webots/<role>`).
Official component definitions and assets also come from that checkout, so local validation and installed runtimes consume one coherent owner source.
Everything else - exact-train validation, caching, staging, and launch - is unchanged, and `phoxal doctor` reports the override so nobody has to guess which binaries are running.

These are development aids. A robot project still pins its framework train in
`Cargo.toml`, and a release built with an override is not a release anybody
should install.

## License

AGPL-3.0-only. See [LICENSE](LICENSE) and [COMMERCIAL.md](COMMERCIAL.md).
