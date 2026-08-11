# Changelog

All notable changes documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project follows
[Semantic Versioning](https://semver.org/).

## [0.37.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.0) - 2026-08-11


### Added

- Adopt the framework wire families and gate attachment on exact framework equality ([#303](https://github.com/phoxal/phoxal-cli/pull/303)) [**breaking**]
- Accept same-line framework trains at attach and bundle admission ([#307](https://github.com/phoxal/phoxal-cli/pull/307)) [**breaking**]

### Refactored

- Remove adversarial hardening from the staging and client paths ([#305](https://github.com/phoxal/phoxal-cli/pull/305))


## [0.36.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.36.1) - 2026-08-10


### Added

- Gate dashboard on startup readiness
- *(build)* Derive managed builder images from runtimes ([#267](https://github.com/phoxal/phoxal-cli/pull/267))
- Batch runtime preparation and progress ([#271](https://github.com/phoxal/phoxal-cli/pull/271))
- *(project)* Resolve component assets directly
- *(supervisor)* Collect participant logs in-process, and fix a shipped catalog regression ([#287](https://github.com/phoxal/phoxal-cli/pull/287)) [**breaking**]
- *(supervisor)* Retain runtime telemetry in-process, under a serve module ([#289](https://github.com/phoxal/phoxal-cli/pull/289)) [**breaking**]
- Read the joypad locally and delete the tool concept ([#290](https://github.com/phoxal/phoxal-cli/pull/290)) [**breaking**]
- Build, stage, and launch the mandatory root brain; drop behavior handling ([#293](https://github.com/phoxal/phoxal-cli/pull/293)) [**breaking**]
- Split phoxal from phoxald around finalized bundles and execution-scoped Zenoh ([#297](https://github.com/phoxal/phoxal-cli/pull/297)) [**breaking**]

### Fixed

- *(build)* Compile container source once ([#259](https://github.com/phoxal/phoxal-cli/pull/259))
- *(simulation)* Create the staged mesh root ([#261](https://github.com/phoxal/phoxal-cli/pull/261))
- Make catalog floor follow framework train ([#263](https://github.com/phoxal/phoxal-cli/pull/263))
- Correct the socket path limit and retire train-anchor wording ([#295](https://github.com/phoxal/phoxal-cli/pull/295))
- Enforce canonical project phase paths ([#300](https://github.com/phoxal/phoxal-cli/pull/300))
- Restore private workspace releases ([#301](https://github.com/phoxal/phoxal-cli/pull/301))

### Refactored

- Establish layered CLI ownership boundaries
- Extract project preparation crate ([#237](https://github.com/phoxal/phoxal-cli/pull/237))
- *(supervisor)* Extract resident runtime authority
- *(cli)* Complete layered boundary cleanup ([#245](https://github.com/phoxal/phoxal-cli/pull/245))
- Consume canonical framework model ([#254](https://github.com/phoxal/phoxal-cli/pull/254))
- Consume compiled framework project output ([#258](https://github.com/phoxal/phoxal-cli/pull/258))
- *(cli)* Remove obsolete executable plan store [**breaking**]
- *(supervisor)* Run the Zenoh router inside the supervisor process ([#279](https://github.com/phoxal/phoxal-cli/pull/279)) [**breaking**]
- Reconcile CLI runtime ownership ([#299](https://github.com/phoxal/phoxal-cli/pull/299))

