//! setup.rs — one-time wine .NET prerequisite setup.
//!
//! Split into two phases matching the actual privilege boundary: system
//! packages (cabextract, curl) need root (via pkexec + a privileged script),
//! while winetricks (a pinned upstream copy kept per user, see
//! `WINETRICKS_URL`) and the install it runs only touch the user's own files
//! and the user-owned wine prefix, and need no privilege escalation.

use crate::launcher::{self, LaunchTarget, ProtonWine};
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const SCRIPT: &str = "/usr/lib/steam-punk/privileged-setup.sh";
const POLICY: &str = "/usr/share/polkit-1/actions/io.github.labj1987.SteamPunk.setup.policy";
pub const LOGFILE: &str = "/var/log/steam-punk.log";

/// Installs the privileged script and policy, then runs the script; run as
/// root through pkexec when the system copies are missing or stale.
const INSTALLER: &str = include_str!("../scripts/install-privileged.sh");

/// The privileged script and polkit policy, compiled into the binary. They
/// are installed from these bytes, never from files next to the binary
/// (an extracted AppDir is writable by the user), and root checks the
/// copies it installs against SHA-256 sums computed at build time
/// (build.rs), so a staged file that changed between the app writing it and
/// root installing it is refused. The sums travel as pkexec arguments, so they
/// do not stop another program of the same user from asking root to install
/// other files: polkit's admin authentication is the gate for that.
const SCRIPT_BYTES: &[u8] = include_bytes!("../scripts/privileged-setup.sh");
const POLICY_BYTES: &[u8] = include_bytes!("../data/io.github.labj1987.SteamPunk.setup.policy");
const SCRIPT_SHA256: &str = env!("SP_SETUP_SCRIPT_SHA256");
const POLICY_SHA256: &str = env!("SP_SETUP_POLICY_SHA256");

/// How long the .NET installer may run (it downloads about 200 MB the first
/// time), and how long its wineserver may take to shut down afterwards.
const WINETRICKS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const WINESERVER_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The winetricks verbs the automatic setup runs, and the manual commands
/// show. See `install_dotnet48` for `-f`.
const WINETRICKS_ARGS: [&str; 4] = ["-q", "-f", "dotnet48", "win10"];

/// winetricks is upstream's release script, pinned to this release and its
/// SHA-256 and kept per user under the app's data dir, never a system
/// package: Debian's and Ubuntu's winetricks package depends on the
/// distribution's whole wine, which this app never runs (winetricks runs with
/// the game's Proton wine). A system wine already installed is left alone.
const WINETRICKS_VERSION: &str = "20260125";
const WINETRICKS_URL: &str =
    "https://raw.githubusercontent.com/Winetricks/winetricks/20260125/src/winetricks";
const WINETRICKS_SHA256: &str = "431f82fc74000e6c864409f1d8fb495d696c03928808e3e8acffc45179312a7b";

/// What the privileged script installs: cabextract, which winetricks unpacks
/// the Microsoft installers with, and curl, which it downloads them with (it
/// takes aria2c, wget or curl; the winetricks package used to bring one in).
/// privileged-setup.sh runs exactly this line; a test holds the two together.
const APT_INSTALL: &str = "apt-get install -y --no-install-recommends cabextract curl";

/// The download programs winetricks can use, in its own order of preference.
const DOWNLOADERS: [&str; 3] = ["aria2c", "wget", "curl"];

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

/// Returns true if what the system setup provides is already there — skip
/// pkexec entirely if so: cabextract and a download program winetricks can
/// use. winetricks drives the game's own Proton wine (see
/// `install_dotnet48`), so no system wine packages are needed, and winetricks
/// itself is the app's own pinned copy.
pub fn system_prereqs_present() -> bool {
    let present = on_path("cabextract") && DOWNLOADERS.iter().any(|d| on_path(d));
    crate::applog::log(&format!("system_prereqs_present -> {present}"));
    present
}

fn installed_matches(path: &str, bytes: &[u8]) -> bool {
    std::fs::read(path).is_ok_and(|b| b == bytes)
}

/// Runs the privileged one-time setup via pkexec. Only ever called because
/// the user chose the automatic setup, so this is the one place a password
/// is asked for. When the system copy of the script and polkit policy is
/// current, pkexec runs the script directly and polkit shows the policy's
/// own message; otherwise the compiled-in copies are installed first, as root in
/// the same pkexec call (see `scripts/install-privileged.sh`). Bails with a
/// clear message if pkexec/polkit isn't available or the user cancels.
pub fn run_system_setup() -> Result<()> {
    if !on_path("pkexec") {
        crate::applog::log("run_system_setup: pkexec is not installed");
        bail!("{}", NO_PKEXEC_MESSAGE);
    }
    let current =
        installed_matches(SCRIPT, SCRIPT_BYTES) && installed_matches(POLICY, POLICY_BYTES);

    let (status, stderr) = if current {
        crate::applog::log("run_system_setup: launching pkexec privileged-setup.sh");
        run_pkexec(Command::new("pkexec").arg(SCRIPT))
    } else {
        // polkit can only show the policy's own message once the policy is
        // installed, so this one prompt (which installs it) is pkexec's
        // generic one for /bin/bash; every later run gets the policy's.
        crate::applog::log(
            "run_system_setup: installing the setup script and policy, then running it",
        );
        let stage = Stage::new()?;
        let staged_script = stage.0.join("privileged-setup.sh");
        let staged_policy = stage.0.join("policy");
        std::fs::write(&staged_script, SCRIPT_BYTES).context("staging the setup script")?;
        std::fs::write(&staged_policy, POLICY_BYTES).context("staging the polkit policy")?;
        run_pkexec(
            Command::new("pkexec")
                .args(["/bin/bash", "-c", INSTALLER, "bash"])
                .arg(&staged_script)
                .arg(SCRIPT)
                .arg(SCRIPT_SHA256)
                .arg(&staged_policy)
                .arg(POLICY)
                .arg(POLICY_SHA256),
        )
    }
    .context("Failed to launch pkexec — is polkit installed?")?;

    crate::applog::log(&format!("run_system_setup: pkexec exited {status:?}"));
    if !status.success() {
        bail!("{}", failure_message(status.code().unwrap_or(-1), &stderr));
    }
    Ok(())
}

/// Why the setup did not complete, from pkexec's exit `code` and what it
/// printed. 126 is a dismissed prompt (COSMIC's and GNOME's agents);
/// polkit-kde-agent reports its Cancel as 127 "Not authorized". The scripts
/// themselves never exit with either: install-privileged.sh turns a setup
/// script that could not be started into 4.
fn failure_message(code: i32, stderr: &str) -> String {
    match code {
        4 => NOT_STARTED.to_string(),
        127 if no_agent(stderr) => NO_AGENT_MESSAGE.to_string(),
        126 => CANCELLED.to_string(),
        127 if stderr.contains("Not authorized") => CANCELLED.to_string(),
        127 => "Authorization was cancelled or refused, or pkexec could not run the setup script, so nothing was changed.".to_string(),
        _ => format!("Script exited with code {code} (see {LOGFILE})"),
    }
}

const CANCELLED: &str = "Authorization was cancelled or refused, so nothing was changed.";

const NOT_STARTED: &str = "The setup script was installed but could not be started (is /usr mounted noexec?), so no packages were installed.";

/// What a system without pkexec is told. The automatic setup is only offered
/// where apt-get is (`apt_available`), so the install command is apt's.
const NO_PKEXEC_MESSAGE: &str = "polkit's pkexec is not installed, so nothing was changed. Install it with: sudo apt install pkexec (and a polkit authentication agent, if your desktop has none), then try again.";

/// pkexec's own complaint when no polkit authentication agent (the program
/// that shows the admin password dialog) is running, as on a bare Hyprland,
/// sway or i3 session. It exits 127 with this on stderr.
const NO_AGENT_TEXTS: [&str; 2] = [
    // With --disable-internal-agent, or when no agent is registered.
    "No authentication agent found",
    // What an app started from a launcher (no terminal) gets: pkexec's own
    // text agent can't open /dev/tty.
    "Error creating textual authentication agent",
];

const NO_AGENT_MESSAGE: &str = "No polkit authentication agent is running (the program that shows the admin password dialog), so nothing was changed. Start one and try again. KDE, GNOME and COSMIC include one; on Hyprland use hyprpolkitagent; on sway or i3 use polkit-gnome (policykit-1-gnome on Debian/Ubuntu) or lxqt-policykit.";

fn no_agent(stderr: &str) -> bool {
    NO_AGENT_TEXTS.iter().any(|t| stderr.contains(t))
}

/// Runs pkexec in a session of its own (see `without_terminal`), with stdin
/// from /dev/null, stdout as it was and stderr captured, which is passed on
/// to this process's own stderr afterwards so nothing is lost.
fn run_pkexec(cmd: &mut Command) -> std::io::Result<(std::process::ExitStatus, String)> {
    without_terminal(cmd);
    let out = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::piped())
        .output()?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    eprint!("{stderr}");
    Ok((out.status, stderr))
}

/// Starts `cmd` in a session of its own, with no controlling terminal. An app
/// started on a desktop that was itself started from a text console (Hyprland
/// or sway run from tty1) has that console as its controlling terminal; with
/// no polkit agent running, pkexec would then ask for the password there, on a
/// console nobody looks at, and wait for ever. Without a terminal it fails at
/// once with its "no agent" message. A running agent shows its dialog as
/// before: polkit goes by the login session, not the terminal.
fn without_terminal(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: the closure runs in the child between fork and exec and only
    // calls setsid(2), which is async-signal-safe and allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
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
    let winetricks = winetricks_path()?;
    ensure_winetricks(&winetricks)?;
    crate::applog::log(&format!(
        "install_dotnet48: running {} {} {}",
        wine.shell_env(),
        winetricks.display(),
        WINETRICKS_ARGS.join(" ")
    ));
    let mut cmd = Command::new(&winetricks);
    wine.apply(&mut cmd);
    // A pinned copy: its check for a newer release would only warn.
    cmd.env("WINETRICKS_LATEST_VERSION_CHECK", "disabled");
    cmd.args(WINETRICKS_ARGS);
    let output = match launcher::run_with_timeout(
        cmd,
        WINETRICKS_TIMEOUT,
        "The .NET installer (winetricks)",
    ) {
        Ok(o) => o,
        Err(e) => {
            // Its wine processes and their wineserver (which runs in its own
            // session) belong to this prefix alone: the game is closed.
            stop_prefix_wine(&wine);
            return Err(e);
        }
    };

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
    wait.arg("-w");
    let waited = launcher::run_with_timeout(
        wait,
        WINESERVER_EXIT_TIMEOUT,
        "Waiting for the installer's wineserver to exit",
    );
    crate::applog::log(&format!(
        "install_dotnet48: wineserver -w -> {:?}",
        waited.as_ref().map(|o| o.status)
    ));
    if let Err(e) = waited {
        stop_prefix_wine(&wine);
        bail!("{e} The prefix may be missing the last registry changes; run the setup again.");
    }

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

/// Where the pinned winetricks is kept: `~/.local/share/steam-punk/tools/`,
/// named by its release.
pub fn winetricks_path() -> Result<PathBuf> {
    Ok(crate::library::data_dir()?
        .join("tools")
        .join(format!("winetricks-{WINETRICKS_VERSION}")))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Makes `path` the pinned winetricks: kept when it already is (checked by its
/// SHA-256 on every use), else downloaded, checked and stored.
fn ensure_winetricks(path: &Path) -> Result<()> {
    if holds(path, WINETRICKS_SHA256) {
        return make_executable(path);
    }
    crate::applog::log(&format!(
        "install_dotnet48: downloading winetricks {WINETRICKS_VERSION} from {WINETRICKS_URL}"
    ));
    let bytes = download(WINETRICKS_URL)
        .with_context(|| format!("Downloading winetricks from {WINETRICKS_URL}"))?;
    store_checked(path, &bytes, WINETRICKS_SHA256)
}

fn holds(path: &Path, sha256: &str) -> bool {
    std::fs::read(path).is_ok_and(|b| sha256_hex(&b) == sha256)
}

/// Stores `bytes` at `path` as an executable, only when their SHA-256 is
/// `sha256`; nothing is written otherwise.
fn store_checked(path: &Path, bytes: &[u8], sha256: &str) -> Result<()> {
    let got = sha256_hex(bytes);
    if got != sha256 {
        bail!(
            "The downloaded winetricks is not the expected file (SHA-256 {got}, expected \
             {sha256}), so it was not used."
        );
    }
    // Created executable, so a concurrent setup never runs a copy that is
    // in place but not yet executable.
    crate::fsutil::write_atomic_mode(path, bytes, 0o755)?;
    make_executable(path)?;
    remove_other_pins(path);
    Ok(())
}

/// Deletes winetricks copies of other (earlier) pinned releases next to
/// `path`, so a pin bump doesn't leave the old one behind.
fn remove_other_pins(path: &Path) {
    let (Some(dir), Some(keep)) = (path.parent(), path.file_name()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name != keep && name.to_string_lossy().starts_with("winetricks-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", path.display()))
}

/// GETs `url` (https only) and returns the body. Runs on the app's Tokio
/// runtime when called from one of its blocking threads (the installer runs
/// on one), else on a runtime of its own.
fn download(url: &str) -> Result<Vec<u8>> {
    let fetch = async {
        let client = reqwest::Client::builder()
            .https_only(true)
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        let response = client.get(url).send().await?.error_for_status()?;
        Ok::<_, anyhow::Error>(response.bytes().await?.to_vec())
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(fetch),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(fetch),
    }
}

/// Kills every wine process in the prefix and its wineserver
/// (`wineserver -k`, Proton's own). Only used once the game is closed, when
/// whatever runs in the prefix is the installer's.
fn stop_prefix_wine(wine: &ProtonWine) {
    let mut kill = Command::new(wine.wineserver());
    wine.apply(&mut kill);
    kill.arg("-k");
    let r = launcher::run_with_timeout(
        kill,
        std::time::Duration::from_secs(30),
        "Stopping the installer's wine processes",
    );
    crate::applog::log(&format!(
        "stop_prefix_wine: wineserver -k -> {:?}",
        r.map(|o| o.status)
    ));
}

/// The last `n` lines of `text` — winetricks output runs to hundreds of
/// lines and the error is at the end.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Exactly what the automatic setup runs, for the dialog's manual
/// instructions: the system packages (with root), the pinned winetricks
/// download and its check, then winetricks with the game's Proton wine, with
/// the game closed.
pub fn manual_commands(target: &LaunchTarget, apt: bool) -> String {
    let packages = if apt {
        format!("sudo {APT_INSTALL}")
    } else {
        "# Install cabextract and curl with your distribution's package manager.".to_string()
    };
    let winetricks = match (ProtonWine::for_target(target), winetricks_path()) {
        (Ok(wine), Ok(path)) => {
            let quoted = launcher::shell_quote(&path.to_string_lossy());
            let dir = launcher::shell_quote(
                &path
                    .parent()
                    .map(|d| d.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            format!(
                "mkdir -p {dir} && curl -fLo {quoted} {WINETRICKS_URL} && \
                 printf '%s  %s\\n' {WINETRICKS_SHA256} {quoted} | sha256sum -c && chmod 755 {quoted}\n\
                 # Quit the game, then:\n{} WINETRICKS_LATEST_VERSION_CHECK=disabled {quoted} {}",
                wine.shell_env(),
                WINETRICKS_ARGS.join(" ")
            )
        }
        (Err(e), _) | (_, Err(e)) => format!("# {e}"),
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
            sandbox: None,
        }
    }

    #[test]
    fn missing_polkit_agent_is_recognised() {
        assert!(no_agent(
            "Error executing command as another user: No authentication agent found.\n"
        ));
        assert!(!no_agent(
            "Error executing command as another user: Not authorized\n"
        ));
        assert!(no_agent(
            "Error creating textual authentication agent: Error opening current controlling terminal for the process (`/dev/tty'): No such device or address\n"
        ));
        assert!(!no_agent(""));
        assert!(
            NO_AGENT_MESSAGE.contains("hyprpolkitagent")
                && NO_AGENT_MESSAGE.contains("nothing was changed")
        );
    }

    #[test]
    fn pkexec_runs_in_its_own_session_with_no_terminal_and_no_stdin() {
        // A stand-in for pkexec that reports its PID, session ID, terminal
        // and stdin on stderr, which run_pkexec captures.
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "read -r line < /proc/$$/stat; set -- ${line##*) }; echo \"$$ $4 $5 $(readlink /proc/$$/fd/0)\" >&2; exit 126",
        ]);
        let (status, stderr) = run_pkexec(&mut cmd).unwrap();
        assert_eq!(status.code(), Some(126), "pkexec's exit status comes back");
        let fields: Vec<&str> = stderr.split_whitespace().collect();
        assert_eq!(fields.len(), 4, "{stderr}");
        assert_eq!(fields[1], fields[0], "a session of its own: {stderr}");
        assert_eq!(fields[2], "0", "no controlling terminal: {stderr}");
        assert_eq!(fields[3], "/dev/null", "{stderr}");
    }

    #[test]
    fn the_setup_failure_reads_right_for_every_desktop() {
        // COSMIC's and GNOME's agents: 126 for a dismissed prompt.
        assert_eq!(
            failure_message(
                126,
                "Error executing command as another user: Request dismissed\n"
            ),
            CANCELLED
        );
        // polkit-kde-agent's Cancel: 127 "Not authorized".
        assert_eq!(
            failure_message(
                127,
                "Error executing command as another user: Not authorized\n\nThis incident has been reported.\n"
            ),
            CANCELLED
        );
        assert!(CANCELLED.contains("cancelled or refused"));
        assert_eq!(
            failure_message(127, "Error creating textual authentication agent: Error opening current controlling terminal for the process (`/dev/tty'): No such device or address\n"),
            NO_AGENT_MESSAGE
        );
        assert_eq!(
            failure_message(127, "No authentication agent found.\n"),
            NO_AGENT_MESSAGE
        );
        assert!(failure_message(127, "pkexec: something else\n")
            .starts_with("Authorization was cancelled or refused, or pkexec could not run"));
        assert_eq!(
            failure_message(3, ""),
            format!("Script exited with code 3 (see {LOGFILE})")
        );
    }

    #[test]
    fn a_missing_pkexec_is_named_with_the_install_command() {
        assert!(NO_PKEXEC_MESSAGE
            .starts_with("polkit's pkexec is not installed, so nothing was changed. "));
        assert!(NO_PKEXEC_MESSAGE.contains("sudo apt install pkexec"));
        assert!(!NO_PKEXEC_MESSAGE.contains("uthorization"));
    }

    #[test]
    fn manual_commands_match_the_automatic_setup() {
        let dir =
            std::env::temp_dir().join(format!("steam-punk-setup-manual-{}", std::process::id()));
        let proton = dir.join("Proton - Experimental");
        std::fs::create_dir_all(proton.join("files/bin")).unwrap();
        std::fs::write(proton.join("files/bin/wine"), b"").unwrap();
        std::fs::write(proton.join("files/bin/wineserver"), b"").unwrap();

        let text = manual_commands(&target(&proton), true);
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(format!("sudo {APT_INSTALL}").as_str()));
        let wt = winetricks_path().unwrap();
        let wt_quoted = launcher::shell_quote(&wt.to_string_lossy());
        let fetch = lines.next().unwrap();
        assert!(
            fetch.contains(&format!("curl -fLo {wt_quoted} {WINETRICKS_URL} && "))
                && fetch.contains(&format!(
                    "printf '%s  %s\\n' {WINETRICKS_SHA256} {wt_quoted} | sha256sum -c"
                )),
            "{fetch}"
        );
        assert_eq!(lines.next(), Some("# Quit the game, then:"));
        let cmd = lines.next().unwrap();
        let run = format!(" {wt_quoted} -q -f dotnet48 win10");
        assert!(cmd.ends_with(&run), "{cmd}");
        assert!(
            cmd.contains(" WINETRICKS_LATEST_VERSION_CHECK=disabled "),
            "{cmd}"
        );
        assert!(
            cmd.contains("WINEPREFIX='/mnt/My Games/steamapps/compatdata/1/pfx'"),
            "{cmd}"
        );
        let wine = format!("{}/files/bin/wine", proton.display());
        assert!(cmd.contains(&format!("WINE='{wine}'")), "{cmd}");
        assert!(cmd.contains(&format!("WINESERVER='{wine}server'")), "{cmd}");
        assert!(cmd.starts_with("env -u WINEARCH "), "{cmd}");

        // The command really sets that environment when run by a shell.
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd.replace(&run, " sh -c 'printf %s \"$WINE|$PATH\"'"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("{wine}|{}/files/bin:/usr/bin:/bin", proton.display())
        );

        let other = manual_commands(&target(&proton), false);
        assert!(
            other.starts_with("# Install cabextract and curl"),
            "{other}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn manual_commands_explain_a_proton_build_without_wine() {
        let text = manual_commands(&target(Path::new("/nonexistent/Proton 9.0")), true);
        assert!(text.contains("has no wine"), "{text}");
    }

    #[test]
    fn the_privileged_script_runs_exactly_the_install_line() {
        let script = std::str::from_utf8(SCRIPT_BYTES).unwrap();
        let line = APT_INSTALL.replacen("apt-get ", "apt-get \"${APT_OPTS[@]}\" ", 1);
        assert!(
            script
                .lines()
                .any(|l| l.contains(&format!("if ! {line} >&3"))),
            "privileged-setup.sh no longer runs `{APT_INSTALL}`"
        );
        assert!(!script.contains("winetricks cabextract"));
        for script in [script, INSTALLER] {
            assert!(
                script.lines().any(|l| l == "umask 022"),
                "both root scripts set a strict umask"
            );
        }
    }

    #[test]
    fn the_winetricks_download_is_used_only_when_its_sha256_matches() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let dir = std::env::temp_dir().join(format!("steam-punk-setup-wt-{}", std::process::id()));
        let path = dir.join("tools/winetricks-test");
        let body = b"#!/bin/sh\necho pinned\n";
        let sum = sha256_hex(body);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        let err = store_checked(&path, b"#!/bin/sh\necho other\n", &sum).unwrap_err();
        assert!(err.to_string().contains("not the expected file"), "{err}");
        assert!(!path.exists(), "nothing is stored");
        let old = dir.join("tools/winetricks-20240101");
        let unrelated = dir.join("tools/notes.txt");
        std::fs::write(&old, b"old pin").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();
        store_checked(&path, body, &sum).unwrap();
        assert!(holds(&path, &sum));
        assert!(!old.exists(), "an earlier pinned release is removed");
        assert!(unrelated.exists(), "other files are left alone");
        let out = Command::new(&path).output().unwrap();
        assert_eq!(out.stdout, b"pinned\n", "stored as an executable");
        std::fs::write(&path, b"changed").unwrap();
        assert!(!holds(&path, &sum), "a changed copy is downloaded again");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(WINETRICKS_SHA256.len(), 64);
        assert!(WINETRICKS_URL.contains(&format!("/{WINETRICKS_VERSION}/")));
    }

    #[test]
    fn a_setup_script_that_cannot_start_is_not_a_cancel() {
        assert_eq!(failure_message(4, ""), NOT_STARTED);
        assert!(!NOT_STARTED.contains("cancelled"));
        // The installer, run against scratch destinations, with a setup script whose
        // interpreter is missing: it is installed (0755, its directory too), then
        // can't start, which is 4.
        let dir =
            std::env::temp_dir().join(format!("steam-punk-setup-inst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = b"#!/nonexistent/interpreter\n";
        let policy = b"<policyconfig/>\n";
        std::fs::write(dir.join("staged-script"), script).unwrap();
        std::fs::write(dir.join("staged-policy"), policy).unwrap();
        let installed = dir.join("lib/steam-punk/privileged-setup.sh");
        let run = |script_sum: &str| {
            Command::new("bash")
                .args(["-c", INSTALLER, "bash"])
                .arg(dir.join("staged-script"))
                .arg(&installed)
                .arg(script_sum)
                .arg(dir.join("staged-policy"))
                .arg(dir.join("actions/policy"))
                .arg(sha256_hex(policy))
                .output()
                .unwrap()
        };
        let out = run(&sha256_hex(b"something else"));
        assert_eq!(
            out.status.code(),
            Some(1),
            "a changed staged file is refused"
        );
        assert!(!installed.exists());
        let out = run(&sha256_hex(script));
        assert_eq!(out.status.code(), Some(4), "{out:?}");
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&installed), 0o755);
        assert_eq!(mode(installed.parent().unwrap()), 0o755);
        assert_eq!(mode(&dir.join("actions/policy")), 0o644);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pinned_hashes_match_the_compiled_in_files() {
        for (bytes, sum) in [(SCRIPT_BYTES, SCRIPT_SHA256), (POLICY_BYTES, POLICY_SHA256)] {
            let mut child = Command::new("sha256sum")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(bytes).unwrap();
            let out = child.wait_with_output().unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next(),
                Some(sum)
            );
        }
    }

    #[test]
    fn last_lines_keeps_the_end() {
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a", 5), "a");
    }
}
