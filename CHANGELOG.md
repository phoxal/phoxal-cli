# Changelog

All notable changes documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project follows
[Semantic Versioning](https://semver.org/).

## [0.28.3](https://github.com/phoxal/phoxal-cli/releases/tag/v0.28.3) - 2026-07-28


### Added

- Cut over resident protocol and force stop


## [0.28.2](https://github.com/phoxal/phoxal-cli/releases/tag/v0.28.2) - 2026-07-28


### Refactored

- Establish layered CLI ownership boundaries


## [0.28.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.28.1) - 2026-07-28


### Other

- Consume framework v0.43.2 ([#231](https://github.com/phoxal/phoxal-cli/pull/231))


## [0.28.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.28.0) - 2026-07-28


### Refactored

- Align with participant authoring train ([#229](https://github.com/phoxal/phoxal-cli/pull/229)) [**breaking**]


## [0.27.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.27.1) - 2026-07-28


### Refactored

- *(build)* Batch official Cargo installs ([#226](https://github.com/phoxal/phoxal-cli/pull/226))


## [0.27.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.27.0) - 2026-07-28


### Fixed

- *(build)* Persist the container Cargo target cache ([#225](https://github.com/phoxal/phoxal-cli/pull/225))

### Refactored

- Finish Cargo-native runtime staging ([#223](https://github.com/phoxal/phoxal-cli/pull/223)) [**breaking**]


## [0.26.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.26.0) - 2026-07-27


### Added

- *(validate)* Check user config against its participant's schema ([#220](https://github.com/phoxal/phoxal-cli/pull/220))

### Fixed

- *(simulate)* Validate the real check outcome, not a defaulted report ([#216](https://github.com/phoxal/phoxal-cli/pull/216))
- *(build)* Install the system libraries officials need to compile ([#221](https://github.com/phoxal/phoxal-cli/pull/221))
- *(run)* Gate the startup stage on a real router connect, not a Zenoh session ([#222](https://github.com/phoxal/phoxal-cli/pull/222))

### Other

- *(deps)* Raise MSRV to 1.88 and refresh the lockfile ([#215](https://github.com/phoxal/phoxal-cli/pull/215))

### Refactored

- Simplify participant metadata consumers ([#214](https://github.com/phoxal/phoxal-cli/pull/214)) [**breaking**]
- Delete dead code ([#217](https://github.com/phoxal/phoxal-cli/pull/217))
- Delete the phoxal check command, keep the validation engine ([#218](https://github.com/phoxal/phoxal-cli/pull/218))
- Materialize official runtimes through Cargo ([#219](https://github.com/phoxal/phoxal-cli/pull/219)) [**breaking**]

### Tests

- Keep repository coverage to unit contracts ([#212](https://github.com/phoxal/phoxal-cli/pull/212))


## [0.25.2](https://github.com/phoxal/phoxal-cli/releases/tag/v0.25.2) - 2026-07-26


### Tests

- *(core)* Remove integration test machinery
- *(cli)* Remove integration test machinery ([#211](https://github.com/phoxal/phoxal-cli/pull/211))


## [0.25.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.25.1) - 2026-07-26


### Refactored

- *(router)* Probe Zenoh readiness from the CLI ([#207](https://github.com/phoxal/phoxal-cli/pull/207))


## [0.25.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.25.0) - 2026-07-26


### Added

- *(cli)* Adopt execution-scoped time and command model ([#205](https://github.com/phoxal/phoxal-cli/pull/205)) [**breaking**]


## [0.24.7](https://github.com/phoxal/phoxal-cli/releases/tag/v0.24.7) - 2026-07-25


### Refactored

- *(simulation)* Simplify Webots source runs ([#203](https://github.com/phoxal/phoxal-cli/pull/203))


## [0.24.6](https://github.com/phoxal/phoxal-cli/releases/tag/v0.24.6) - 2026-07-25


### Refactored

- *(suite)* Accept artifact-only phoxal.suite/v0, drop profile surface ([#201](https://github.com/phoxal/phoxal-cli/pull/201))


## [0.24.5](https://github.com/phoxal/phoxal-cli/releases/tag/v0.24.5) - 2026-07-25


### Fixed

- *(supervisor)* Scrub guardian systemd environment ([#199](https://github.com/phoxal/phoxal-cli/pull/199))


## [0.24.4](https://github.com/phoxal/phoxal-cli/releases/tag/v0.24.4) - 2026-07-25


### Fixed

- *(deploy)* Converge installed runtime hygiene ([#197](https://github.com/phoxal/phoxal-cli/pull/197))


## [0.24.3](https://github.com/phoxal/phoxal-cli/releases/tag/v0.24.3) - 2026-07-25


### Fixed

- *(runtime)* Support reference robot graph capacity ([#195](https://github.com/phoxal/phoxal-cli/pull/195))


## [0.24.2](https://github.com/phoxal/phoxal-cli/releases/tag/v0.24.2) - 2026-07-25


### Fixed

- *(deploy)* Preserve artifact activation links ([#193](https://github.com/phoxal/phoxal-cli/pull/193))


## [0.24.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.24.1) - 2026-07-25


### Fixed

- *(build)* Omit simulators from native bundles ([#191](https://github.com/phoxal/phoxal-cli/pull/191))


## [0.24.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.24.0) - 2026-07-25


### Added

- Install immutable runtime releases ([#189](https://github.com/phoxal/phoxal-cli/pull/189)) [**breaking**]


## [0.23.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.23.0) - 2026-07-24


### Added

- Compile source projects into flat runtime roots and build.phoxal (phase 10) ([#186](https://github.com/phoxal/phoxal-cli/pull/186)) [**breaking**]
- Declaration-driven user services and tools (phase 10B) ([#188](https://github.com/phoxal/phoxal-cli/pull/188)) [**breaking**]


## [0.22.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.22.1) - 2026-07-24


### Added

- *(runtime)* Adopt Cargo workspace model ([#184](https://github.com/phoxal/phoxal-cli/pull/184))


## [0.22.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.22.0) - 2026-07-23


### Added

- *(supervisor)* Add resident attachable sessions ([#180](https://github.com/phoxal/phoxal-cli/pull/180)) [**breaking**]
- *(suite)* Consume versioned launch profiles ([#183](https://github.com/phoxal/phoxal-cli/pull/183)) [**breaking**]

### Fixed

- *(supervisor)* Preserve guardian descriptors across exec ([#182](https://github.com/phoxal/phoxal-cli/pull/182))


## [0.21.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.21.1) - 2026-07-23


### Fixed

- *(release)* Converge release automation


## [0.21.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.21.0) - 2026-07-23


### Added

- *(supervisor)* Add immutable managed execution plans [**breaking**]

### Fixed

- *(release)* Restore release-plz-managed CLI publication ([#175](https://github.com/phoxal/phoxal-cli/pull/175)) [**breaking**]

### Other

- Drop stale multi-version catalog remnants ([#173](https://github.com/phoxal/phoxal-cli/pull/173))

### Refactored

- Harden supervisor state and readiness


## [0.20.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.20.1) - 2026-07-22


### Fixed

- Keep vendored artifacts on locked train (#171)

## [0.20.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.20.0) - 2026-07-22


### Added

- Select immutable framework suites by Cargo.lock (#168)

### Fixed

- Run doctor registry probe off async runtime (#170)

## [0.19.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.19.0) - 2026-07-20


### Added

- *(telemetry)* Add retained robot diagnostics (#166)

## [0.18.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.18.0) - 2026-07-20


### Added

- *(supervisor)* Recover foreground process graphs (#164)

### Refactored

- *(cli)* Remove alternate output modes
- *(cli)* Establish core and UI crate boundaries (#161)
- Complete CLI crate reorganization (#162)
- *(supervisor)* Consume Zenoh Liveliness

## [0.17.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.17.0) - 2026-07-19


### Added

- Consume infrastructure router (#158)

## [0.16.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.16.1) - 2026-07-19


### Fixed

- *(tui)* Simplify joypad motion status (#156)

## [0.16.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.16.0) - 2026-07-18


### Added

- *(tui)* Refine session interface and navigation (#154)

## [0.15.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.15.0) - 2026-07-17


### Added

- *(tui)* Redesign sessions around robot development (#152)

## [0.14.2](https://github.com/phoxal/phoxal-cli/releases/tag/v0.14.2) - 2026-07-17


### Fixed

- *(simulation)* Respect Webots-owned lifecycle (#150)

## [0.14.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.14.1) - 2026-07-16


### Refactored

- *(runtime)* Keep site tools clockless (#148)

## [0.14.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.14.0) - 2026-07-15


### Added

- Simplify CLI runtime state (#146)

## [0.13.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.13.1) - 2026-07-15


### Fixed

- *(deps)* Restore published framework version pins + bump time (#144)

## [0.13.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.13.0) - 2026-07-15


### Added

- Adopt the simplified service topology (#142) [**breaking**]

### Refactored

- *(api)* Consume stable v1 and preview v2 (#141)

## [0.12.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.12.0) - 2026-07-14


### Added

- *(update)* Redesign artifact progress (#138)

## [0.11.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.11.1) - 2026-07-13


### Fixed

- *(tui)* Polish simulation interaction (#134)

## [0.11.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.11.0) - 2026-07-13


### Added

- *(simulation)* Finish clock and diagnostics UX (#131)

## [0.10.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.10.0) - 2026-07-13


### Added

- *(cli-ux)* Event-driven session core (follow-up refactor) (#128)

## [0.9.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.9.0) - 2026-07-13


### Added

- *(simulate)* Serve the robot spawn set over the bus, drop spawn from --config (#120)
- *(simulate)* Observed-readiness barrier + failure propagation (#122)
- *(cli-ux)* Branded operational console - theme, TUI, staged startup, live telemetry (#125)

### Fixed

- *(simulate)* Make live Webots simulation work end to end (#116)
- *(simulate)* Clean Webots shutdown, native IMPORTABLE, in-project base pins (#118)
- *(simulate)* Fail-fast on terminal graph failure; use framework spawn contracts; atomic state write (#123)
- *(run/deploy)* Don't pass PHOXAL_CONFIG to configless tools; give telemetry PHOXAL_CONNECT (#126)

### Refactored

- *(artifacts)* Version-atomic store, self-cleaning lock, retire target-independent (#119)

### Tests

- *(output-mode)* Derive cli_version in json baseline from CARGO_PKG_VERSION (#127)

## [0.8.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.8.1) - 2026-07-11


### Fixed

- *(resolver)* Component_assets optional for driverless components (#113)
- *(resolver)* Read official component-driver binary as phoxal-component-<id> (#115)

## [0.8.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.8.0) - 2026-07-11


### Added

- *(deploy)* Phoxal-deploy group grant instead of per-user sudoers (#110) [**breaking**]
- *(check)* Validate user-runtime config against its real emitted schema (W7) [hold: needs slotted catalog] (#109)

### Refactored

- *(store)* Filesystem-safe .phoxal package dirs (phoxal-x, not phoxal%2Fx) (#111)

## [0.7.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.7.0) - 2026-07-11


### Added

- Repoint joypad tool catalog to the framework release train (#59)
- *(cli)* Build against framework 0.11.0 (#61)
- *(cli)* Build against framework 0.12.0 (robot.yaml schema:v0 + api_version) (#62)
- *(cli)* Add the `phoxal check` graph validation engine (pure core) (#63)
- *(cli)* Migrate runtime resolution to api-version + channel (#64) [**breaking**]
- *(cli)* Add the `phoxal check` command (#65)
- *(cli)* Add `phoxal runtime add` (scaffold a user runtime) (#66)
- *(cli)* Add `phoxal runtime run` (host-native user runtime) + bump to phoxal 0.15 (#67)
- *(cli)* Check user runtimes by building and inspecting them (#69)
- *(cli)* Check component drivers from source (git/path) in `phoxal check` (#70)
- *(cli)* Add `phoxal deploy build` + migrate compose to the PHOXAL_* launch env (#71)
- *(cli)* Catalog all 18 official runtimes on a single y2026_1 (#72)
- Reconcile CLI to the rewrite spec + doc polish (plan-vs-code audit) (#73)
- *(16)* Refine framework integration for 0.20 (phoxal-cli) (#76)
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
- Rework runtime metadata on the new phoxal-api model (#103) [**breaking**]
- *(check)* Apply validation at check/deploy/run/simulate (W6) (#105)
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
- *(gate)* Add live split-recovery gate for the simulation default (#11)

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
