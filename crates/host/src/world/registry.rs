//! Exact-ID live world registration lookup and lease validation.

use super::*;

trait ValidateLocalWorldRegistration {
    fn validate(&self, expected_instance: &str) -> Result<()>;
}

impl ValidateLocalWorldRegistration for LocalWorldRegistration {
    fn validate(&self, expected_instance: &str) -> Result<()> {
        let expected = parse_instance_id(expected_instance)?;
        self.validate_structure(expected)?;
        ensure!(
            self.lease == format!("{}.lease", self.instance),
            "world registration lease must be the instance-relative basename"
        );
        Ok(())
    }
}

pub(super) trait ValidateWorldCheckpoint {
    fn validate(&self, registration: &LocalWorldRegistration) -> Result<()>;
}

impl ValidateWorldCheckpoint for WorldCheckpoint {
    fn validate(&self, registration: &LocalWorldRegistration) -> Result<()> {
        self.validate_structure(registration)?;
        if let Some(native) = &self.native_process {
            ensure!(
                native.executable.is_absolute()
                    && native.executable.components().all(|component| matches!(
                        component,
                        Component::Prefix(_) | Component::RootDir | Component::Normal(_)
                    )),
                "native executable must be a canonical absolute path"
            );
            #[cfg(unix)]
            ensure!(
                native.process_group == Some(native.process.pid),
                "native Unix process group must equal its direct Webots PID"
            );
        }
        Ok(())
    }
}

/// Exact-ID reader and stale-cleaner for live registrations.
pub struct WorldRegistry<I = SystemProcessInspector> {
    pub(super) paths: WorldPaths,
    pub(super) inspector: I,
}

pub(super) enum RegistrationProbe {
    Missing,
    Live(LocalWorldRegistration),
    Stale(StaleRegistration),
}

pub(super) struct StaleRegistration {
    pub(super) registration: LocalWorldRegistration,
    pub(super) registration_file: File,
    pub(super) lease_file: File,
}

impl WorldRegistry<SystemProcessInspector> {
    pub fn discover() -> Result<Self> {
        Ok(Self::new(WorldPaths::discover()?, SystemProcessInspector))
    }
}

impl<I: ProcessInspector> WorldRegistry<I> {
    #[must_use]
    pub const fn new(paths: WorldPaths, inspector: I) -> Self {
        Self { paths, inspector }
    }

    #[must_use]
    pub const fn paths(&self) -> &WorldPaths {
        &self.paths
    }

    /// Resolve exactly one complete instance ID and validate both liveness
    /// witnesses. An unlocked lease is reported as stale but retained for
    /// evidence-aware recovery. A locked lease paired with the wrong process
    /// birth is inconsistent and is never silently removed or trusted.
    pub fn resolve(&self, instance: &str) -> Result<LocalWorldRegistration> {
        validate_instance_id(instance)?;
        self.find(instance)?.with_context(|| {
            format!(
                "no live world instance `{instance}` is registered; `phoxal simulation list` shows live instances"
            )
        })
    }

    /// Resolve a complete instance ID when it is live, returning `None` for a
    /// missing or ordinary stale registration.
    pub fn find(&self, instance: &str) -> Result<Option<LocalWorldRegistration>> {
        validate_instance_id(instance)?;
        self.read_live(instance)
    }

    /// Return every valid live registration in full-ID order. Stale crash
    /// residue is retained so an evidence-aware lookup can finalize it.
    pub fn list(&self) -> Result<Vec<LocalWorldRegistration>> {
        let mut instances = Vec::new();
        for instance in self.registration_instances()? {
            if let Some(registration) = self.read_live(&instance)? {
                instances.push(registration);
            }
        }
        instances.sort_by_key(|registration| registration.instance.to_string());
        Ok(instances)
    }

    /// Return every syntactically valid instance named by a registration,
    /// including stale entries. This performs no lifecycle recovery.
    pub fn registration_instances(&self) -> Result<Vec<String>> {
        let mut instances = Vec::new();
        for entry in fs::read_dir(self.paths.registry()).with_context(|| {
            format!(
                "failed to read world registry {}",
                self.paths.registry().display()
            )
        })? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(instance) = name.strip_suffix(".json") else {
                continue;
            };
            validate_instance_id(instance)
                .with_context(|| format!("invalid world registration filename `{name}`"))?;
            instances.push(instance.to_owned());
        }
        instances.sort();
        instances.dedup();
        Ok(instances)
    }

    fn read_live(&self, instance: &str) -> Result<Option<LocalWorldRegistration>> {
        match self.probe(instance)? {
            RegistrationProbe::Missing | RegistrationProbe::Stale(_) => Ok(None),
            RegistrationProbe::Live(registration) => Ok(Some(registration)),
        }
    }

    pub(super) fn probe(&self, instance: &str) -> Result<RegistrationProbe> {
        let path = self.paths.registration_path(instance);
        let Some((registration_file, document)) = open_and_read_owner_file_if_present(&path)?
        else {
            return Ok(RegistrationProbe::Missing);
        };
        let registration: LocalWorldRegistration = serde_json::from_slice(&document)
            .with_context(|| format!("failed to parse world registration {}", path.display()))?;
        registration.validate(instance)?;

        let lease_path = self.paths.registry().join(&registration.lease);
        let lease = open_owner_file(&lease_path, true)
            .with_context(|| format!("failed to open world lease {}", lease_path.display()))?;
        let acquired = try_lock_lease(&lease)?;
        let observed_birth = self.inspector.started_at_unix_s(registration.process.pid);
        let process_matches = observed_birth == Some(registration.process.started_at_unix_s);

        if !acquired && process_matches {
            return Ok(RegistrationProbe::Live(registration));
        }
        if !acquired {
            bail!(
                "world registration `{instance}` has a live lease but PID {} has birth {:?}, expected {}; refusing to trust or remove it",
                registration.process.pid,
                observed_birth,
                registration.process.started_at_unix_s
            );
        }
        if process_matches {
            bail!(
                "world registration `{instance}` has an unlocked lease while its exact host process {} is still live; refusing premature recovery",
                registration.process.pid
            );
        }
        Ok(RegistrationProbe::Stale(StaleRegistration {
            registration,
            registration_file,
            lease_file: lease,
        }))
    }
}
