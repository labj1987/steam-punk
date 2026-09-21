//! setup.rs — one-time wine .NET prerequisite setup.
//!
//! Split into two phases matching the actual privilege boundary: system
//! packages need root (via pkexec + a privileged script, same pattern as
//! KernelPop's install.rs), while the winetricks install itself only touches the
//! user-owned wine prefix and needs no privilege escalation.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::Command;

const SCRIPT: &str = "/usr/lib/steampunk/privileged-setup.sh";
pub const LOGFILE: &str = "/var/log/steampunk.log";

/// True on Debian-family systems: `privileged-setup.sh` is apt-only, so the
/// automatic system setup is only offered where `apt-get` exists.
pub fn apt_available() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join("apt-get").is_file()))
        .unwrap_or(false)
        || Path::new("/usr/bin/apt-get").is_file()
}

/// Returns true if the one-time system packages are already present — skip
/// pkexec entirely if so. Both checks are quick and need no root.
pub fn system_prereqs_present() -> bool {
    // wine32:i386 is a Debian-family package; other distros have no dpkg, so
    // only winetricks itself can be checked there.
    let wine32 = !apt_available()
        || Command::new("dpkg")
            .args(["-s", "wine32:i386"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    let present = wine32
        && Command::new("which")
            .arg("winetricks")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    crate::applog::log(&format!("system_prereqs_present -> {present}"));
    present
}

/// Runs the privileged one-time setup via pkexec. Same pattern as KernelPop's
/// install.rs — bails with a clear message if pkexec/polkit isn't available
/// or the user cancels the auth prompt.
pub fn run_system_setup() -> Result<()> {
    crate::applog::log("run_system_setup: launching pkexec privileged-setup.sh");
    if !Path::new(SCRIPT).exists() {
        crate::applog::log("run_system_setup: script not found");
        bail!("Privileged script not found at {}", SCRIPT);
    }
    let status = Command::new("pkexec")
        .arg(SCRIPT)
        .status()
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

/// Runs `winetricks -q -f dotnet48 win10` against the target prefix. No
/// privilege escalation — this only touches files the user already owns.
/// `-f` (force) matters: winetricks marks a verb done in the prefix's
/// winetricks.log the first time it's attempted, regardless of whether the
/// install actually completed successfully (observed: dotnet48 logged as
/// done while system32/mscoree.dll was still Wine's non-functional builtin
/// stub) — without `-f` a retry here would silently no-op against a prefix
/// this dialog exists specifically to repair. Output is captured (rather
/// than just the exit status) so a failure can be surfaced to the user with
/// the actual winetricks error, and appended to the same log the privileged
/// script writes to, for a single place to look.
pub fn install_dotnet48(prefix_dir: &Path) -> Result<()> {
    crate::applog::log(&format!(
        "install_dotnet48: running winetricks -q -f dotnet48 win10 (WINEPREFIX={})",
        prefix_dir.display()
    ));
    let output = Command::new("winetricks")
        .env("WINEPREFIX", prefix_dir)
        .args(["-q", "-f", "dotnet48", "win10"])
        .output()
        .context("Failed to launch winetricks — is it installed?")?;

    append_to_log(&output.stdout, &output.stderr);
    crate::applog::log(&format!(
        "install_dotnet48: winetricks exited {:?}",
        output.status
    ));

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
            tail.trim()
        );
    }
    Ok(())
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
    let _ = writeln!(f, "[steampunk] ==== winetricks dotnet48 output ====");
    let _ = f.write_all(stdout);
    let _ = f.write_all(stderr);
}
