//! Resolving an endpoint to exactly one execution.
//!
//! Step 2 of direct attachment is a rule, not a preference: zero routers means
//! there is nothing to attach to, and more than one means the endpoint does not
//! identify a robot. Selecting among several routers behind a shared fabric is
//! deliberately a separate problem.
//!
//! Cardinality is all this module decides. Whether a connected router *is* a
//! Phoxal execution is settled one layer down, by
//! [`phoxal_bus::Bus::probe_routers`], which reports executions rather than raw
//! transport identities and errors on anything that is not one.

use phoxal_runtime_contract::identity::ExecutionId;

use crate::error::AttachError;

/// Apply the exactly-one-router rule.
///
/// # Errors
///
/// [`AttachError::NoRouter`] for none and [`AttachError::MultipleRouters`] for
/// more than one.
pub fn exactly_one_execution(
    endpoint: &str,
    executions: &[ExecutionId],
) -> Result<ExecutionId, AttachError> {
    match executions {
        [] => Err(AttachError::NoRouter {
            endpoint: endpoint.to_string(),
        }),
        [only] => Ok(*only),
        many => {
            let mut sorted: Vec<String> = many.iter().map(ExecutionId::to_string).collect();
            sorted.sort();
            Err(AttachError::MultipleRouters {
                endpoint: endpoint.to_string(),
                count: many.len(),
                routers: sorted.join(", "),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_router_resolves_and_anything_else_is_an_error() {
        let execution = ExecutionId::mint();
        assert_eq!(
            exactly_one_execution("tcp/host:7447", &[execution]).unwrap(),
            execution
        );

        let none = exactly_one_execution("tcp/host:7447", &[])
            .expect_err("no router means nothing to attach to");
        assert!(matches!(none, AttachError::NoRouter { .. }), "{none}");
        assert!(none.to_string().contains("tcp/host:7447"));

        let second = ExecutionId::mint();
        let many = exactly_one_execution("tcp/host:7447", &[execution, second])
            .expect_err("an endpoint must name exactly one execution");
        let rendered = many.to_string();
        assert!(matches!(
            many,
            AttachError::MultipleRouters { count: 2, .. }
        ));
        // Both identities are named, so an operator can tell which fabric they
        // reached rather than being told only that it was ambiguous.
        assert!(rendered.contains(&execution.to_string()), "{rendered}");
        assert!(rendered.contains(&second.to_string()), "{rendered}");
    }
}
