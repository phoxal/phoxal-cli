# Changelog

All notable changes documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project follows
[Semantic Versioning](https://semver.org/).

## [0.42.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.42.1) - 2026-08-22



## [0.42.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.42.0) - 2026-08-20


### Added

- Validate driver blocks and every declared service config against the owning binary ([#343](https://github.com/phoxal/phoxal-cli/pull/343)) [**breaking**]


## [0.41.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.41.0) - 2026-08-19


### Refactored

- Consume the framework as one `phoxal` library and attach through `phoxal::session` ([#341](https://github.com/phoxal/phoxal-cli/pull/341)) [**breaking**]


## [0.40.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.40.0) - 2026-08-17


### Added

- Follow the 0.65 train, ask the supervisor which robot it runs, and delete what the campaign left behind ([#339](https://github.com/phoxal/phoxal-cli/pull/339)) [**breaking**]


## [0.39.4](https://github.com/phoxal/phoxal-cli/releases/tag/v0.39.4) - 2026-08-17



## [0.39.3](https://github.com/phoxal/phoxal-cli/releases/tag/v0.39.3) - 2026-08-17



## [0.39.2](https://github.com/phoxal/phoxal-cli/releases/tag/v0.39.2) - 2026-08-17



## [0.39.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.39.1) - 2026-08-17



## [0.39.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.39.0) - 2026-08-17


### Other

- Update Cargo.toml dependencies


## [0.38.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.38.0) - 2026-08-16


### Added

- Restore the startup welcome and make simulation session endings legible ([#326](https://github.com/phoxal/phoxal-cli/pull/326))
- Launch the robot from the CLI and build one bundle for every mode ([#328](https://github.com/phoxal/phoxal-cli/pull/328)) [**breaking**]


## [0.37.7](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.7) - 2026-08-15


### Fixed

- Isolate registry package compiler outputs ([#324](https://github.com/phoxal/phoxal-cli/pull/324))


## [0.37.6](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.6) - 2026-08-15



## [0.37.5](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.5) - 2026-08-15


### Fixed

- Strip physical drivers from simulation ([#320](https://github.com/phoxal/phoxal-cli/pull/320))


## [0.37.4](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.4) - 2026-08-15


### Refactored

- Cut CLI over to framework supervisor ([#318](https://github.com/phoxal/phoxal-cli/pull/318))


## [0.37.3](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.3) - 2026-08-13


### Refactored

- Finish CLI internal ownership cleanup ([#316](https://github.com/phoxal/phoxal-cli/pull/316))


## [0.37.2](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.2) - 2026-08-11


### Added

- Make the robot project the framework authority for build validation ([#312](https://github.com/phoxal/phoxal-cli/pull/312))
- Deployment releases own the executor and the bundle ([#313](https://github.com/phoxal/phoxal-cli/pull/313))


## [0.37.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.1) - 2026-08-11


### Fixed

- Derive simulation membership from the compiled robot ([#308](https://github.com/phoxal/phoxal-cli/pull/308))


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

