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

Every official package - the services, the component drivers, and
`phoxal-supervisor` - is normally materialized from the `phoxal` registry at the
exact train the robot project locked. A train that is still being written is not
installable, so two environment variables redirect that materialization at a
local checkout instead:

```sh
export PHOXAL_FRAMEWORK_PATH=/path/to/framework
export PHOXAL_SIMULATOR_WEBOTS_PATH=/path/to/simulator-webots
phoxal run
```

`PHOXAL_FRAMEWORK_PATH` builds every official package with
`cargo install --path <checkout>/<crate>` (`services/<name>`,
`components/<name>`, `supervisor`). `PHOXAL_SIMULATOR_WEBOTS_PATH` does the same
for the Webots controller, which lives in its own repository on its own release
train. Everything else - caching, staging, validation - is unchanged, and
`phoxal doctor` reports whichever of the two is set so nobody has to guess which
binaries a robot is running.

These are development aids. A robot project still pins its framework train in
`Cargo.toml`, and a release built with an override is not a release anybody
should install.

## License

AGPL-3.0-only. See [LICENSE](LICENSE) and [COMMERCIAL.md](COMMERCIAL.md).
