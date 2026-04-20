use crate::ids::{PaneId, TabId, WorkspaceId};

/// Recursive pane layout tree.
#[derive(Clone, Debug)]
pub enum PaneLayout {
    Single(PaneId),
    HSplit {
        ratio: f32,
        top: Box<PaneLayout>,
        bottom: Box<PaneLayout>,
    },
    VSplit {
        ratio: f32,
        left: Box<PaneLayout>,
        right: Box<PaneLayout>,
    },
}

/// A single tab containing a pane layout.
#[derive(Debug)]
pub struct Tab {
    pub id: TabId,
    pub title: String,
    pub layout: PaneLayout,
}

impl Tab {
    pub fn new_single(title: impl Into<String>) -> (Self, PaneId) {
        let pane_id = PaneId::new();
        let tab = Self {
            id: TabId::new(),
            title: title.into(),
            layout: PaneLayout::Single(pane_id),
        };
        (tab, pane_id)
    }
}

/// The top-level workspace containing tabs.
#[derive(Debug)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            id: WorkspaceId::new(),
            tabs: Vec::new(),
            active_tab: 0,
        }
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}
