//! The Crowded Room: direct programs sharing a small PTY umbrella.

mod app;
mod doorbell;
mod mailroom;
mod pane;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("send") {
        doorbell::send_command()
    } else {
        app::run()
    }
}
