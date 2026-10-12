//! Pins the SHA-256 of the privileged setup script and its polkit policy,
//! which the binary carries (include_bytes! in setup.rs), so root verifies
//! what it installs against sums fixed at build time.

use std::process::Command;

fn main() {
    for (var, path) in [
        ("SP_SETUP_SCRIPT_SHA256", "scripts/privileged-setup.sh"),
        ("SP_SETUP_POLICY_SHA256", "data/io.github.labj1987.SteamPunk.setup.policy"),
    ] {
        println!("cargo:rerun-if-changed={path}");
        let out = Command::new("sha256sum")
            .arg(path)
            .output()
            .unwrap_or_else(|e| panic!("running sha256sum on {path}: {e}"));
        assert!(out.status.success(), "sha256sum {path} failed");
        let text = String::from_utf8(out.stdout).expect("sha256sum output");
        let sum = text.split_whitespace().next().unwrap_or_default();
        assert!(
            sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()),
            "unexpected sha256sum output for {path}: {text:?}"
        );
        println!("cargo:rustc-env={var}={sum}");
    }
}
