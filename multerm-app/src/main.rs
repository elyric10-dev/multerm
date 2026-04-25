mod app;
mod clipboard;
mod daemon;
mod icon;
mod user_event;

use app::MultermApp;
use winit::event_loop::{ControlFlow, EventLoop};

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--daemon") {
        // Runs the background PTY daemon and exits into an accept loop.
        return daemon::run_daemon();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("multerm=debug".parse().unwrap())
                .add_directive("egui_winit::clipboard=off".parse().unwrap()),
        )
        .init();

    let event_loop = EventLoop::<user_event::UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    let mut app = MultermApp::new(proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}
