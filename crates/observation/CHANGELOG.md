# Changelog

All notable changes documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project follows
[Semantic Versioning](https://semver.org/).

## [0.39.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.39.1) - 2026-08-17



## [0.39.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.39.0) - 2026-08-17


### Added

- Adopt normalized manual intent and the snapshot robot identity ([#329](https://github.com/phoxal/phoxal-cli/pull/329)) [**breaking**]


## [0.38.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.38.0) - 2026-08-16


### Added

- Launch the robot from the CLI and build one bundle for every mode ([#328](https://github.com/phoxal/phoxal-cli/pull/328)) [**breaking**]


## [0.37.7](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.7) - 2026-08-15



## [0.37.6](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.6) - 2026-08-15



## [0.37.5](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.5) - 2026-08-15



## [0.37.4](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.4) - 2026-08-15


### Refactored

- Cut CLI over to framework supervisor ([#318](https://github.com/phoxal/phoxal-cli/pull/318))


## [0.37.3](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.3) - 2026-08-13



## [0.37.2](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.2) - 2026-08-11


### Added

- Phoxal-client owns the external robot boundary and deployment drops the product-version gate ([#314](https://github.com/phoxal/phoxal-cli/pull/314))


## [0.37.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.1) - 2026-08-11



## [0.37.0](https://github.com/phoxal/phoxal-cli/releases/tag/v0.37.0) - 2026-08-11


### Added

- Adopt the framework wire families and gate attachment on exact framework equality ([#303](https://github.com/phoxal/phoxal-cli/pull/303)) [**breaking**]

### Refactored

- Remove adversarial hardening from the staging and client paths ([#305](https://github.com/phoxal/phoxal-cli/pull/305))


## [0.36.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.36.1) - 2026-08-10


### Added

- *(ui)* Rebuild attachment console with tui-realm ([#243](https://github.com/phoxal/phoxal-cli/pull/243))
- *(ui)* Remove the Bus page and the device and clock feeds ([#285](https://github.com/phoxal/phoxal-cli/pull/285)) [**breaking**]
- Read the joypad locally and delete the tool concept ([#290](https://github.com/phoxal/phoxal-cli/pull/290)) [**breaking**]
- Split phoxal from phoxald around finalized bundles and execution-scoped Zenoh ([#297](https://github.com/phoxal/phoxal-cli/pull/297)) [**breaking**]

### Fixed

- Harden attachment lifecycle resilience ([#247](https://github.com/phoxal/phoxal-cli/pull/247))
- Carry the resident's real failure reason to the client terminal ([#252](https://github.com/phoxal/phoxal-cli/pull/252))
- Restore private workspace releases ([#301](https://github.com/phoxal/phoxal-cli/pull/301))

### Refactored

- Establish layered CLI ownership boundaries
- *(supervisor)* Extract resident runtime authority
- *(client)* Decompose attachment runtime
- *(cli)* Complete layered boundary cleanup ([#245](https://github.com/phoxal/phoxal-cli/pull/245))
- *(cli)* Remove obsolete executable plan store [**breaking**]
- Reconcile CLI runtime ownership ([#299](https://github.com/phoxal/phoxal-cli/pull/299))

