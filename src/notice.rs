//! notice.rs — telling the user something when the app has no window to say
//! it in: no working graphics driver, or a running copy that does not answer.
//! It goes through whatever the desktop offers, tried in order until one
//! shows it: a dialog (zenity, kdialog), a notification (notify-send, or the
//! notification service straight over D-Bus with gdbus, which desktops
//! without notify-send such as Pop!_OS COSMIC still have), or xmessage. The
//! AppRun's `libcheck_show` (build-appimage.sh) tries the same programs in
//! the same order; a test holds the two together.

/// Shows `message` as an error, when there is a display to show it on.
pub fn show_error(message: &str) {
    if !has_display() {
        return;
    }
    for (program, args) in commands(message) {
        let status = std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if status.is_ok_and(|status| showed(program, status.code())) {
            break;
        }
    }
}

/// Whether there is a display server to show anything on.
pub fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// The programs tried, in order, with their arguments. The notification
/// sent over D-Bus has no expiry (0): COSMIC drops one with the default
/// after a few seconds, before anyone has read an error.
fn commands(message: &str) -> Vec<(&'static str, Vec<String>)> {
    let plain = plain(message);
    let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let with = |mut list: Vec<String>, last: &str| {
        list.push(last.to_string());
        list
    };
    vec![
        (
            "zenity",
            with(
                args(&[
                    "--error",
                    "--no-markup",
                    "--title=Steam Punk",
                    "--width=480",
                    "--text",
                ]),
                message,
            ),
        ),
        (
            "kdialog",
            with(args(&["--title", "Steam Punk", "--error"]), &plain),
        ),
        (
            "notify-send",
            with(
                args(&["--urgency=critical", "--app-name=Steam Punk", "Steam Punk"]),
                &plain,
            ),
        ),
        (
            "gdbus",
            with(
                args(&[
                    "call",
                    "--session",
                    "--dest",
                    "org.freedesktop.Notifications",
                    "--object-path",
                    "/org/freedesktop/Notifications",
                    "--method",
                    "org.freedesktop.Notifications.Notify",
                    "'Steam Punk'",
                    "uint32 0",
                    "''",
                    "'Steam Punk'",
                ]),
                &gvariant_string(&plain),
            )
            .into_iter()
            .chain(args(&["@as []", "{'urgency': <byte 2>}", "int32 0"]))
            .collect(),
        ),
        (
            "xmessage",
            with(args(&["-center", "-title", "Steam Punk"]), message),
        ),
    ]
}

/// Whether `program`, having ended with exit `code`, did show the message, so
/// the next one must not show it again. The dialogs end with 0 when the user
/// pressed their button and 1 when the user closed them another way; both
/// mean it was seen. Anything else (no display, a crash, a signal) does not.
/// notify-send and gdbus show nothing themselves: only 0 says the desktop
/// took the message. xmessage ends with 1 when it could not open the display.
fn showed(program: &str, code: Option<i32>) -> bool {
    match program {
        "notify-send" | "gdbus" | "xmessage" => code == Some(0),
        _ => matches!(code, Some(0 | 1)),
    }
}

/// `text` with nothing kdialog (Qt rich text) or a notification server (body
/// markup) would take as markup: no &, < or >.
fn plain(text: &str) -> String {
    text.replace('&', "and").replace('<', "‹").replace('>', "›")
}

/// `text` as a GVariant text-format string, which is how gdbus reads its
/// arguments.
fn gvariant_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_way_to_show_a_message_is_tried_in_the_apprun_order() {
        let names: Vec<_> = commands("x")
            .into_iter()
            .map(|(program, _)| program)
            .collect();
        assert_eq!(
            names,
            ["zenity", "kdialog", "notify-send", "gdbus", "xmessage"]
        );
        // The AppRun's libcheck_show tries them in the same order.
        let script = include_str!("../build-appimage.sh");
        let show = &script[script.find("libcheck_show() {").unwrap()..];
        let show = &show[..show.find("\n}\n").unwrap()];
        let order: Vec<usize> = names
            .iter()
            .map(|name| show.find(&format!("command -v {name} ")).expect(name))
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    }

    #[test]
    fn only_a_shown_message_stops_the_chain() {
        assert!(showed("zenity", Some(0)));
        assert!(showed("zenity", Some(1)), "closed with the window's X");
        assert!(showed("kdialog", Some(1)));
        assert!(!showed("zenity", Some(255)), "could not open a display");
        assert!(!showed("kdialog", None), "killed by a signal");
        assert!(showed("notify-send", Some(0)));
        assert!(!showed("notify-send", Some(1)), "no notification service");
        assert!(showed("gdbus", Some(0)));
        assert!(!showed("gdbus", Some(1)), "no notification service");
        assert!(showed("xmessage", Some(0)));
        assert!(!showed("xmessage", Some(1)), "could not open the display");
    }

    #[test]
    fn gdbus_gets_the_message_as_one_quoted_string_with_no_expiry() {
        let (_, args) = commands("it's \"odd\" <b> & a\\b\nline 2")
            .into_iter()
            .find(|(program, _)| *program == "gdbus")
            .unwrap();
        assert!(args
            .windows(2)
            .any(|w| w == ["--method", "org.freedesktop.Notifications.Notify"]));
        // Notify's eight arguments: app, replaces, icon, summary, body, actions, hints, expiry.
        let notify = &args[args.len() - 8..];
        assert_eq!(notify[4], r#"'it\'s "odd" ‹b› and a\\b\nline 2'"#);
        assert_eq!(notify[7], "int32 0", "stays up until it is closed");
        assert_eq!(args.len(), 16, "{args:?}");
        assert_eq!(gvariant_string("a\tb\r"), r"'a\tb\r'");
    }

    #[test]
    fn the_dialogs_get_the_message_as_it_is_and_the_others_without_markup() {
        let commands = commands("a <b> & c");
        let last = |program: &str| {
            commands
                .iter()
                .find(|(name, _)| *name == program)
                .and_then(|(_, args)| args.last().cloned())
                .unwrap()
        };
        assert_eq!(last("zenity"), "a <b> & c", "--no-markup shows it as it is");
        assert_eq!(last("xmessage"), "a <b> & c");
        assert_eq!(last("kdialog"), "a ‹b› and c");
        assert_eq!(last("notify-send"), "a ‹b› and c");
    }
}
