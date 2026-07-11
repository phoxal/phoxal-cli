use std::sync::{Mutex, OnceLock};

use serde_json::json;

use crate::commands::MessageFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateNotice {
    Artifacts(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NoticePolicy {
    pub(crate) artifact_consuming: bool,
    pub(crate) message_format: MessageFormat,
    pub(crate) quiet: bool,
    pub(crate) interactive: bool,
}

#[derive(Debug)]
struct InvocationState {
    policy: NoticePolicy,
    notice: Option<UpdateNotice>,
}

fn invocation() -> &'static Mutex<Option<InvocationState>> {
    static INVOCATION: OnceLock<Mutex<Option<InvocationState>>> = OnceLock::new();
    INVOCATION.get_or_init(|| Mutex::new(None))
}

pub(crate) fn begin(policy: NoticePolicy) {
    *invocation()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(InvocationState {
        policy,
        notice: None,
    });
}

/// Offers a notice to the once-per-top-level-invocation gate.
///
/// The first non-empty notice wins. Watch rebuilds never call this because
/// their resolver options set `emit_update_notice` to false.
pub(crate) fn offer(notice: UpdateNotice) {
    let mut invocation = invocation()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = invocation.as_mut() else {
        return;
    };
    if state.notice.is_none() {
        state.notice = Some(notice);
    }
}

pub(crate) fn finish() {
    let state = invocation()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(state) = state else {
        return;
    };
    if let Some(message) = render(state.policy, state.notice.as_ref()) {
        eprintln!("{message}");
    }
}

fn render(policy: NoticePolicy, notice: Option<&UpdateNotice>) -> Option<String> {
    if !policy.artifact_consuming || policy.quiet || !policy.interactive {
        return None;
    }
    match (policy.message_format, notice?) {
        (MessageFormat::Human, UpdateNotice::Artifacts(newer)) => Some(format!(
            "warning: newer artifact versions available: {}; run `phoxal update`",
            newer.join(", ")
        )),
        (MessageFormat::Json, UpdateNotice::Artifacts(newer)) => {
            Some(json!({ "updates_available": newer }).to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(message_format: MessageFormat) -> NoticePolicy {
        NoticePolicy {
            artifact_consuming: true,
            message_format,
            quiet: false,
            interactive: true,
        }
    }

    #[test]
    fn artifact_json_notice_has_a_structured_updates_available_field() {
        let updates = vec!["phoxal/service-drive 0.5.0 -> 0.6.0".to_string()];
        let rendered = render(
            policy(MessageFormat::Json),
            Some(&UpdateNotice::Artifacts(updates.clone())),
        )
        .expect("notice should render");
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["updates_available"][0], updates[0]);
    }

    #[test]
    fn notice_is_suppressed_for_quiet_non_interactive_and_non_artifact_sessions() {
        let notice = UpdateNotice::Artifacts(vec!["update".to_string()]);
        for suppressed in [
            NoticePolicy {
                quiet: true,
                ..policy(MessageFormat::Human)
            },
            NoticePolicy {
                interactive: false,
                ..policy(MessageFormat::Human)
            },
            NoticePolicy {
                artifact_consuming: false,
                ..policy(MessageFormat::Human)
            },
        ] {
            assert_eq!(render(suppressed, Some(&notice)), None);
        }
    }
}
