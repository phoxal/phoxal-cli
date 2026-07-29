//! Closed identifiers for pages, panels, and mounted tui-realm components.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PageId {
    #[default]
    Overview,
    Runtimes,
    Logs,
    Bus,
    Input,
}

impl PageId {
    pub const ALL: [Self; 5] = [
        Self::Overview,
        Self::Runtimes,
        Self::Logs,
        Self::Bus,
        Self::Input,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Runtimes => "Runtimes",
            Self::Logs => "Logs",
            Self::Bus => "Bus",
            Self::Input => "Input",
        }
    }

    #[must_use]
    pub fn offset(self, delta: isize) -> Self {
        let index = Self::ALL.iter().position(|page| *page == self).unwrap_or(0);
        let len = Self::ALL.len() as isize;
        Self::ALL[((index as isize + delta).rem_euclid(len)) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimesPanelId {
    Processes,
    Performance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogsPanelId {
    Filters,
    Stream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BusPanelId {
    Controls,
    Topics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputPanelId {
    Devices,
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelId {
    Runtimes(RuntimesPanelId),
    Logs(LogsPanelId),
    Bus(BusPanelId),
    Input(InputPanelId),
}

impl PanelId {
    #[must_use]
    pub const fn page(self) -> PageId {
        match self {
            Self::Runtimes(_) => PageId::Runtimes,
            Self::Logs(_) => PageId::Logs,
            Self::Bus(_) => PageId::Bus,
            Self::Input(_) => PageId::Input,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModalId {
    Help,
    SessionInfo,
    ConfirmStop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ComponentId {
    Input,
    Header,
    Tabs,
    Overview,
    Runtimes,
    Logs,
    Bus,
    InputPage,
    Modal,
    Footer,
}
