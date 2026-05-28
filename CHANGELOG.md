# Changelog

All notable changes documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project follows
[Semantic Versioning](https://semver.org/).

## [0.1.2](https://github.com/phoxal/phoxal-cli/releases/tag/v0.1.2) - 2026-05-28


### Fixed

- *(catalog)* Bump simulator_webots tools to v0.2.0

## [0.1.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.1.1) - 2026-05-28


### Fixed

- *(catalog)* Bump rerun_proxy + joypad to 0.1.0 with phoxal- prefixed assets

## [0.1.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.1.0) - 2026-05-28


### Added

- *(simulate)* Webots staging pipeline + multi-arch + compose name + install.sh

### CI

- *(release)* Replace release-plz with homegrown release-prep PR + matrix release
- *(release)* Keep release-prep body out of PR diff
- *(release-prep)* Skip when Cargo.toml is ahead of last tag (release in flight); cliff ignores 'release:' commits
- *(release)* Drop x86_64-apple-darwin (Intel Mac); Apple Silicon only

### Fixed

- *(release)* Create tag via gh release --target (GITHUB_TOKEN can't git push workflow-touching commits)

## [0.0.0-dev](https://github.com/phoxal/phoxal-cli/releases/tag/v0.0.0-dev) - 2026-05-28


### Added

- Integrate ORB-SLAM3 backend with robot-localize runtime
- *(utils-robot)* Single Robot struct + new robot.yaml schema
- *(phoxal-cli)* Resolver + v1 commands (validate, simulate, doctor, create)
- *(phoxal-cli)* Add `update` subcommand and `simulate --dry-run` flag
- *(cli)* Offline by default; --pin-digests opts into Docker resolution
- *(phoxal-cli)* Fetch framework releases over HTTP; cache 1h; drop hardcoded list

### CI

- Wire release-plz + cargo-dist binary release
- *(release)* Cargo-dist owns the GitHub Release; release-plz only tags

### Other

- *(license)* Switch workspace to AGPL-3.0-only
- Bootstrap phoxal-cli workspace
- Ignore target/ and editor cruft
- *(deps)* Rewire engine/runtimes git deps to phoxal/framework
- *(deps)* Track framework renames (drop utils- prefix)
- *(version)* Workspace → 0.0.0-dev (align with framework)
- Release v0.0.0-dev

### Refactored

- *(workspace)* Carve members into future-repo subdirs
- *(engine)* Fold phoxal-utils-conventions into phoxal-engine
- *(api)* Introduce pub mod v1 in every phoxal-*-api crate
- *(phoxal-cli)* Delete orphan command modules + phoxal-cli-webots

### Tests

- Inline plan_robot.yaml fixture so CI doesn't need sibling framework checkout

