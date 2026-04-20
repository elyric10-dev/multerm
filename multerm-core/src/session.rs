use crossbeam_channel::Receiver;
use multerm_vt::TerminalParser;

use crate::ids::PaneId;

/// A live terminal session: parser state + PTY output channel.
pub struct TerminalSession {
    pub pane_id: PaneId,
    pub parser:  TerminalParser,
    pub rx:      Receiver<Vec<u8>>,
}

impl TerminalSession {
    pub fn new(pane_id: PaneId, rows: usize, cols: usize, rx: Receiver<Vec<u8>>) -> Self {
        Self {
            pane_id,
            parser: TerminalParser::new(rows, cols),
            rx,
        }
    }

    /// Drain all pending PTY chunks and feed them into the parser.
    /// Returns `true` if any data was processed.
    pub fn drain_and_parse(&mut self) -> bool {
        let mut had_data = false;
        while let Ok(chunk) = self.rx.try_recv() {
            self.parser.process(&chunk);
            had_data = true;
        }
        had_data
    }
}
