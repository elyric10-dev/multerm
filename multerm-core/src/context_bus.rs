use crossbeam_channel::{unbounded, Receiver, Sender};
use std::sync::Arc;

/// Events published on the context bus (Phase 3 expansion).
#[derive(Debug, Clone)]
pub enum ContextBusEvent {
    /// Raw PTY output captured from a pane.
    PaneOutput { pane_id: crate::ids::PaneId, data: Vec<u8> },
    /// A watched file changed.
    FileChanged { path: String },
    /// User-initiated notepad edit.
    NotebadEdit { content: String },
}

/// A cloneable, broadcast-style context bus backed by crossbeam channels.
#[derive(Clone)]
pub struct ContextBus {
    tx: Sender<ContextBusEvent>,
    rx: Receiver<ContextBusEvent>,
}

impl ContextBus {
    pub fn new() -> Arc<Self> {
        let (tx, rx) = unbounded();
        Arc::new(Self { tx, rx })
    }

    pub fn publish(&self, event: ContextBusEvent) {
        let _ = self.tx.send(event);
    }

    pub fn receiver(&self) -> Receiver<ContextBusEvent> {
        self.rx.clone()
    }
}

impl Default for ContextBus {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self { tx, rx }
    }
}
