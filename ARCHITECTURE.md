# phoxal-cli architecture

The workspace has three product crates with one-way dependencies:

```text
phoxal-cli (command dispatch and operating-system adapters)
├── phoxal-cli-ui (terminal interaction and ratatui presentation)
│   └── phoxal-cli-core (terminal-independent records and behavior)
└── phoxal-cli-core
```

The root crate composes the product. UI may consume neutral core records; core
never imports UI, command parsing, terminal libraries, or root-crate modules.
Neither extracted crate may depend back on the root crate.

## Root crate

The root package is the adapter shell. It owns clap dispatch, child processes,
raw bus sessions, SSH and sudo, filesystem mutation, artifact downloads,
Webots process integration, terminal selection, and session-controller
composition. The root supervisor also owns the per-user foreground authority
and router recovery: router loss tears down the CLI-owned process graph,
restarts the same endpoint, clears stale presence, and recreates the staged
graph without replacing the operator's session controller.

The four orchestration commands are thin façades:

- `check`: command flow plus coherence, participant, graph, metadata, and build
  steps.
- `deploy`: command flow plus target, preparation, payload, source-build,
  official-artifact, unit, metadata, release, bootstrap, and SSH/sync steps.
- `run`: command flow plus preparation, participants, stages, router,
  telemetry, reporting, environment, and build steps.
- `simulation`: command flow plus preparation, resolution, participants,
  reporting, stages, Webots, controller, and filesystem-staging steps.

Supervision and native-artifact provisioning use the same named-step layout.
Raw bus authority stays in explicit root adapter modules and is enforced by the
dependency audit.

## Core crate

`phoxal-cli-core` owns reusable facts and behavior:

- `project`: suite fetch/validation, resolver records, checked launch plans,
  and tooling. The immutable suite client and crates.io train-status probe are
  the intentional core network edges; artifact downloads and all runtime bus
  traffic remain root adapters.
- `deploy`: transport-independent delivery records and target planning.
- `simulation`: world resolution and simulation-domain records.
- `session`: events, state, board/log/telemetry records, launch environment,
  modes, participant roles, and bounded stores.
- `check`: source and compiled participant metadata.
- `artifacts`: neutral native-artifact descriptors.

Core has no clap command types, crossterm, ratatui, terminal output policy, raw
bus authority, SSH, or process execution.

## UI crate

`phoxal-cli-ui` owns the complete terminal surface: terminal guards and input,
startup and runtime state, view models, visibility, ratatui rendering, and
semantic theme roles. Its TUI consumes core session records and emits UI
actions; it never imports root command modules or owns project resolution,
artifact provisioning, deployment, process supervision, or raw bus sessions.

## Dependency rule

Move reusable behavior toward core and terminal behavior toward UI. Keep
operating-system and network adapters in the root, except the suite client
and train-status probe noted above. Do not add compatibility re-exports for old internal paths: update
consumers to the owning crate. The audited dependency direction is
`root -> UI -> core` and `root -> core`, with no reverse edge.
