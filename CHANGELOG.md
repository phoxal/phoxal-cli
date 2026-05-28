# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.0-dev](https://github.com/phoxal/phoxal-cli/releases/tag/v0.0.0-dev) - 2026-05-28

### Added

- *(phoxal-cli)* fetch framework releases over HTTP; cache 1h; drop hardcoded list
- *(cli)* offline by default; --pin-digests opts into Docker resolution
- *(phoxal-cli)* add `update` subcommand and `simulate --dry-run` flag
- *(phoxal-cli)* resolver + v1 commands (validate, simulate, doctor, create)
- *(utils-robot)* single Robot struct + new robot.yaml schema
- integrate ORB-SLAM3 backend with robot-localize runtime

### Other

- *(release)* allow release-plz to track the binary as publishable
- *(release)* explicit per-package release tracking for the binary-only repo
- *(release)* set release = true so release-plz tracks the publish=false workspace
- inline plan_robot.yaml fixture so CI doesn't need sibling framework checkout
- *(version)* workspace → 0.0.0-dev (align with framework)
- wire release-plz + cargo-dist binary release
- *(deps)* track framework renames (drop utils- prefix)
- *(phoxal-cli)* delete orphan command modules + phoxal-cli-webots
- *(deps)* rewire engine/runtimes git deps to phoxal/framework
- ignore target/ and editor cruft
- bootstrap phoxal-cli workspace
- *(release)* configure release-plz + cargo-dist + publish flags (dry-run)
- *(api)* introduce pub mod v1 in every phoxal-*-api crate
- *(engine)* fold phoxal-utils-conventions into phoxal-engine
- *(workspace)* carve members into future-repo subdirs
- *(license)* switch workspace to AGPL-3.0-only
- Add MIT license

## [0.1.0](https://github.com/phoxal/engine/releases/tag/v0.1.0) - 2026-03-14

### Added

- *(xtask)* add `simulator webots` command to orchestrate Webots and dev host
- Introduce `xtask` for project orchestration, including Docker Compose generation, and add new BNO085 and DDSM115 drivers.
- *(drive)* implement Stage 2 enhancements and update integration plans

### Fixed

- *(xtask)* resolve unused imports, dead code and module inception warnings

### Other

- xtask uses tmp files
- xtask uses tmp files
- Refactor to robot_model and add module exclusion features
- Robot-Model Provisioning Plan
- webots-proto
- models!!!
- improvements
- improvements
- improvements
- improvements
- improvements
- improve
- improve
- improve
- wip
- move webots-proto simplification plan into its own directory
- wip
- wip
- wip
- wip
- wip
- improvements!
- improvements!
- *(xtask)* simplify console output by streaming commands without spinners
- improvements!
- wip
- improvements!
- improvements!
- improvements!
- improvements!
- Merge branch 'master' into xtask-improvements-14262231103928999666
- Refactor xtask commands and CLI arguments
- *(xtask)* simplify option combinators and fix unwrap panic
- Use `rust-ini` for generating `systemd` config templates and clean up `ArtifactPlan` in `xtask`
- *(xtask)* remove deploy metadata generation from bundle command
- improvements!
- improvements!
- Merge pull request #689 from jBernavaPrah/feature-simulator-webots-command-8839701334612325762
- improvements!
- manifest + xtask
- manifest + xtask
- building
- xtask
- xtask
- wip
- wip
- wip
- added systemd with steroids
- Dynamically process runtime services in xtask systemd
- Remove robot-runtime-launcher crate completely
- *(xtask)* remove service_arg_overrides and release mode args
- standardize xtask robot and simulator arguments and remove manifest flag
- remove remaining instances of 'robot-cleaner'
- wip
- xtask
- xtask
- wip
- wip
- Split xtask dev backends and robustify abstractions
- Complete requested refactor for xtask and drivers
- rename robot-binary to robot-runtime and implement dynamic discovery
- Simplify run_cargo_build_in_docker to build directly to target dir
- Refactor xtask crate: centralize docker commands, use cargo metadata, use rust-ini
- Refactor xtask binary discovery to use standard paths
- wip
- wip
- wip
- github workflow
- simulation part is almost done!!
- simulation part is almost done!!
- simulation part is almost done!!
- simulation part is almost done!!
- simulation part is almost done!!
- simulation part is almost done!!
- simulation part is almost done!!
- rework on the simulation part!
- wip
- wip
- wip
- wip
- wip
- improvements
- wip
- wip
- wip
- wip
- switch to docker-first dev and simulator flow
- wip
- simplify xtask compose modes
- Refactored manifest handling and args mapping: centralized model loading, prepared manifest workflow, replaced driver_services with runtime_services, and improved docker-compose operations
- Refactored launcher image naming to be model-independent; updated related tests and README; adjusted repository prefix checks in clean command.
- Added support for transmission configuration: direction sign and gear ratio for differential drive and odometry systems. Refactored related implementations and tests accordingly.
- introduced Dockerfile-based builder and runtime images, added 'doctor' xtask for preflight checks
- added Bno085 IMU proto and DDSm115 3D model files
- migrated transform system to modular services: added frame and joint APIs, removed legacy tf components
- added robot-tf service and query API for transform caching and retrieval
- improvements
- improvements
- improvements
- improvements
- Improvements
- Refactor xtask to pass arguments instead of environment variables to services
- Rename `robot/` to `robot-utils/` and update dependencies to `robot-utils-*`
- wip
- Fix formatting
- New plan!
- revert to manifest.yaml files
- Refactor odometry to remove IMU fusion, update plans, and clean up manifests
- xtask!

### Removed

- removed `robot-manifest` crate and migrated its functionality into odometry and other modules. Updated kinematics and profiles to use explicit configurations.
- removed assets from manifest.
- removed assets from manifest.
