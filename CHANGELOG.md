# Changelog

All notable changes documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project follows
[Semantic Versioning](https://semver.org/).

## [0.8.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.8.0) - 2026-07-11


### Added

- *(deploy)* Phoxal-deploy group grant instead of per-user sudoers (#110) [**breaking**]
- *(check)* Validate user-runtime config against its real emitted JSON Schema (#109)

### Refactored

- *(store)* Filesystem-safe .phoxal package dirs (phoxal-x, not phoxal%2Fx) (#111)

## [0.7.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.7.0) - 2026-07-11


### Added

- Repoint joypad tool catalog to the framework release train (#59)
- *(cli)* Build against framework 0.11.0 (#61)
- *(cli)* Build against framework 0.12.0 (robot.yaml schema:v0 + api_version) (#62)
- *(cli)* Add the `phoxal check` graph validation engine (pure core) (#63)
- *(cli)* Migrate runtime resolution to api-version + channel (#64) [**breaking**]
- *(cli)* Add the `phoxal check` command (emit-apis graph validation) (#65)
- *(cli)* Add `phoxal runtime add` (scaffold a user runtime) (#66)
- *(cli)* Add `phoxal runtime run` (host-native user runtime) + bump to phoxal 0.15 (#67)
- *(cli)* Check user runtimes by building them + running emit-apis (#69)
- *(cli)* Check component drivers from source (git/path) in `phoxal check` (#70)
- *(cli)* Add `phoxal deploy build` + migrate compose to the PHOXAL_* launch env (#71)
- *(cli)* Catalog all 18 official runtimes on a single y2026_1 (#72)
- Reconcile CLI to the rewrite spec + doc polish (plan-vs-code audit) (#73)
- *(16)* Per-contract schema_id check + 0.20 catch-up (phoxal-cli) (#76)
- *(06,16)* Catalog consumption, D5 resolution, lifecycle diagnostics (#80)
- *(18)* The typed LaunchPlan + the shared PHOXAL_* env encoder (#81)
- *(04)* The host-native supervisor - run engine, board, bus logs (#82)
- *(10)* Simulate mounts the supervisor with contract substitution (#83)
- *(19)* --watch hot reload + path overrides (#84)
- *(03)* The single deploy verb - probe, cross-build, render, sync, health (#85)
- *(11)* Catalog activation - tools/simulators via catalog, stopgaps removed (#86)
- *(12)* --message-format json for version and self upgrade (#88)
- *(13)* Check --service self-sufficient on a cold cache (#89)
- *(simulate)* Launch Webots with a staged world and supervisor-spawned robots (#91)
- *(simulate)* Adapt to the relaxed check + stage/launch Webots correctly (#92)
- *(cli)* Cut over to the five-root-key grammar + package identity (Phase 7 Band B) (#93) [**breaking**]
- *(cli)* Catalog-native fetch for component assets + drivers (Phase 7) (#94) [**breaking**]
- *(cli)* Add --target to check and simulate for cross-target validation (#95)
- Rework off emit-apis onto the new phoxal-api model (#103) [**breaking**]
- *(check)* Coherence gate at check/deploy/run/simulate (W6) (#105)
- *(self)* Update-available banner (W8) (#106)
- *(deploy)* Robot-side download + transactional remote release (W9) (#107)

### Fixed

- *(cli)* Drop driver-ddsm115 from the platform-runtime catalog (#68)
- *(03)* Deploy hardening from the first live robot E2E (Jetson Orin) (#87)

### Other

- Bump to phoxal 0.19 + scaffold uses phoxal_api (plan #00) (#74)
- *(20)* Remove robot new / service add scaffolding commands until v1 (#90)
- Retrigger release-prep after shared-workflow pipefail fix

### Refactored

- Remove the lockfile - resolve live, pin via the deploy bundle (#75)
- *(05,02)* CLI uses shared phoxal::check core + participant_class (#77)
- *(15)* Retire the runtime name - exact kinds, service verb, 0.21 scaffold (#78)
- Remove Docker/Compose/Balena/GHCR paths from phoxal-cli (#79)
- *(catalog)* Read the catalog from the `stable` release, not the git ref (#96) [**breaking**]
- *(simulate)* Stage under ~/.phoxal/run, symlink controllers+meshes, opt-in joypad (#97)
- *(simulate)* Drop substitution checking, keep the sim board display (phoxal 0.28) (#98)
- *(plan)* One LaunchPlan descriptor - LaunchMode::Webots{world}, drop SimulatePlan (WS3) (#99)
- *(catalog)* Consume phoxal 0.29 lean phoxal::catalog, delete the schema mirror + sidecar (WS1b) (#100) [**breaking**]
- *(cache)* Flat tarball store + local manifest + generalize git to any pin (WS2) (#101) [**breaking**]
- Adapt to phoxal 0.30 - absorb check strip + manifest v0 rename (WS5 downstream) (#102) [**breaking**]
- Transition phoxal-cli onto phoxal.catalog/v0 (W1-W5) (#104) [**breaking**]
- Remove the phoxal init command and doctor's gitignore check (#108) [**breaking**]

## [0.6.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.6.0) - 2026-06-16


### Added

- *(cli)* Lock and recipe-build user runtimes (#55)
- *(cli)* Provision Webots simulator binaries on live simulate (#57)

### Fixed

- *(cli)* Widen supported runtime train to any 0.x≥0.8 (#48)
- *(cli)* Validate hard-fails user-runtime framework mismatch (#49)
- *(cli)* Validate platform-runtime override version against releases (#51)
- *(cli)* Simulate no longer silently writes phoxal.lock (#50)

### Refactored

- *(cli)* Derive platform-runtime image repo from name (#52)
- *(cli)* Split simulate.rs into docker_stack + local_build modules (#53)

### Style

- *(cli)* Apply rustfmt to resolver + resolver_basic test (#47)

## [0.5.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.5.1) - 2026-06-16


### Added

- *(cli)* Consume the Webots simulator from the framework release train (#45)

### Fixed

- *(cli)* Make --version and `version` print the same string (#43)

## [0.5.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.5.0) - 2026-06-13


### Added

- Self upgrade, version commands, and a phoxal.com installer (#38)

### Refactored

- *(cli)* Make doctor a read-only host-readiness check (#40) (#42)

## [0.4.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.4.0) - 2026-06-12


### Added

- Decouple CLI version from robot.yaml and pin scaffolds to the runtime train (#34) [**breaking**]

## [0.3.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.3.0) - 2026-06-09


### Added

- *(resolver)* Stage git components from a repository subdirectory (#29)

### CI

- Gate releases on the release/ branch prefix (#28)
- Use the shared rust-ci reusable workflow (#32)

### Refactored

- Depend on single `phoxal` crate (#31)

## [0.2.2](https://github.com/phoxal/phoxal-cli/releases/tag/v0.2.2) - 2026-06-07


### CI

- Gate release on the release-prep branch, not the PR title (#25)

### Fixed

- Make the lockfile-mismatch error in simulate actionable (#26)

## [0.2.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.2.1) - 2026-06-07


### CI

- Adopt shared reusable release workflows (#20)
- Enforce Conventional Commit PR titles (#21)
- Repoint reusable workflows to public phoxal/.github (#22)

### Fixed

- Clarify rustup install hint in doctor (#23)

## [0.2.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.2.0) - 2026-06-07


### Added

- *(compose)* Hand-write router service on upstream zenoh + add local_zenoh module
- *(simulate)* Two-stack bring-up via phoxal-link network + compose --wait readiness
- *(simulate)* World becomes CLI arg; caches move to ~/.phoxal/
- *(gate)* Add live split-recovery gate for robot-v1 simulate default (#11)

### CI

- *(release)* Explicit, retry-safe release tag handling (#19)

### Documentation

- *(README)* Document simulate <world> + host layout under ~/.phoxal/

### Fixed

- *(resolver)* Never enter fake image digests into simulate or phoxal.lock (#10)

### Other

- Adopt phoxal framework renames (infra/api/core/runtime/validation)
- Restore framework branch pin to main after restructure merge
- *(docker)* Point OCI image.source label at phoxal/phoxal-cli

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

