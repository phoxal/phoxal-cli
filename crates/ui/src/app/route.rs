//! Structural panel-first focus routes.

use super::id::{BusPanelId, InputPanelId, LogsPanelId, ModalId, PageId, PanelId, RuntimesPanelId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusRoute {
    Tabs {
        page: PageId,
        candidate: PageId,
    },
    Panels {
        page: PageId,
        candidate: Option<PanelId>,
    },
    Content {
        panel: PanelId,
    },
    Modal {
        modal: ModalId,
        return_to: Box<FocusRoute>,
    },
}

impl Default for FocusRoute {
    fn default() -> Self {
        Self::Tabs {
            page: PageId::Overview,
            candidate: PageId::Overview,
        }
    }
}

impl FocusRoute {
    #[must_use]
    pub const fn page(&self) -> PageId {
        match self {
            Self::Tabs { page, .. } => *page,
            Self::Panels { page, .. } => *page,
            Self::Content { panel } => panel.page(),
            Self::Modal { return_to, .. } => return_to.page(),
        }
    }

    #[must_use]
    pub fn panels(page: PageId) -> Self {
        Self::Panels {
            page,
            candidate: first_panel(page),
        }
    }

    #[must_use]
    pub fn open_modal(self, modal: ModalId) -> Self {
        Self::Modal {
            modal,
            return_to: Box::new(self),
        }
    }
}

#[must_use]
pub const fn first_panel(page: PageId) -> Option<PanelId> {
    match page {
        PageId::Overview => None,
        PageId::Runtimes => Some(PanelId::Runtimes(RuntimesPanelId::Processes)),
        PageId::Logs => Some(PanelId::Logs(LogsPanelId::Filters)),
        PageId::Bus => Some(PanelId::Bus(BusPanelId::Controls)),
        PageId::Input => Some(PanelId::Input(InputPanelId::Devices)),
    }
}

#[must_use]
pub fn cycle_panel(page: PageId, current: Option<PanelId>, delta: isize) -> Option<PanelId> {
    let panels: &[PanelId] = match page {
        PageId::Overview => &[],
        PageId::Runtimes => &[
            PanelId::Runtimes(RuntimesPanelId::Processes),
            PanelId::Runtimes(RuntimesPanelId::Performance),
        ],
        PageId::Logs => &[
            PanelId::Logs(LogsPanelId::Filters),
            PanelId::Logs(LogsPanelId::Stream),
        ],
        PageId::Bus => &[
            PanelId::Bus(BusPanelId::Controls),
            PanelId::Bus(BusPanelId::Topics),
        ],
        PageId::Input => &[
            PanelId::Input(InputPanelId::Devices),
            PanelId::Input(InputPanelId::Motion),
        ],
    };
    if panels.is_empty() {
        return None;
    }
    let index = current
        .and_then(|panel| panels.iter().position(|candidate| *candidate == panel))
        .unwrap_or(0);
    let len = panels.len() as isize;
    Some(panels[((index as isize + delta).rem_euclid(len)) as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_identifiers_cannot_cross_page_boundaries() {
        for panel in [
            PanelId::Runtimes(RuntimesPanelId::Processes),
            PanelId::Logs(LogsPanelId::Stream),
            PanelId::Bus(BusPanelId::Topics),
            PanelId::Input(InputPanelId::Devices),
        ] {
            let route = FocusRoute::Content { panel };
            assert_eq!(route.page(), panel.page());
        }
    }

    #[test]
    fn modal_carries_the_exact_return_route() {
        let return_to = FocusRoute::Content {
            panel: PanelId::Logs(LogsPanelId::Stream),
        };
        assert_eq!(
            return_to.clone().open_modal(ModalId::Help),
            FocusRoute::Modal {
                modal: ModalId::Help,
                return_to: Box::new(return_to),
            }
        );
    }

    #[test]
    fn overview_has_no_fake_focusable_panel() {
        assert_eq!(first_panel(PageId::Overview), None);
        assert_eq!(cycle_panel(PageId::Overview, None, 1), None);
    }
}
