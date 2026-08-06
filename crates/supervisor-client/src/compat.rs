//! Connect-time validation: the API revision plus every client-visible schema
//! this client intends to consume.
//!
//! This is the *whole* attachment gate. Framework SemVer is provenance, not
//! compatibility: a newer `phoxal` may attach to an older running `phoxald`
//! whenever these checks pass, because the exact-pair rule governs what is
//! installed on disk, not what a client may attach to.
//!
//! A client validates only what it will open. A `status` command that reads the
//! snapshot and nothing else has no business failing on a telemetry schema it
//! never subscribes to.

use phoxal_runtime_contract::ApiId;
use phoxal_supervisor_api::{SchemaSurface, SupervisorSchemas, current_api};

use crate::error::AttachError;

/// What this client requires of a supervisor.
#[derive(Clone, Debug)]
pub struct Expectations {
    api: ApiId,
    schemas: SupervisorSchemas,
    consumed: Vec<SchemaSurface>,
}

impl Expectations {
    /// Require every surface: the full attachment a TUI session opens.
    #[must_use]
    pub fn full() -> Self {
        Self::consuming(SchemaSurface::ALL.to_vec())
    }

    /// Require only the named surfaces.
    ///
    /// [`SchemaSurface::Bus`] is always included: every other surface rides on
    /// it, so a client that consumes anything consumes the bus ABI.
    #[must_use]
    pub fn consuming(surfaces: impl IntoIterator<Item = SchemaSurface>) -> Self {
        let mut consumed: Vec<SchemaSurface> = surfaces.into_iter().collect();
        consumed.push(SchemaSurface::Bus);
        consumed.sort_unstable();
        consumed.dedup();
        Self {
            api: current_api(),
            schemas: SupervisorSchemas::current(),
            consumed,
        }
    }

    /// The surfaces this client validates, in diagnostic order.
    #[must_use]
    pub fn consumed(&self) -> &[SchemaSurface] {
        &self.consumed
    }

    /// Check a connect reply's compatibility claims.
    ///
    /// # Errors
    ///
    /// [`AttachError::ApiMismatch`] or the first
    /// [`AttachError::SchemaMismatch`], each naming both sides and the fix.
    pub fn validate(
        &self,
        endpoint: &str,
        api: &ApiId,
        schemas: &SupervisorSchemas,
    ) -> Result<(), AttachError> {
        if *api != self.api {
            return Err(AttachError::ApiMismatch {
                endpoint: endpoint.to_string(),
                client: self.api.clone(),
                daemon: api.clone(),
            });
        }
        for surface in &self.consumed {
            let client = self.schemas.get(*surface);
            let daemon = schemas.get(*surface);
            if client != daemon {
                return Err(AttachError::SchemaMismatch {
                    endpoint: endpoint.to_string(),
                    surface: *surface,
                    client: client.clone(),
                    daemon: daemon.clone(),
                });
            }
        }
        Ok(())
    }
}

impl Default for Expectations {
    fn default() -> Self {
        Self::full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_runtime_contract::SchemaId;

    const ENDPOINT: &str = "unix/.phoxal/run/supervisor.sock";

    #[test]
    fn a_matching_daemon_passes_every_surface() {
        Expectations::full()
            .validate(ENDPOINT, &current_api(), &SupervisorSchemas::current())
            .expect("a daemon on the same train attaches");
    }

    #[test]
    fn a_schema_mismatch_names_both_sides_the_surface_and_the_fix() {
        let mut daemon = SupervisorSchemas::current();
        daemon.snapshot = SchemaId::new("phoxal/supervisor-snapshot/v1");

        let error = Expectations::full()
            .validate(ENDPOINT, &current_api(), &daemon)
            .expect_err("a changed snapshot schema must not attach");
        let rendered = error.to_string();

        assert!(matches!(
            error,
            AttachError::SchemaMismatch {
                surface: SchemaSurface::Snapshot,
                ..
            }
        ));
        assert!(rendered.contains("snapshot schema"), "{rendered}");
        assert!(
            rendered.contains("phoxal/supervisor-snapshot/v1"),
            "{rendered}"
        );
        assert!(
            rendered.contains(phoxal_supervisor_api::SNAPSHOT_SCHEMA),
            "{rendered}"
        );
        assert!(rendered.contains(ENDPOINT), "{rendered}");
        assert!(rendered.contains("same train"), "{rendered}");
    }

    #[test]
    fn an_api_mismatch_names_both_revisions() {
        let error = Expectations::full()
            .validate(ENDPOINT, &ApiId::new("v0.2"), &SupervisorSchemas::current())
            .expect_err("a different robot API must not attach");
        let rendered = error.to_string();
        assert!(matches!(error, AttachError::ApiMismatch { .. }));
        assert!(rendered.contains("v0.2"), "{rendered}");
        assert!(rendered.contains("v0.1"), "{rendered}");
    }

    #[test]
    fn a_client_is_only_gated_on_what_it_opens() {
        let mut daemon = SupervisorSchemas::current();
        daemon.telemetry = SchemaId::new("phoxal/supervisor-telemetry/v1");

        // `status` reads the snapshot; a telemetry schema it never subscribes
        // to is not its problem.
        let status = Expectations::consuming([SchemaSurface::Snapshot]);
        status
            .validate(ENDPOINT, &current_api(), &daemon)
            .expect("an unconsumed surface does not gate attachment");
        // The bus ABI is never optional: everything else rides on it.
        assert!(status.consumed().contains(&SchemaSurface::Bus));

        Expectations::full()
            .validate(ENDPOINT, &current_api(), &daemon)
            .expect_err("a full session does consume telemetry");
    }
}
