/// Cursor location/state for a rendered terminal pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorState {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
}
