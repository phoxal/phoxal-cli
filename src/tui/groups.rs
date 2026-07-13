//! Build the navigator's grouped runtime list (design doc, "Left pane") from
//! a [`BoardSnapshot`]: one stable heading per group, in a fixed order, each
//! showing only its present members - never an empty heading.

use crate::launch_plan::{SITE_TOOL_JOYPAD, SITE_TOOL_ROUTER, SITE_TOOL_TELEMETRY};
use crate::participant_kind::ParticipantKind;
use crate::supervisor::{BoardSnapshot, ParticipantStatus};

/// The synthetic Webots application row's id. It is supervision machinery,
/// not an operator-facing tool, and is therefore filtered from the navigator.
pub const WEBOTS_SITE_ID: &str = "webots";
const WEBOTS_SUPERVISOR_ID: &str = "simulator-webots-supervisor";
const WEBOTS_CONTROLLER_ID_PREFIX: &str = "simulator-webots-controller";

/// The stable navigator group order (design doc, "Left pane").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    System,
    Services,
    Drivers,
}

impl Group {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Services => "Services",
            Self::Drivers => "Drivers",
        }
    }
}

/// One group heading plus its present member ids, in board (alphabetic) id
/// order within the group.
#[derive(Debug, Clone)]
pub struct GroupSection {
    pub group: Group,
    pub ids: Vec<String>,
}

/// A hardcoded id -> group override for ids whose board `kind` alone does
/// not place them correctly: the site tools (router/joypad/telemetry, board
/// `kind: Tool`) belong in System rather than falling through to their raw
/// kind.
fn group_override(id: &str) -> Option<Group> {
    match id {
        SITE_TOOL_ROUTER | SITE_TOOL_JOYPAD | SITE_TOOL_TELEMETRY => Some(Group::System),
        _ => None,
    }
}

#[must_use]
pub fn group_for(id: &str, kind: ParticipantKind) -> Group {
    if let Some(group) = group_override(id) {
        return group;
    }
    match kind {
        ParticipantKind::Tool => Group::System,
        ParticipantKind::Service => Group::Services,
        ParticipantKind::Driver => Group::Drivers,
        // Simulator participants are filtered by `visible_in_navigator`.
        ParticipantKind::Simulator => Group::System,
    }
}

fn visible_in_navigator(status: &ParticipantStatus, simulation: bool) -> bool {
    // Webots-owned rows can arrive from the live board as `Service` even
    // though they are simulation machinery. Identify those rows by their
    // stable launch ids as well as by kind so implementation details never
    // leak into the operator-facing service list.
    if status.id == WEBOTS_SITE_ID
        || status.id == WEBOTS_SUPERVISOR_ID
        || status.id.starts_with(WEBOTS_CONTROLLER_ID_PREFIX)
        || status.kind == ParticipantKind::Simulator
    {
        return false;
    }
    !(simulation && status.kind == ParticipantKind::Driver)
}

/// Partition `board`'s participants into the stable group order, dropping
/// any group with no members entirely (never an empty heading). `filter` is
/// an already-lowercased substring match against the id (see `state::AppState`'s
/// `/` filter); pass `""` for no filtering.
#[must_use]
pub fn build_groups(board: &BoardSnapshot, filter: &str, simulation: bool) -> Vec<GroupSection> {
    let mut buckets: [(Group, Vec<String>); 3] = [
        (Group::System, Vec::new()),
        (Group::Services, Vec::new()),
        (Group::Drivers, Vec::new()),
    ];
    for status in board.participants.values() {
        if !visible_in_navigator(status, simulation) {
            continue;
        }
        if !filter.is_empty() && !status.id.to_lowercase().contains(filter) {
            continue;
        }
        let group = group_for(&status.id, status.kind);
        if let Some((_, ids)) = buckets
            .iter_mut()
            .find(|(candidate, _)| *candidate == group)
        {
            ids.push(status.id.clone());
        }
    }
    buckets
        .into_iter()
        .filter(|(_, ids)| !ids.is_empty())
        .map(|(group, ids)| GroupSection { group, ids })
        .collect()
}

/// The row the "Right pane opens on Overview" suggestion (design doc) should
/// point at: the first failed/degraded participant if any exist, else the
/// first still-starting one, else `None` (nothing needs attention).
#[must_use]
pub fn suggested_participant(board: &BoardSnapshot) -> Option<&ParticipantStatus> {
    use crate::supervisor::ParticipantState;
    board
        .participants
        .values()
        .find(|status| {
            matches!(
                status.state,
                ParticipantState::Failed | ParticipantState::Degraded
            )
        })
        .or_else(|| {
            board
                .participants
                .values()
                .find(|status| status.state == ParticipantState::Starting)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::{ParticipantState, ParticipantStatus};

    fn board_with(entries: &[(&str, ParticipantKind, ParticipantState)]) -> BoardSnapshot {
        let mut board = BoardSnapshot::default();
        for (id, kind, state) in entries {
            board.participants.insert(
                (*id).to_string(),
                ParticipantStatus::new(*id, *kind, *state),
            );
        }
        board
    }

    #[test]
    fn router_and_joypad_land_in_system_despite_tool_kind() {
        assert_eq!(
            group_for(SITE_TOOL_ROUTER, ParticipantKind::Tool),
            Group::System
        );
        assert_eq!(
            group_for(SITE_TOOL_JOYPAD, ParticipantKind::Tool),
            Group::System
        );
        assert_eq!(group_for("telemetry", ParticipantKind::Tool), Group::System);
    }

    #[test]
    fn ordinary_operator_facing_kinds_map_to_their_matching_group() {
        assert_eq!(
            group_for("drive", ParticipantKind::Service),
            Group::Services
        );
        assert_eq!(
            group_for("left_wheel", ParticipantKind::Driver),
            Group::Drivers
        );
    }

    #[test]
    fn empty_groups_never_appear() {
        let board = board_with(&[("drive", ParticipantKind::Service, ParticipantState::Ready)]);
        let groups = build_groups(&board, "", false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group, Group::Services);
    }

    #[test]
    fn groups_stay_in_stable_order() {
        let board = board_with(&[
            (
                "left_wheel",
                ParticipantKind::Driver,
                ParticipantState::Ready,
            ),
            (
                SITE_TOOL_ROUTER,
                ParticipantKind::Tool,
                ParticipantState::Ready,
            ),
            ("drive", ParticipantKind::Service, ParticipantState::Ready),
            (
                "simulator-webots-supervisor",
                ParticipantKind::Service,
                ParticipantState::Ready,
            ),
            (
                "simulator-webots-controller-robot-v1",
                ParticipantKind::Service,
                ParticipantState::Ready,
            ),
        ]);
        let groups = build_groups(&board, "", false);
        let order: Vec<Group> = groups.iter().map(|section| section.group).collect();
        assert_eq!(order, vec![Group::System, Group::Services, Group::Drivers]);
    }

    #[test]
    fn simulation_hides_component_drivers_and_webots_internals() {
        let board = board_with(&[
            ("drive", ParticipantKind::Service, ParticipantState::Ready),
            (
                "left_wheel",
                ParticipantKind::Driver,
                ParticipantState::Ready,
            ),
            (
                WEBOTS_SITE_ID,
                ParticipantKind::Tool,
                ParticipantState::Ready,
            ),
            (
                "simulator-webots-supervisor",
                ParticipantKind::Service,
                ParticipantState::Ready,
            ),
            (
                "simulator-webots-controller-robot-v1",
                ParticipantKind::Service,
                ParticipantState::Ready,
            ),
        ]);
        let groups = build_groups(&board, "", true);
        let ids = groups
            .iter()
            .flat_map(|section| section.ids.iter().map(String::as_str))
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["drive"]);
    }

    #[test]
    fn filter_matches_by_substring_case_insensitively() {
        let board = board_with(&[
            (
                "left_wheel",
                ParticipantKind::Driver,
                ParticipantState::Ready,
            ),
            (
                "right_wheel",
                ParticipantKind::Driver,
                ParticipantState::Ready,
            ),
        ]);
        let groups = build_groups(&board, "left", false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].ids, vec!["left_wheel".to_string()]);
    }

    #[test]
    fn suggested_prefers_failed_over_degraded_over_starting() {
        let board = board_with(&[
            ("a", ParticipantKind::Service, ParticipantState::Starting),
            ("b", ParticipantKind::Service, ParticipantState::Degraded),
            ("c", ParticipantKind::Service, ParticipantState::Failed),
            ("d", ParticipantKind::Service, ParticipantState::Ready),
        ]);
        let suggested = suggested_participant(&board).expect("a suggestion");
        assert!(matches!(
            suggested.state,
            ParticipantState::Failed | ParticipantState::Degraded
        ));
    }

    #[test]
    fn suggested_falls_back_to_starting_when_nothing_is_unhealthy() {
        let board = board_with(&[
            ("a", ParticipantKind::Service, ParticipantState::Ready),
            ("b", ParticipantKind::Service, ParticipantState::Starting),
        ]);
        let suggested = suggested_participant(&board).expect("a suggestion");
        assert_eq!(suggested.id, "b");
    }

    #[test]
    fn suggested_is_none_when_everything_is_healthy() {
        let board = board_with(&[("a", ParticipantKind::Service, ParticipantState::Ready)]);
        assert!(suggested_participant(&board).is_none());
    }
}
