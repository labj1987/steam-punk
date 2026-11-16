//! instance.rs — one running copy of the app per user session.
//!
//! The first copy listens on a socket in the user's runtime directory. A
//! second launch finds it, asks it to show its window and exits, so it never
//! starts (truncates) the session log the running copy reads trainer output
//! back from.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

/// Requests from later launches to show the running window.
pub type Activations = UnboundedReceiver<()>;

fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // SAFETY: getuid has no preconditions and cannot fail.
            std::env::temp_dir().join(format!("steam-punk-{}", unsafe { libc::getuid() }))
        });
    dir.join("steam-punk.sock")
}

/// `None` when another copy is already running (it has been asked to show
/// its window); otherwise this is the primary copy, and the receiver yields
/// once per later launch.
pub fn claim() -> Option<Activations> {
    claim_at(&socket_path())
}

fn claim_at(path: &Path) -> Option<Activations> {
    if let Ok(mut stream) = UnixStream::connect(path) {
        let _ = stream.write_all(b"activate\n");
        return None;
    }
    // Nobody answers: a socket file left by a copy that is gone.
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let (tx, rx) = unbounded_channel();
    match UnixListener::bind(path) {
        Ok(listener) => {
            std::thread::spawn(move || {
                for connection in listener.incoming() {
                    if connection.is_ok() && tx.send(()).is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => {
            // Without the socket the app still runs; it just can't be found
            // by a second launch.
            eprintln!("steam-punk: single-instance socket {}: {e}", path.display());
        }
    }
    Some(rx)
}

#[cfg(test)]
mod tests {
    use super::claim_at;

    #[test]
    fn second_launch_reaches_the_first() {
        let dir = std::env::temp_dir().join(format!("steam-punk-instance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sock");
        let mut first = claim_at(&path).expect("the first copy is the primary");
        assert!(
            claim_at(&path).is_none(),
            "a second copy finds the first and stands down"
        );
        assert_eq!(first.blocking_recv(), Some(()), "and the first is told");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_socket_file_is_replaced() {
        let dir = std::env::temp_dir().join(format!("steam-punk-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sock");
        std::fs::write(&path, b"").unwrap();
        assert!(
            claim_at(&path).is_some(),
            "a dead socket does not block startup"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
