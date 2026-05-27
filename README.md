# xtask

`xtask` is the workspace orchestration binary for source validation, bundle generation, compose generation, local Webots simulation, and deployment workflows.

## Commands

```bash
cargo xtask generate bundle robot-v1
cargo xtask generate compose robot-v1 deploy
cargo xtask generate all robot-v1 local

cargo xtask validate robot robot-v1
cargo xtask validate component ddsm115
cargo xtask validate scenario --phase p0
cargo xtask validate scenario --phase p1
cargo xtask validate scenario --phase p2
cargo xtask validate scenario --tier 2 --category localization,mapping,traversability,revision-convergence
cargo xtask validate scenario --scenario p2-localization-rgbd-inertial-orb-slam3
cargo xtask validate scenario --list

cargo xtask report conformance robot-v1
cargo xtask report roles robot-v1
cargo xtask report inventory

cargo xtask doctor
cargo xtask doctor fix

cargo xtask webots up robot-v1 SimpleWorld
cargo xtask webots up robot-v1 Hengelosestraat
cargo xtask webots up robot-v1 SimpleWorld --local-driver left_motor_driver
cargo xtask webots up robot-v1 SimpleWorld --local-runtime odometry
cargo xtask webots up robot-v1 SimpleWorld --with-rerun
cargo xtask webots up robot-v1 SimpleWorld --with-joypad
cargo xtask webots up robot-v1 SimpleWorld --with-joypad=2d931510-d99f-494a-8c67-87feb05e1594
cargo xtask webots scenario robot-v1
cargo xtask webots scenario robot-v1 --scenario drive-forward,drive-backward
cargo xtask webots stage robot-v1 SimpleWorld
cargo xtask webots restart robot-v1 SimpleWorld
cargo xtask webots reset robot-v1
cargo xtask webots down robot-v1

cargo xtask deploy local robot-v1 192.168.1.50 --registry localhost:5500/robot-framework --tag local
cargo xtask deploy fleet robot-v1 myorg/myfleet ghcr.io/example/robot-framework v0.2.2
```

## Current Model

- `generate bundle` writes the source-shaped robot bundle under `dist/models/<robot-model>/bundle/`.
- `generate compose` writes only compose output. It does not own image build or deploy steps.
- `validate robot` checks the source model, role resolution, autonomy profile conformance, and typed deploy-descriptor construction.
- `validate component` checks `component.yaml`, `structure.urdf`, and `simulation.yaml`.
- `validate scenario` is the framework-conformance phase-gate runner (see `.plans/xtask-validate-scenario/readme.md`). It accepts `--phase`, `--tier --category`, `--scenario`, or `--list`; it does **not** accept a `<robot-model>` argument because framework scenarios are discovered from the runtime and robot binaries that own them. Headless scenarios run in the owning binary, Webots scenarios start a non-interactive session for the declared world, and the runner writes `dist/validation/scenario/<selector>/report.json` plus one stdout line per scenario and a summary line.
- `webots scenario` is the per-robot acceptance scenario runner (see `.plans/xtask-webots-scenario/readme.md`). It discovers the open robot-authored catalog by running the robot Runtime binary's `scenarios list` JSON output, filters optional `--scenario` names, groups selected scenarios by their Webots world, starts a non-interactive session per world, resets simulation before each runtime-owned scenario, and prints JSON result lines plus a JSON summary. It launches Webots with the GUI by default; pass `--headless` for non-visual runs. It validates one specific robot model end-to-end against simulator truth and does **not** gate Blueprint phases.
- `report conformance` prints the autonomy profile conformance report from `phoxal-utils-robot`.
- `report roles` prints the role-resolution report from `phoxal-utils-robot`.
- `report inventory` prints owner-local topic/query contracts from API crate `TypedSchema::SCHEMA_NAME` constants.
- `doctor` shows only `ok` and `warn` lines for xtask host prerequisites.
- `doctor fix` installs the Rust target and known macOS GNU cross toolchains when xtask knows how to do it.
- `webots up` is the primary local simulator workflow.
- `webots stage` stages the Webots project under `dist/simulator/webots/` and stops there. It does not build Docker images, write compose, start local processes, or launch Webots.
- `webots up` stages the Webots project under `dist/simulator/webots/`, including `protos/`, `worlds/`, `controllers/phoxal-simulator-webots-supervisor/`, and `controllers/phoxal-simulator-webots-controller/`.
- `webots up` generates reusable component PROTOs from authored `components/<type>/simulation.yaml` and composes them into a thin top-level robot PROTO.
- Webots PROTO rendering is real in this phase. It derives from source `component.yaml`, source `structure.urdf`, and `simulation.yaml`; Webots mesh assets are staged under `dist/simulator/webots/meshes/`.
- `webots stage` compiles the required staged host binaries for the staged project: `phoxal-simulator-webots-supervisor` and `phoxal-simulator-webots-controller`.
- `webots up` compiles the required host binaries at the beginning of the workflow, including `phoxal-simulator-webots-supervisor`, `phoxal-simulator-webots-controller`, any requested local host runtimes or drivers, and `phoxal-rerun-proxy` when `--with-rerun` is set.
- `webots up --with-joypad[=auto|id]` also builds and starts `phoxal-joypad` as a managed host-local process.
- `webots up` injects a dedicated `supervisor TRUE` node plus a separate generated robot node into the staged world.
- Simulation synchronization uses the `simulation/clock` stream plus the `simulation/reset` command-with-ack.
- Controller runtime metadata is still sourced from staged PROTO `# rf:` comments generated by `webots stage`/`webots up`.
- `phoxal-simulator-webots-controller` reads staged Webots PROTO metadata; host tools fetch source-shaped bundle assets such as `robot.yaml` and component configs through the robot asset query service.
- `webots up` builds the required local images from release-profile Rust binaries, writes local compose under `dist/dev/<robot-hostname>/docker-compose.yml`, runs `docker compose up -d`, starts requested host-local drivers and runtimes, starts `rerun-proxy` when `--with-rerun` is set, launches Webots, and then exits.
- The local Webots topology keeps one per-robot `phoxal-runtime-router` inside each Docker stack and exposes it to host-local tools and runtimes at `tcp/127.0.0.1:7447`.
- `webots down` discovers owned host-local processes by the hidden `--xtask-session=<robot-hostname>` marker, stops the single live Webots process on the system, and then runs `docker compose down`.
- `--with-joypad` defaults to `auto`, which binds `phoxal-joypad` to the first controller that appears. Pass `--with-joypad=<id>` to wait for a specific controller UUID.
- `webots restart` performs a full session restart via `down` then `up`.
- `webots reset` sends the reset command and returns only after a successful acknowledgement.
- Image builds use plain `cargo build --target <linux-triple>` for Rust binaries. When the host cannot natively link that target, the matching Rust target and host cross C linker must already be installed.
- All driver containers are excluded from the local Webots compose flow.
- All runtimes run in the local Webots workflow. Docker runs the full runtime suite by default; `--local-runtime <name>` moves that runtime out of Docker and starts it on the host.
- `--local-driver <component-id>` starts that component instance driver on the host.
- Local host processes are executed directly from `target/debug/` after the initial compile pass; `webots up` does not use `cargo run` for those processes.
- Local Webots processes use an interactive Zenoh connection profile with fast warnings and background retries when the local router path is unavailable.
- `xtask webots up` expects that no other Webots process is already running on the system. Use `webots down` or `webots restart` to replace it.
- `webots down` sends `SIGTERM` first and escalates to `SIGKILL` after a short wait. Pass `--force` to skip the graceful shutdown step and kill owned processes immediately.
- `deploy local` builds and pushes images to a local registry, generates deploy compose, and applies a local-mode Supervisor target state to the local balena device.
- `deploy fleet` builds and pushes images to the target registry/tag, generates deploy compose, and runs `balena deploy`.

## Bundle Output

`generate bundle` writes deterministic artifacts under:

```text
dist/models/<robot-model>/bundle/
```

Contents:

- `robot.yaml`
- `components/<component-type>/component.yaml`
- `structure.urdf`

The canonical deploy compose is written alongside the bundle at:

```text
dist/models/<robot-model>/docker-compose.yml
```

#### Dependency audit

`phoxal-cli/tests/dependency_audit.rs` enforces a ratchet on `phoxal-cli`'s dependency surface. The snapshot file `phoxal-cli/tests/dependency_audit_snapshot.txt` lists the runtime implementation, runtime API, and orchestration-layer crates that `phoxal-cli` is allowed to depend on transitionally while the workspace migrates to a registry- and manifest-driven shape. New dependencies that are not in the snapshot must be added to the allowed baseline inside the test (if they are foundation-shaped) or the snapshot (if they are transitional). Dependencies that are removed from `phoxal-cli/Cargo.toml` must also be removed from the snapshot, tightening the audit.
