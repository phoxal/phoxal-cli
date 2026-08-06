//! Resolving an endpoint to exactly one execution.
//!
//! Step 3 of direct attachment is a rule, not a preference: zero routers means
//! there is nothing to attach to, and more than one means the endpoint does not
//! identify a robot. Selecting among several routers behind a shared fabric is
//! deliberately a separate problem (organization#989).

use std::fmt;

use phoxal_runtime_contract::ExecutionId;

use crate::error::AttachError;

/// A router identity exactly as the transport reported it.
///
/// Kept as text rather than parsed on construction so an identity that is not
/// a Phoxal execution can still be *named* in the diagnostic that rejects it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RouterId(String);

impl RouterId {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The execution this router *is*.
    ///
    /// One supervisor equals one router equals one execution, and the two
    /// identities are the same value with the same canonical text - so this is
    /// a parse of the transport's own rendering, never a lookup.
    ///
    /// # Errors
    ///
    /// Returns [`AttachError::RouterIdentity`] when the router is not a Phoxal
    /// execution: a foreign Zenoh router on the same endpoint, or a router
    /// whose id was authored rather than pinned.
    pub fn execution(&self) -> Result<ExecutionId, AttachError> {
        ExecutionId::parse(&self.0).map_err(|source| AttachError::RouterIdentity {
            router: self.clone(),
            source,
        })
    }
}

impl fmt::Display for RouterId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Apply the exactly-one-router rule and derive the execution from the winner.
///
/// # Errors
///
/// [`AttachError::NoRouter`] for none, [`AttachError::MultipleRouters`] for
/// more than one, and [`AttachError::RouterIdentity`] when the single router is
/// not a Phoxal execution.
pub fn exactly_one_execution(
    endpoint: &str,
    routers: &[RouterId],
) -> Result<ExecutionId, AttachError> {
    match routers {
        [] => Err(AttachError::NoRouter {
            endpoint: endpoint.to_string(),
        }),
        [only] => only.execution(),
        many => {
            let mut sorted: Vec<&RouterId> = many.iter().collect();
            sorted.sort();
            Err(AttachError::MultipleRouters {
                endpoint: endpoint.to_string(),
                count: many.len(),
                routers: sorted
                    .iter()
                    .map(|router| router.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution_router() -> (ExecutionId, RouterId) {
        let execution = ExecutionId::mint();
        let router = RouterId::new(execution.to_string());
        (execution, router)
    }

    #[test]
    fn exactly_one_router_resolves_and_anything_else_is_an_error() {
        let (execution, router) = execution_router();
        assert_eq!(
            exactly_one_execution("tcp/host:7447", std::slice::from_ref(&router)).unwrap(),
            execution
        );

        let none = exactly_one_execution("tcp/host:7447", &[])
            .expect_err("no router means nothing to attach to");
        assert!(matches!(none, AttachError::NoRouter { .. }), "{none}");
        assert!(none.to_string().contains("tcp/host:7447"));

        let (_, second) = execution_router();
        let many = exactly_one_execution("tcp/host:7447", &[router.clone(), second.clone()])
            .expect_err("an endpoint must name exactly one execution");
        let rendered = many.to_string();
        assert!(matches!(
            many,
            AttachError::MultipleRouters { count: 2, .. }
        ));
        // Both identities are named, so an operator can tell which fabric they
        // reached rather than being told only that it was ambiguous.
        assert!(rendered.contains(router.as_str()), "{rendered}");
        assert!(rendered.contains(second.as_str()), "{rendered}");
    }

    #[test]
    fn a_router_that_is_not_an_execution_is_named_in_its_own_rejection() {
        for text in [
            "",
            "ABCDEF01234567890123456789ABCDEF",
            "0f00000000000000000000000000000f",
            "abc",
        ] {
            let router = RouterId::new(text);
            let error = exactly_one_execution("tcp/host:7447", std::slice::from_ref(&router))
                .expect_err("a non-execution router id must not resolve");
            assert!(
                matches!(error, AttachError::RouterIdentity { .. }),
                "{text:?} produced {error}"
            );
            assert!(error.to_string().contains(text), "{text:?}");
        }
    }
}
