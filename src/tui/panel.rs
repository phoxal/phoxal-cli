//! The closed set of detail-surface panels a runtime's detail view can offer
//! (Target design part 6). Before this module, panel selection was driven by
//! PARTICIPANT-ID STRING CHECKS scattered across `tui::state`
//! (`on_router_traffic_tab`/`on_joypad_devices_tab`), `tui::groups`
//! (`bespoke_tab_label`), and `tui::render` (`footer_segments`,
//! `draw_bespoke_tab`, `tab_label`) - four separate places that all had to
//! agree on the same router/joypad/telemetry mapping. [`panels_for`] is now
//! the SOLE place a participant id is mapped to its panels; every other
//! panel-selection site consumes a [`Panel`] value or this function's
//! result, and the compiler's exhaustiveness checking on `match Panel { .. }`
//! replaces the old id `==` chains.

use crate::launch_plan::{SITE_TOOL_JOYPAD, SITE_TOOL_ROUTER, SITE_TOOL_TELEMETRY};

/// One detail-surface panel. `Overview`/`Logs` are offered by every
/// participant; `Traffic`/`Devices`/`Resources` are each offered by exactly
/// one hardcoded system tool (design doc: "hardcode the tool -> panel
/// mapping; no generic abstraction") - see [`panels_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Panel {
    #[default]
    Overview,
    Logs,
    Traffic,
    Devices,
    Resources,
}

impl Panel {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Logs => "Logs",
            Self::Traffic => "Traffic",
            Self::Devices => "Devices",
            Self::Resources => "Resources",
        }
    }
}

/// The single bespoke panel `id`'s tool offers, if any - the ONE mapping
/// function every panel-selection site now consumes instead of re-checking
/// the id itself. Mirrors `groups::group_override`'s hardcoded id set.
#[must_use]
pub fn bespoke_panel_for(id: &str) -> Option<Panel> {
    match id {
        SITE_TOOL_ROUTER => Some(Panel::Traffic),
        SITE_TOOL_JOYPAD => Some(Panel::Devices),
        SITE_TOOL_TELEMETRY => Some(Panel::Resources),
        _ => None,
    }
}

/// The panels `id`'s detail surface offers, in display order: Overview +
/// Logs always, plus a bespoke third panel for the three hardcoded system
/// tools (design doc), via [`bespoke_panel_for`].
#[must_use]
pub fn panels_for(id: &str) -> Vec<Panel> {
    let mut panels = vec![Panel::Overview, Panel::Logs];
    if let Some(bespoke) = bespoke_panel_for(id) {
        panels.push(bespoke);
    }
    panels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_offers_overview_logs_and_traffic_only() {
        assert_eq!(
            panels_for(SITE_TOOL_ROUTER),
            vec![Panel::Overview, Panel::Logs, Panel::Traffic]
        );
    }

    #[test]
    fn joypad_offers_overview_logs_and_devices_only() {
        assert_eq!(
            panels_for(SITE_TOOL_JOYPAD),
            vec![Panel::Overview, Panel::Logs, Panel::Devices]
        );
    }

    #[test]
    fn telemetry_offers_overview_logs_and_resources_only() {
        assert_eq!(
            panels_for(SITE_TOOL_TELEMETRY),
            vec![Panel::Overview, Panel::Logs, Panel::Resources]
        );
    }

    #[test]
    fn an_ordinary_participant_offers_only_overview_and_logs() {
        assert_eq!(panels_for("drive"), vec![Panel::Overview, Panel::Logs]);
        assert_eq!(panels_for("left_wheel"), vec![Panel::Overview, Panel::Logs]);
    }

    #[test]
    fn bespoke_panel_for_is_none_for_an_ordinary_participant() {
        assert_eq!(bespoke_panel_for("drive"), None);
    }

    #[test]
    fn panel_label_is_stable_for_every_variant() {
        assert_eq!(Panel::Overview.label(), "Overview");
        assert_eq!(Panel::Logs.label(), "Logs");
        assert_eq!(Panel::Traffic.label(), "Traffic");
        assert_eq!(Panel::Devices.label(), "Devices");
        assert_eq!(Panel::Resources.label(), "Resources");
    }
}
