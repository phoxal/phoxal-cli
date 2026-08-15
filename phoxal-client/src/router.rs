//! Resolve one configured endpoint to exactly one execution.

use phoxal_runtime_contract::identity::ExecutionId;

use crate::ConnectError;

pub(crate) fn exactly_one_execution(
    endpoint: &str,
    executions: &[ExecutionId],
) -> Result<ExecutionId, ConnectError> {
    match executions {
        [] => Err(ConnectError::NoExecution {
            endpoint: endpoint.to_string(),
        }),
        [only] => Ok(*only),
        many => {
            let mut executions = many.to_vec();
            executions.sort_by_key(ToString::to_string);
            Err(ConnectError::MultipleExecutions {
                endpoint: endpoint.to_string(),
                count: executions.len(),
                executions,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_execution_preserves_ambiguous_identities() {
        let first = ExecutionId::mint();
        assert_eq!(
            exactly_one_execution("tcp/host:7447", &[first]).expect("one execution resolves"),
            first
        );
        assert!(matches!(
            exactly_one_execution("tcp/host:7447", &[]),
            Err(ConnectError::NoExecution { .. })
        ));

        let second = ExecutionId::mint();
        let error = exactly_one_execution("tcp/host:7447", &[first, second])
            .expect_err("several executions are ambiguous");
        assert!(matches!(
            error,
            ConnectError::MultipleExecutions {
                count: 2,
                executions,
                ..
            } if executions.contains(&first) && executions.contains(&second)
        ));
    }
}
