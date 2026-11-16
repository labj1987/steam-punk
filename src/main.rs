mod applog;
mod fsutil;
mod gamedata;
mod instance;
mod launcher;
mod library;
mod setup;
mod steam;
mod ui;

use std::sync::OnceLock;
use tokio::runtime::Runtime;

/// The Wayland app_id and X11 window class. It matches `StartupWMClass` in the
/// .desktop file, so the running window is tied to the desktop file (name,
/// icon) on either backend.
const APP_ID: &str = "io.github.labj1987.SteamPunk";

static TOKIO_RT: OnceLock<Runtime> = OnceLock::new();

pub fn runtime() -> &'static Runtime {
    TOKIO_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime")
    })
}

/// The UI toolkit reports renderer and display-server problems through the
/// `log` crate; they go to stderr so a terminal or the journal shows them.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "steam-punk: {} [{}] {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

fn main() {
    // Only the primary copy goes on. A second launch has asked the running
    // one to show its window and must not touch the log: truncating it would
    // wipe the running session's log, and with it the offsets that launch
    // diagnoses read the trainer's output back from.
    let Some(activations) = instance::claim() else {
        return;
    };
    applog::init();
    log::set_logger(&StderrLog).ok();
    // STEAM_PUNK_LOG=info (or debug) also shows which renderer was picked.
    let level = std::env::var("STEAM_PUNK_LOG")
        .ok()
        .and_then(|level| level.parse().ok())
        .unwrap_or(log::LevelFilter::Warn);
    log::set_max_level(level);
    let _ = runtime();

    gpui_kit::application()
        .with_assets(ui::Assets)
        .run(move |cx| {
            ui::init(cx);
            ui::build_ui(cx, activations);
        });
}
