# phoxal-cli

The `phoxal` command-line tool for developing, running, simulating, and
deploying Phoxal robot projects.

Public installation, command, and deployment documentation is published at
<https://phoxal.com>. The command itself is the local reference:

```sh
phoxal --help
```

## The root brain

Every robot project's root Cargo package IS its one mandatory brain: a
non-published workspace member that depends on `phoxal`, has exactly one
binary target, and has no library target. The minimal root source is:

```rust
// src/main.rs
#[phoxal::brain]
struct Brain;

fn main() -> phoxal::Result<()> {
    phoxal::run::<Brain>()
}
```

The CLI discovers it from Cargo metadata, always builds it, stages it
canonically as `bin/brain`, and launches it in every native and Webots graph.
It is never declared under `robot.yaml` `services:` - `brain` is a reserved
identity there. A project whose root is still a code-less `src/lib.rs` anchor
is rejected with the exact migration instruction.

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
