//! setup.rs — one-time wine .NET prerequisite setup.
//!
//! Split into two phases matching the actual privilege boundary: system
//! packages need root (via pkexec + a privileged script), while the winetricks
//! install itself only touches the user-owned wine prefix and needs no
//! privilege escalation.

use crate::launcher::{self, LaunchTarget, ProtonWine};
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const SCRIPT: &str = "/usr/lib/steam-punk/privileged-setup.sh";
const POLICY: &str = "/usr/share/polkit-1/actions/io.github.labj1987.SteamPunk.setup.policy";
pub const LOGFILE: &str = "/var/log/steam-punk.log";

/// Installs the bundled privileged script and policy, then runs the script;
/// run as root through pkexec when the system copies are missing or stale.
const INSTALLER: &str = include_str!("../scripts/install-privileged.sh");

/// The winetricks verbs the automatic setup runs, and the manual commands
/// show. See `install_dotnet48` for `-f`.
const WINETRICKS_ARGS: [&str; 4] = ["-q", "-f", "dotnet48", "win10"];

/// What the privileged script installs (and `system_prereqs_present` checks).
const APT_INSTALL: &str = "apt-get install -y --no-install-recommends winetricks cabextract";

/// Shown when the winetricks fallback is reached while the game still runs.
pub const GAME_RUNNING: &str = "The game is still running. The .NET installer can only \
change this game's prefix while nothing is using it: quit the game, then try again.";

/// True on Debian-family systems: `privileged-setup.sh` is apt-only, so the
/// automatic system setup is only offered where `apt-get` exists.
pub fn apt_available() -> bool {
    on_path("apt-get") || Path::new("/usr/bin/apt-get").is_file()
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}

/// Returns true if the one-time system packages are already present — skip
/// pkexec entirely if so. winetricks drives the game's own Proton wine (see
/// `install_dotnet48`), so no system wine packages are needed: only
/// winetricks and the cabextract it unpacks the Microsoft installers with.
pub fn system_prereqs_present() -> bool {
    let present = on_path("winetricks") && on_path("cabextract");
    crate::applog::log(&format!("system_prereqs_present -> {present}"));
    present
}

/// The privileged script and policy inside the running AppImage.
struct Bundled {
    script: PathBuf,
    policy: PathBuf,
}

fn bundled() -> Option<Bundled> {
    let appdir = PathBuf::from(std::env::var_os("APPDIR")?);
    let b = Bundled {
        script: appdir.join("usr/lib/steam-punk/privileged-setup.sh"),
        policy: appdir.join("usr/share/polkit-1/actions/io.github.labj1987.SteamPunk.setup.policy"),
    };
    (b.script.is_file() && b.policy.is_file()).then_some(b)
}

fn same_contents(a: &Path, b: &Path) -> bool {
    matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

fn sha256(path: &Path) -> Result<String> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .context("running sha256sum")?;
    if !out.status.success() {
        bail!("sha256sum {} failed", path.display());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .next()
        .map(str::to_string)
        .with_context(|| format!("no checksum for {}", path.display()))
}

/// Runs the privileged one-time setup via pkexec. Only ever called because
/// the user chose the automatic setup, so this is the one place a password
/// is asked for. When the system copy of the script and polkit policy is
/// current, pkexec runs the script directly and polkit shows the policy's
/// own message; otherwise the bundled copies are installed first, as root in
/// the same pkexec call (see `scripts/install-privileged.sh`). Bails with a
/// clear message if pkexec/polkit isn't available or the user cancels.
pub fn run_system_setup() -> Result<()> {
    let bundle = bundled();
    let current = match &bundle {
        Some(b) => same_contents(&b.script, Path::new(SCRIPT)) && same_contents(&b.policy, Path::new(POLICY)),
        None => Path::new(SCRIPT).is_file(),
    };

    let status = if current {
        crate::applog::log("run_system_setup: launching pkexec privileged-setup.sh");
        Command::new("pkexec").arg(SCRIPT).status()
    } else {
        let Some(b) = bundle else {
            crate::applog::log("run_system_setup: script not found");
            bail!(
                "The setup script isn't installed at {SCRIPT}, and only the Steam Punk AppImage \
                 carries it. Run the AppImage, or use the manual commands."
            );
        };
        crate::applog::log("run_system_setup: installing the bundled setup script and policy, then running it");
        let stage = Stage::new()?;
        let staged_script = stage.0.join("privileged-setup.sh");
        let staged_policy = stage.0.join("policy");
        std::fs::copy(&b.script, &staged_script).context("staging the setup script")?;
        std::fs::copy(&b.policy, &staged_policy).context("staging the polkit policy")?;
        Command::new("pkexec")
            .args(["/bin/bash", "-c", INSTALLER, "bash"])
            .arg(&b.script)
            .arg(&staged_script)
            .arg(SCRIPT)
            .arg(sha256(&b.script)?)
            .arg(&b.policy)
            .arg(&staged_policy)
            .arg(POLICY)
            .arg(sha256(&b.policy)?)
            .status()
    }
    .context("Failed to launch pkexec — is polkit installed?")?;

    crate::applog::log(&format!("run_system_setup: pkexec exited {status:?}"));
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        if code == 126 || code == 127 {
            bail!("Authentication was cancelled.");
        }
        bail!("Script exited with code {} (see {})", code, LOGFILE);
    }
    Ok(())
}

/// A private temp dir for the staged copies, removed on drop.
struct Stage(PathBuf);

impl Stage {
    fn new() -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!(
            "steam-punk-setup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `winetricks -q -f dotnet48 win10` against the game's prefix, with the
/// game's own Proton wine (`ProtonWine`) — never the system wine, which is a
/// different wine build than the one that made and runs this prefix. No
/// privilege escalation — this only touches files the user already owns.
///
/// The game must not be running. winetricks waits for the prefix's
/// wineserver to exit between steps (`wineserver -w`), which a running game
/// never lets happen, and the Microsoft installer must not change a prefix a
/// game is using. Fails with `GAME_RUNNING` instead of starting then.
///
/// `-f` (force) matters: winetricks marks a verb done in the prefix's
/// winetricks.log the first time it's attempted, regardless of whether the
/// install actually completed successfully (observed: dotnet48 logged as
/// done while system32/mscoree.dll was still Wine's non-functional builtin
/// stub) — without `-f` a retry here would silently no-op against a prefix
/// this dialog exists specifically to repair. Output is captured (rather
/// than just the exit status) so a failure can be surfaced to the user with
/// the actual winetricks error, and appended to the same log the privileged
/// script writes to, for a single place to look.
pub fn install_dotnet48(target: &LaunchTarget) -> Result<()> {
    let wine = ProtonWine::for_target(target)?;
    let prefix = target.prefix_dir();
    if launcher::prefix_in_use(&prefix) {
        crate::applog::log("install_dotnet48: the prefix's wineserver is still running");
        bail!(GAME_RUNNING);
    }
    crate::applog::log(&format!(
        "install_dotnet48: running {} winetricks {}",
        wine.shell_env(),
        WINETRICKS_ARGS.join(" ")
    ));
    let mut cmd = Command::new("winetricks");
    wine.apply(&mut cmd);
    let output = cmd
        .args(WINETRICKS_ARGS)
        .stdin(std::process::Stdio::null())
        .output()
        .context("Failed to launch winetricks — is it installed?")?;

    append_to_log(&output.stdout, &output.stderr);
    crate::applog::log(&format!(
        "install_dotnet48: winetricks exited {:?}",
        output.status
    ));

    // The wineserver winetricks started writes the registry out when it
    // exits, a few seconds after the last wine process; wait for that, so the
    // check that follows reads the final state.
    let mut wait = Command::new(wine.wineserver());
    wine.apply(&mut wait);
    let waited = wait.arg("-w").status();
    crate::applog::log(&format!("install_dotnet48: wineserver -w -> {waited:?}"));

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = if stderr.trim().is_empty() {
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else {
            stderr.into_owned()
        };
        bail!(
            "winetricks exited with code {:?} installing dotnet48: {}",
            output.status.code(),
            last_lines(tail.trim(), 15)
        );
    }
    Ok(())
}

/// The last `n` lines of `text` — winetricks output runs to hundreds of
/// lines and the error is at the end.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Exactly what the automatic setup runs, for the dialog's manual
/// instructions: the system packages (with root), then winetricks with the
/// game's Proton wine, with the game closed.
pub fn manual_commands(target: &LaunchTarget, apt: bool) -> String {
    let packages = if apt {
        format!("sudo {APT_INSTALL}")
    } else {
        "# Install winetricks and cabextract with your distribution's package manager.".to_string()
    };
    let winetricks = match ProtonWine::for_target(target) {
        Ok(wine) => format!(
            "# Quit the game, then:\n{} winetricks {}",
            wine.shell_env(),
            WINETRICKS_ARGS.join(" ")
        ),
        Err(e) => format!("# {e}"),
    };
    format!("{packages}\n{winetricks}")
}

/// Best-effort: the log file is created root-owned by privileged-setup.sh,
/// so appending as a normal user may not be possible — that's fine, the
/// caller still gets the output via the returned Result.
fn append_to_log(stdout: &[u8], stderr: &[u8]) {
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOGFILE)
    else {
        return;
    };
    let _ = writeln!(f, "[steam-punk] ==== winetricks dotnet48 output ====");
    let _ = f.write_all(stdout);
    let _ = f.write_all(stderr);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(proton: &Path) -> LaunchTarget {
        LaunchTarget {
            appid: 1,
            client_dir: PathBuf::from("/home/u/.local/share/Steam"),
            compatdata_dir: PathBuf::from("/mnt/My Games/steamapps/compatdata/1"),
            proton_dir: proton.to_path_buf(),
        }
    }

    #[test]
    fn manual_commands_match_the_automatic_setup() {
        let dir = std::env::temp_dir().join(format!("steam-punk-setup-manual-{}", std::process::id()));
        let proton = dir.join("Proton - Experimental");
        std::fs::create_dir_all(proton.join("files/bin")).unwrap();
        std::fs::write(proton.join("files/bin/wine"), b"").unwrap();
        std::fs::write(proton.join("files/bin/wineserver"), b"").unwrap();

        let text = manual_commands(&target(&proton), true);
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(format!("sudo {APT_INSTALL}").as_str()));
        assert_eq!(lines.next(), Some("# Quit the game, then:"));
        let cmd = lines.next().unwrap();
        assert!(cmd.ends_with(" winetricks -q -f dotnet48 win10"), "{cmd}");
        assert!(cmd.contains("WINEPREFIX='/mnt/My Games/steamapps/compatdata/1/pfx'"), "{cmd}");
        let wine = format!("{}/files/bin/wine", proton.display());
        assert!(cmd.contains(&format!("WINE='{wine}'")), "{cmd}");
        assert!(cmd.contains(&format!("WINESERVER='{wine}server'")), "{cmd}");
        assert!(cmd.starts_with("env -u WINEARCH "), "{cmd}");

        // The command really sets that environment when run by a shell.
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd.replace(" winetricks -q -f dotnet48 win10", " sh -c 'printf %s \"$WINE|$PATH\"'"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("{wine}|{}/files/bin:/usr/bin:/bin", proton.display())
        );

        let other = manual_commands(&target(&proton), false);
        assert!(other.starts_with("# Install winetricks and cabextract"), "{other}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn manual_commands_explain_a_proton_build_without_wine() {
        let text = manual_commands(&target(Path::new("/nonexistent/Proton 9.0")), true);
        assert!(text.contains("has no bin/wine"), "{text}");
    }

    #[test]
    fn last_lines_keeps_the_end() {
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a", 5), "a");
    }
}
