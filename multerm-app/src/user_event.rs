/// Custom events sent from background threads to the winit event loop.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// New PTY output is available — drain the channel and request a redraw.
    PtyData,
}
