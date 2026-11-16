use crate::applog;
use crate::gamedata;
use crate::instance::Activations;
use crate::launcher::{self, LaunchTarget};
use crate::library::{self, Trainer};
use crate::setup;
use crate::steam;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::link::Link;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::text::TextView;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Icon, Root, Sizable as _, Theme, ThemeMode, ThemeRegistry,
    TitleBar, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    accesskit, div, img, px, size, Anchor, App, AppContext as _, AssetSource, Bounds,
    ClipboardItem, Context, Div, Element, ElementId, Entity, ExternalPaths, FocusHandle,
    Focusable as _, FontWeight, GlobalElementId, Image, ImageFormat, InspectorElementId,
    InteractiveElement as _, IntoElement, LayoutId, MouseButton, ObjectFit, ParentElement as _,
    PathPromptOptions, Pixels, Position, Render, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Style, Styled as _, StyledImage as _, Subscription, Task,
    Text, WeakEntity, Window, WindowBounds, WindowDecorations, WindowOptions,
};

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// Cover-art slot in the trainer list, the shape of Steam's 460x215 header art.
const COVER_WIDTH: f32 = 107.;
const COVER_HEIGHT: f32 = 50.;

const WEBSITE: &str = "https://github.com/labj1987/steam-punk";
const ISSUES: &str = "https://github.com/labj1987/steam-punk/issues";

// ─────────────────────────────────────────────────────────────────────────────
//  Assets: GPUI Kit's component icons plus the two this app adds.
// ─────────────────────────────────────────────────────────────────────────────

gpui_kit::assets::icon_assets!(ExtraIcons, [Gamepad2, Trash]);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Async bridge: run a future on the shared Tokio runtime, deliver the result
//  back on the UI thread, in the window it was started from.
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_async<F, T, CB>(window: &Window, cx: &App, future: F, callback: CB)
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
    CB: FnOnce(T, &mut Window, &mut App) + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel::<T>();
    crate::runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    window
        .spawn(cx, async move |cx| {
            if let Ok(val) = rx.await {
                let _ = cx.update(|window, cx| callback(val, window, cx));
            }
        })
        .detach();
}

/// A message this long (a setup failure with its explanation) stays until it
/// is closed; shorter ones go away on their own.
const LONG_TOAST: usize = 120;

// Notifications show their text as plain text, so trainer names and error
// text (Wine output is full of `<null>`) need no escaping.
//
// False when the message could not be shown: a window has no place for
// notifications until the toolkit's root view is in it, which is after the
// main view is built (pushing one earlier panics in the toolkit).
fn toast(window: &mut Window, cx: &mut App, text: &str) {
    show_toast(window, cx, text);
}

fn show_toast(window: &mut Window, cx: &mut App, text: &str) -> bool {
    if window.root::<Root>().flatten().is_none() {
        applog::log(&format!("UI: not shown (the window is not up yet): {text}"));
        return false;
    }
    window.push_notification(
        Notification::new()
            .message(text.to_string())
            // At the bottom, clear of the first rows' Launch and Stop buttons.
            .placement(Anchor::BottomCenter)
            .autohide(text.chars().count() <= LONG_TOAST),
        cx,
    );
    announce(text);
    window.refresh();
    true
}

// ─────────────────────────────────────────────────────────────────────────────
//  Screen readers. GPUI reports an element to assistive technology only when
//  it has an id and a role, so the text, dialog names and announcements below
//  are spelled out; a bare string child is drawn but never read.
// ─────────────────────────────────────────────────────────────────────────────

/// Text a screen reader can read (a label node holding the text).
fn label(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Text {
    Text::new(id.into(), text.into())
}

/// GPUI Kit 0.9.1 gives a dialog no accessible name and has no way to set
/// one, so everything in a dialog, its buttons included, sits in a group
/// named after the title: a screen reader says the name when the keyboard
/// focus moves into the group, which is when the dialog opens.
fn named_group(id: &'static str, name: impl Into<SharedString>) -> Stateful<Div> {
    v_flex().id(id).role(Role::Group).aria_label(name).gap_3()
}

// Every dialog is opened with `.overlay(false)`: the toolkit's dimming layer
// (0.9.1) also covers the invisible margin a client-decorated window keeps
// around itself for its shadow, which shows as a dark frame around the
// window on KDE and GNOME. Without it a dialog is as modal as with it (it
// takes the input and the focus); gone are the dimming, click-outside-to-close
// and dragging the window by its title bar or the dimmed area, for which the
// dialog's own title stands in (`dialog_title`).

/// Puts the keyboard focus on a dialog's main button when the dialog opens.
/// Left alone the focus sits on the dialog itself, which has no name; on a
/// button it is inside the named group, so a screen reader announces the
/// title, and Space acts on the button straight away. Enter goes to the
/// dialog first, which acts as the focused button would (see `EnterKey`).
struct InitialFocus {
    handle: FocusHandle,
    pending: Cell<bool>,
}

impl InitialFocus {
    fn new(cx: &mut App) -> Rc<Self> {
        Rc::new(Self {
            handle: cx.focus_handle(),
            pending: Cell::new(true),
        })
    }

    /// Call from the dialog's builder, which runs on every frame: only the
    /// first call moves the focus.
    fn apply(&self, window: &mut Window, cx: &mut App) {
        if self.pending.replace(false) {
            self.handle.focus(window, cx);
        }
    }
}

/// The height a dialog's text may take before it scrolls. The toolkit cuts
/// off a dialog that is taller than the window instead of scrolling it
/// (0.9.1), which would take the buttons out of reach, so the part above
/// the buttons gets what the window leaves after the dialog's margin, title
/// and button row.
fn dialog_room(window: &Window) -> Pixels {
    (window.viewport_size().height - px(DIALOG_CHROME)).max(px(60.))
}

/// What a dialog needs of the window's height besides its text.
const DIALOG_CHROME: f32 = 220.;

/// The trainers.json problem to tell the user about: the current one, unless
/// it is the one already shown.
fn unreported<'a>(current: &'a Option<String>, shown: &Option<String>) -> Option<&'a String> {
    current.as_ref().filter(|_| current != shown)
}

/// What Enter does in a dialog. The toolkit hands Enter to the dialog (its
/// "confirm") before the focused button sees it, so the dialog looks at
/// where the focus is and acts as that button would: Enter on Cancel
/// cancels, Enter on the action confirms, and Enter anywhere else (the close
/// button, a disclosure) is not a decision.
#[derive(Clone, Copy, Debug, PartialEq)]
enum EnterKey {
    Confirm,
    Cancel,
    Nothing,
}

fn enter_key(action_focused: bool, cancel_focused: bool) -> EnterKey {
    if action_focused {
        EnterKey::Confirm
    } else if cancel_focused {
        EnterKey::Cancel
    } else {
        EnterKey::Nothing
    }
}

/// Which button of a question has the focus when it opens, and so which one
/// Enter and Space act on if the user just presses them.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DefaultButton {
    Action,
    Cancel,
}

/// A dialog's title, which is also the handle to move the window by: while a
/// dialog is open the toolkit keeps the pointer off everything under it, the
/// window's own title bar included (its dimming layer would take the drag,
/// but see `.overlay(false)` above).
fn dialog_title(title: impl Into<SharedString>) -> impl IntoElement {
    div()
        .w_full()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
        .child(label("title", title))
}

/// The button row at the bottom of a dialog.
fn dialog_buttons() -> Div {
    h_flex().justify_end().gap_2().pt_1()
}

thread_local! {
    /// The newest notification, for `Announcer`.
    static ANNOUNCEMENT: RefCell<SharedString> = RefCell::default();
}

/// Has the screen reader read `text` out (through the main window's `Announcer`).
fn announce(text: &str) {
    ANNOUNCEMENT.with(|current| {
        let mut current = current.borrow_mut();
        // The announcement is the change of the node's name, so the same
        // message twice in a row needs a name that differs: an invisible
        // zero-width space is added or dropped.
        *current = if current.as_ref() == text {
            format!("{text}\u{200b}").into()
        } else {
            text.to_string().into()
        };
    });
}

/// A live region: an invisible node whose accessible name is the newest
/// notification, marked "polite" so a screen reader reads each change out
/// once it has finished speaking. GPUI Kit's own notifications are not
/// announced (longbridge/gpui-kit issue 3659: no live regions).
struct Announcer(SharedString);

impl IntoElement for Announcer {
    type Element = Hidden;

    fn into_element(self) -> Self::Element {
        Hidden {
            id: "announcer",
            text: self.0,
            live: true,
        }
    }
}

/// Text for screen readers only, as a label: for text the toolkit draws
/// itself without exposing it (the manual commands' code view).
struct Unseen(&'static str, SharedString);

impl IntoElement for Unseen {
    type Element = Hidden;

    fn into_element(self) -> Self::Element {
        Hidden {
            id: self.0,
            text: self.1,
            live: false,
        }
    }
}

/// An element that draws nothing and takes no room, and is in the
/// accessibility tree: a live region named `text`, or a label holding it.
struct Hidden {
    id: &'static str,
    text: SharedString,
    live: bool,
}

impl IntoElement for Hidden {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Hidden {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<Role> {
        Some(if self.live { Role::Status } else { Role::Label })
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        if !self.live {
            node.set_value(self.text.to_string());
            return;
        }
        node.set_live(accesskit::Live::Polite);
        if !self.text.is_empty() {
            node.set_label(self.text.to_string());
        }
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Out of the flow and with no size: it takes no room in the window.
        let style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        _: &mut Window,
        _: &mut App,
    ) {
    }
}

/// A question with Cancel and one action. `on_ok` runs at most once, after
/// the dialog is out of the way; cancelling (the button, Escape, the close
/// button) drops it.
struct Question {
    id: &'static str,
    title: &'static str,
    body: SharedString,
    ok_text: &'static str,
    default: DefaultButton,
}

fn show_confirm_dialog(question: Question, on_ok: Continuation, window: &mut Window, cx: &mut App) {
    let Question {
        id,
        title,
        body,
        ok_text,
        default,
    } = question;
    let slot = Rc::new(RefCell::new(Some(on_ok)));
    let action_focus = cx.focus_handle();
    let cancel_focus = cx.focus_handle();
    let first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            match default {
                DefaultButton::Action => action_focus.focus(window, cx),
                DefaultButton::Cancel => cancel_focus.focus(window, cx),
            }
        }
        let confirm = {
            let slot = slot.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some(callback) = slot.borrow_mut().take() {
                    window.defer(cx, move |window, cx| callback(window, cx));
                }
            }
        };
        let enter = confirm.clone();
        let enter_cancel = slot.clone();
        let cancel = slot.clone();
        let dismissed = slot.clone();
        let (on_action, on_cancel) = (action_focus.clone(), cancel_focus.clone());
        dialog
            .title(dialog_title(title))
            .overlay(false)
            .w(px(400.))
            // Enter: what the focused button says (see `EnterKey`).
            .on_ok(move |_, window, cx| {
                match enter_key(on_action.is_focused(window), on_cancel.is_focused(window)) {
                    EnterKey::Confirm => enter(window, cx),
                    EnterKey::Cancel => {
                        enter_cancel.borrow_mut().take();
                    }
                    EnterKey::Nothing => return false,
                }
                true
            })
            // Escape and the close button; after a confirm there is nothing left to drop.
            .on_close(move |_, _, _| {
                dismissed.borrow_mut().take();
            })
            .child(
                named_group(id, title)
                    .child(
                        div()
                            .id("body")
                            .max_h(dialog_room(window))
                            .overflow_y_scroll()
                            .child(label("text", body.clone())),
                    )
                    .child(
                        dialog_buttons()
                            .child(
                                Button::new("cancel")
                                    .label("Cancel")
                                    .track_focus(&cancel_focus)
                                    .on_click(move |_, window, cx| {
                                        cancel.borrow_mut().take();
                                        window.close_dialog(cx);
                                    }),
                            )
                            .child(
                                Button::new("ok")
                                    .primary()
                                    .label(ok_text)
                                    .track_focus(&action_focus)
                                    .on_click(move |_, window, cx| {
                                        window.close_dialog(cx);
                                        confirm(window, cx);
                                    }),
                            ),
                    ),
            )
    });
}

/// The window is drawn with the GPU, so with no usable Vulkan or OpenGL ES
/// driver (not even Mesa's software ones) there is nothing to draw an error
/// in. Says so through whatever the desktop offers, then exits: a process
/// left behind with no window would also keep later launches from starting.
fn no_window(error: &str) -> ! {
    let has_display = ["WAYLAND_DISPLAY", "DISPLAY"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    let message = no_window_message(error, has_display);
    eprintln!("steam-punk: {message}");
    applog::log(&format!("UI: {message}"));
    // With no display there is nowhere to show a dialog either.
    if has_display {
        for (program, args) in ERROR_DIALOGS {
            let status = std::process::Command::new(program)
                .args(*args)
                .arg(&message)
                .status();
            if status.is_ok_and(|status| helper_showed(program, status.code())) {
                break;
            }
        }
    }
    std::process::exit(1);
}

/// The programs tried, in order, to show an error when the app has no window.
const ERROR_DIALOGS: &[(&str, &[&str])] = &[
    ("kdialog", &["--title", "Steam Punk", "--error"]),
    (
        "zenity",
        &[
            "--error",
            "--no-markup",
            "--title=Steam Punk",
            "--width=480",
            "--text",
        ],
    ),
    (
        "notify-send",
        &["--urgency=critical", "--app-name=Steam Punk", "Steam Punk"],
    ),
    ("xmessage", &["-center"]),
];

/// Whether `program`, having ended with exit `code`, did show the message, so
/// the next one must not show it again. The dialogs end with 0 when the user
/// pressed their button and 1 when the user closed them another way; both
/// mean it was seen. Anything else (no display, a crash, a signal) does not.
/// notify-send shows nothing itself: only 0 says the desktop took the message.
fn helper_showed(program: &str, code: Option<i32>) -> bool {
    match program {
        "notify-send" => code == Some(0),
        _ => matches!(code, Some(0 | 1)),
    }
}

fn no_window_message(error: &str, has_display: bool) -> String {
    if has_display {
        format!(
            "Steam Punk could not open its window: no working graphics driver was found \
             (Vulkan or OpenGL ES). Install or repair your GPU driver, or install Mesa's \
             software renderer (mesa-vulkan-drivers on Debian, Ubuntu and Fedora; vulkan-swrast \
             on Arch).\n\nDetails: {error}"
        )
    } else {
        format!(
            "Steam Punk could not open its window: there is no display to open it on \
             (neither WAYLAND_DISPLAY nor DISPLAY is set). Start it from your desktop session.\
             \n\nDetails: {error}"
        )
    }
}

type RunningMap = Rc<RefCell<HashMap<PathBuf, u32>>>;

/// What the launch, stop and setup flows need from the window: the list to
/// refresh, and the tracked pgid per running trainer.
#[derive(Clone)]
struct Ui {
    view: WeakEntity<MainView>,
    running: RunningMap,
}

impl Ui {
    /// Re-reads the trainers dir and rebuilds the list. Never call this from
    /// inside an update of the main view itself (use `MainView::refresh`).
    fn refresh(&self, window: &mut Window, cx: &mut App) {
        let _ = self.view.update(cx, |view, cx| view.refresh(window, cx));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  build_ui
// ─────────────────────────────────────────────────────────────────────────────

/// The families tried for the UI font, in the order GPUI itself falls back to them.
const UI_FONTS: [&str; 9] = [
    "Ubuntu",
    "Adwaita Sans",
    "Cantarell",
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
    "Arial",
    "Helvetica",
    "FreeSans",
];

/// The families tried for fixed-width text (the manual setup commands), the
/// toolkit's own default first.
const MONO_FONTS: [&str; 10] = [
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "Ubuntu Mono",
    "Ubuntu Sans Mono",
    "Adwaita Mono",
    "Source Code Pro",
    "Hack",
    "FreeMono",
    "Nimbus Mono PS",
];

/// The first of `families` that is installed.
fn first_installed(families: &[&'static str], installed: &[String]) -> Option<&'static str> {
    families
        .iter()
        .copied()
        .find(|family| installed.iter().any(|name| name == family))
}

/// Starts the toolkit, with installed families as its fonts. Left alone, the
/// theme asks for GPUI's "system UI font", a family no Linux desktop ships,
/// and for a fixed-width family that many lack; GPUI and the toolkit then
/// log warnings and fall back to the first family they find. Naming those
/// up front gives the same text and no warnings.
pub fn init(cx: &mut App) {
    let installed = cx.text_system().all_font_names();
    let ui_font = first_installed(&UI_FONTS, &installed);
    if let Some(ui_font) = ui_font {
        // The toolkit keeps a theme that is already there, and families set
        // on it are used as they are.
        let mut theme = Theme::default();
        theme.font_family = ui_font.into();
        theme.mono_font_family = first_installed(&MONO_FONTS, &installed)
            .unwrap_or(ui_font)
            .into();
        cx.set_global(theme);
    }
    gpui_kit::init(cx);
    if ui_font.is_some() {
        // What the toolkit does for a theme it creates itself: its light and
        // dark colours, then the mode.
        let registry = ThemeRegistry::global(cx);
        let light = registry.default_light_theme().clone();
        let dark = registry.default_dark_theme().clone();
        let theme = Theme::global_mut(cx);
        theme.light_theme = light;
        theme.dark_theme = dark;
        Theme::change(ThemeMode::Light, None, cx);
    }
}

pub fn build_ui(cx: &mut App, mut activations: Activations) {
    // With no display GPUI would run on with no window and no word about it.
    if ["WAYLAND_DISPLAY", "DISPLAY"]
        .into_iter()
        .all(|name| std::env::var_os(name).is_none_or(|value| value.is_empty()))
    {
        no_window("no display server to connect to");
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(480.), px(560.)), cx)),
        window_min_size: Some(size(px(360.), px(280.))),
        app_id: Some(crate::APP_ID.to_string()),
        // The app draws its own title bar, so it looks and works the same on
        // every desktop, including compositors with no server-side decorations.
        window_decorations: Some(WindowDecorations::Client),
        ..TitleBar::window_options()
    };
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("Steam Punk");
        // Follow the desktop's light/dark preference, now and when it changes.
        Theme::sync_system_appearance(Some(window), cx);
        window
            .observe_window_appearance(|window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
            })
            .detach();
        cx.new(|cx| MainView::new(window, cx))
    });
    let (window, view) = match opened {
        Ok(opened) => opened,
        Err(e) => no_window(&format!("{e:#}")),
    };
    // The first read of the trainers, now that the window can show what is
    // wrong with them.
    let _ = window.update(cx, |_, window, cx| {
        view.update(cx, |view, cx| view.refresh(window, cx));
    });

    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();

    // A second launch asks this one to show its window, with the activation
    // token the desktop gave that launch when there is one: on Wayland only
    // that raises the window (see activation.rs). Without a token, or off
    // Wayland, GPUI's own request is what there is; a Wayland compositor then
    // marks the window as wanting attention instead of raising it.
    cx.spawn(async move |cx| {
        while let Some(token) = activations.recv().await {
            let _ = window.update(cx, |_, window, _| {
                let raised = token
                    .as_deref()
                    .is_some_and(|token| crate::activation::activate(window, token));
                if !raised {
                    window.activate_window();
                }
            });
        }
    })
    .detach();
}

// ─────────────────────────────────────────────────────────────────────────────
//  The main window: empty state <-> trainer list
// ─────────────────────────────────────────────────────────────────────────────

pub struct MainView {
    trainers: Vec<Trainer>,
    /// Tracked pgid per running trainer (keyed by trainer file path), for the
    /// lifetime of the app session — not just at launch time. See
    /// launcher::launch_trainer's doc comment for why the process group,
    /// not a single pid, is what's tracked.
    running: RunningMap,
    /// AppIDs whose cover-art fetch has already been retried this session —
    /// caps retries at one per AppID per run rather than re-attempting every
    /// refresh if it keeps failing (e.g. genuinely offline).
    cover_retry_attempted: HashSet<u32>,
    /// The last trainers.json problem shown, so a damaged file is reported
    /// once rather than on every refresh.
    meta_error_shown: Option<String>,
    /// Holds the keyboard focus until a control takes it, so Tab and the
    /// other keys work from the moment the window opens.
    focus_handle: FocusHandle,
    _poll: Task<()>,
}

impl MainView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Running-trainer poll — a non-blocking try_wait() on a single pid
        // isn't enough (see launcher::process_group_alive's doc comment), so
        // this periodically re-checks each tracked pgid instead. Runs for the
        // app's session lifetime; only touches the list when something
        // actually changed. The /proc scan runs on a blocking thread, never
        // the UI thread, and the loop waits for it, so scans never overlap.
        let poll = cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(1000))
                .await;
            let Ok(tracked) = this.update(cx, |this, _| this.tracked()) else {
                break;
            };
            if tracked.is_empty() {
                continue;
            }
            let scan = crate::runtime().spawn_blocking(move || {
                tracked
                    .into_iter()
                    .filter(|(_, pgid)| !launcher::process_group_alive(*pgid))
                    // A trainer gone right after launch gets a diagnosis
                    // (see launcher::finish_launch).
                    .map(|(path, pgid)| (path, pgid, launcher::finish_launch(pgid)))
                    .collect::<Vec<_>>()
            });
            let Ok(exited) = scan.await else { continue };
            if this
                .update_in(cx, |this, window, cx| this.reap(exited, window, cx))
                .is_err()
            {
                break;
            }
        });

        let view = Self {
            trainers: Vec::new(),
            running: Rc::new(RefCell::new(HashMap::new())),
            cover_retry_attempted: HashSet::new(),
            meta_error_shown: None,
            focus_handle: cx.focus_handle(),
            _poll: poll,
        };
        // The list is filled by `build_ui` once the window is up: reading it
        // can have something to tell the user (a damaged trainers.json), and
        // there is nowhere to show that from here.
        view.focus_handle.focus(window, cx);
        view
    }

    fn ui(&self, cx: &Context<Self>) -> Ui {
        Ui {
            view: cx.weak_entity(),
            running: self.running.clone(),
        }
    }

    /// The launches with a real process (pgid 0 = launch pending).
    fn tracked(&self) -> Vec<(PathBuf, u32)> {
        self.running
            .borrow()
            .iter()
            .filter(|(_, &pgid)| pgid != 0)
            .map(|(path, &pgid)| (path.clone(), pgid))
            .collect()
    }

    /// Drops the trainers the poll found gone, with the likely cause for one
    /// that died right after launch.
    fn reap(
        &mut self,
        exited: Vec<(PathBuf, u32, Option<String>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for (path, pgid, diagnosis) in &exited {
            // Only if it's still the same launch (not stopped and relaunched
            // while the scan ran).
            if untrack(&mut self.running.borrow_mut(), path, *pgid) {
                applog::log(&format!(
                    "UI: trainer at {} exited on its own",
                    path.display()
                ));
                changed = true;
                if let Some(cause) = diagnosis.as_ref().and_then(|d| d.lines().next()) {
                    toast(
                        window,
                        cx,
                        &format!("Trainer exited right after launch. {cause}"),
                    );
                }
            }
        }
        if changed {
            self.refresh(window, cx);
        }
    }

    /// Re-reads the trainers dir and rebuilds the list.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let listing = library::list_trainers().unwrap_or_else(|e| library::Listing {
            trainers: Vec::new(),
            meta_error: Some(format!("{e:#}")),
        });
        if let Some(e) = unreported(&listing.meta_error, &self.meta_error_shown) {
            // Only what the user has really been shown counts as reported.
            if show_toast(window, cx, e) {
                self.meta_error_shown = listing.meta_error.clone();
            }
        } else {
            self.meta_error_shown = listing.meta_error.clone();
        }
        self.trainers = listing.trainers;

        // A name or cover that's still missing — the initial fetch failed
        // (e.g. imported offline), or an AppID whose guessed CDN paths both
        // 404 (confirmed live for some current-gen games) — would otherwise
        // stay missing forever, since fetch_and_cache only runs at
        // association time. Retry once per AppID per session whenever either
        // is absent.
        for trainer in &self.trainers {
            let Some(appid) = trainer.appid else { continue };
            if (trainer.game_name.is_none()
                || trainer.cover_path.is_none()
                || gamedata::cached_online(appid).is_none())
                && self.cover_retry_attempted.insert(appid)
            {
                let ui = self.ui(cx);
                spawn_async(
                    window,
                    cx,
                    gamedata::fetch_and_cache(appid),
                    move |result, window, cx| {
                        if let Err(e) = result {
                            applog::log(&format!(
                                "gamedata: name/cover retry failed for AppID {appid}: {e}"
                            ));
                        }
                        ui.refresh(window, cx);
                    },
                );
            }
        }
        cx.notify();
    }

    fn render_row(&self, index: usize, trainer: &Trainer, ui: &Ui, cx: &App) -> impl IntoElement {
        // Rows are keyed by the trainer's file, not its place in the list.
        let key = trainer.path.to_string_lossy().into_owned();
        let id = |part: &str| ElementId::Name(format!("{part}:{key}").into());

        // pgid 0 = launch pending (no process yet): show Launch, whose click
        // is a no-op until the launch resolves.
        let running_pgid = self
            .running
            .borrow()
            .get(&trainer.path)
            .copied()
            .filter(|&p| p != 0);

        // Screen readers get the trainer's name on the row and on its
        // buttons: an icon alone, or three identical "Launch"es, say nothing.
        let name = trainer.display_name().to_string();
        h_flex()
            .id(id("row"))
            .role(Role::ListItem)
            .aria_label(name.clone())
            .w_full()
            .gap_3()
            .px_3()
            .py_2()
            .when(index > 0, |row| {
                row.border_t_1().border_color(cx.theme().border)
            })
            .child(
                // The cover and the name; the AppID tooltip is theirs alone, so
                // it never comes up over a dialog a button of the row opened.
                h_flex()
                    .id(id("game"))
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .when_some(trainer.appid, |game, appid| {
                        game.tooltip(move |window, cx| {
                            Tooltip::new(format!("Steam AppID {appid}")).build(window, cx)
                        })
                    })
                    // Every cover is Steam's landscape header art (460x215) in
                    // one fixed slot.
                    .when_some(trainer.cover_path.clone(), |game, cover| {
                        game.child(
                            img(cover)
                                .w(px(COVER_WIDTH))
                                .h(px(COVER_HEIGHT))
                                .flex_none()
                                .rounded(cx.theme().radius)
                                .object_fit(ObjectFit::Cover),
                        )
                    })
                    .child(
                        // Long names wrap onto more lines rather than being cut off.
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(trainer.display_name().to_string()),
                    ),
            )
            .map(|row| match running_pgid {
                Some(pgid) => row
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().success)
                            .child(label(id("running"), "Running")),
                    )
                    .child(
                        Button::new(id("stop"))
                            .danger()
                            .label("Stop")
                            .accessibility_label(format!("Stop {name}"))
                            .on_click({
                                let ui = ui.clone();
                                let path = trainer.path.clone();
                                move |_, window, cx| {
                                    stop_clicked(&ui, path.clone(), pgid, window, cx)
                                }
                            }),
                    ),
                None => row.child(
                    Button::new(id("launch"))
                        .primary()
                        .label("Launch")
                        .accessibility_label(format!("Launch {name}"))
                        .on_click({
                            let ui = ui.clone();
                            let trainer = trainer.clone();
                            move |_, window, cx| launch_clicked(&ui, &trainer, window, cx)
                        }),
                ),
            })
            .child(
                Button::new(id("remove"))
                    .ghost()
                    .icon(Icon::new(gpui_kit::assets::IconName::Trash))
                    .tooltip("Remove")
                    .accessibility_label(format!("Remove {name}"))
                    .on_click({
                        let ui = ui.clone();
                        let path = trainer.path.clone();
                        move |_, window, cx| {
                            // A running (or launching) trainer keeps its row: with
                            // it gone there would be no Stop for the process, and a
                            // pending launch would run a deleted file.
                            if ui.running.borrow().contains_key(&path) {
                                toast(window, cx, "Stop the trainer before removing it");
                                return;
                            }
                            if let Err(e) = library::remove_trainer(&path) {
                                toast(window, cx, &format!("Remove failed: {e}"));
                                return;
                            }
                            ui.refresh(window, cx);
                        }
                    }),
            )
    }
}

impl Render for MainView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = self.ui(cx);

        let title_bar = TitleBar::new()
            .child(
                h_flex()
                    .gap_2()
                    // Clicks on the buttons must not start a window drag.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new("import")
                            .ghost()
                            .small()
                            .icon(Icon::new(gpui_kit::component::IconName::Plus))
                            .tooltip("Import trainer .exe")
                            .accessibility_label("Import trainer .exe")
                            .on_click({
                                let ui = ui.clone();
                                move |_, window, cx| import_clicked(&ui, window, cx)
                            }),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Steam Punk"),
            )
            .child(
                h_flex()
                    .px_2()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new("about")
                            .ghost()
                            .small()
                            .icon(Icon::new(gpui_kit::component::IconName::Info))
                            .tooltip("About")
                            .accessibility_label("About")
                            .on_click(|_, window, cx| show_about_dialog(window, cx)),
                    ),
            );

        let content = if self.trainers.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Empty::new().header(
                        EmptyHeader::new()
                            .media(
                                EmptyMedia::new()
                                    .with_variant(EmptyMediaVariant::Icon)
                                    .child(Icon::new(gpui_kit::assets::IconName::Gamepad2)),
                            )
                            .title(EmptyTitle::new().child(label("empty-title", "No Trainers Yet")))
                            .description(EmptyDescription::new().child(label(
                                "empty-hint",
                                "Drop a trainer .exe here, or use the + button",
                            ))),
                    ),
                )
                .into_any_element()
        } else {
            div()
                .id("trainer-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p_3()
                .child(
                    v_flex()
                        .id("trainers")
                        .role(Role::List)
                        .aria_label("Trainers")
                        .w_full()
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().secondary)
                        .children(
                            self.trainers
                                .iter()
                                .enumerate()
                                .map(|(index, trainer)| self.render_row(index, trainer, &ui, cx)),
                        ),
                )
                .into_any_element()
        };

        // Accept dropped .exe files anywhere in the window.
        v_flex()
            .id("main")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(title_bar)
            .child(content)
            .child(Announcer(ANNOUNCEMENT.with(|text| text.borrow().clone())))
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(move |dropped: &ExternalPaths, window, cx| {
                let (exes, others): (Vec<PathBuf>, Vec<PathBuf>) = dropped
                    .paths()
                    .iter()
                    .cloned()
                    .partition(|path| library::is_exe(path));
                if !others.is_empty() {
                    toast(window, cx, NOT_AN_EXE);
                }
                if !exes.is_empty() {
                    import_off_main_thread(exes, ui.clone(), window, cx);
                }
            })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Import — file picker (the desktop's own, through the portal)
// ─────────────────────────────────────────────────────────────────────────────

/// Neither the desktop's file dialog (GPUI can't give it a file-type filter)
/// nor a drop keeps other files out, so they are turned away here.
const NOT_AN_EXE: &str = "Only trainer .exe files can be imported";

fn import_clicked(ui: &Ui, window: &mut Window, cx: &mut App) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Import".into()),
    });
    let ui = ui.clone();
    window
        .spawn(cx, async move |cx| {
            let result = picked.await;
            let _ = cx.update(|window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    let (exes, others): (Vec<PathBuf>, Vec<PathBuf>) =
                        paths.into_iter().partition(|path| library::is_exe(path));
                    if !others.is_empty() {
                        toast(window, cx, NOT_AN_EXE);
                    }
                    if !exes.is_empty() {
                        import_off_main_thread(exes, ui, window, cx);
                    }
                }
                Ok(Ok(None)) | Err(_) => {}
                Ok(Err(e)) => toast(
                    window,
                    cx,
                    &format!(
                        "Could not open the file dialog: {e:#}. Drop the trainer .exe on the \
                         window instead."
                    ),
                ),
            });
        })
        .detach();
}

/// Imports trainers off the UI thread (a copy of a large file, or from a slow mount, would
/// freeze the window), then asks for the AppIDs of those that came in.
fn import_off_main_thread(paths: Vec<PathBuf>, ui: Ui, window: &mut Window, cx: &mut App) {
    spawn_async(
        window,
        cx,
        async move {
            tokio::task::spawn_blocking(move || {
                paths
                    .into_iter()
                    .map(|path| {
                        let result = library::import_trainer(&path);
                        (path, result)
                    })
                    .collect::<Vec<_>>()
            })
            .await
        },
        move |results, window, cx| {
            let Ok(results) = results else {
                toast(
                    window,
                    cx,
                    "Import failed: the import task ended unexpectedly",
                );
                return;
            };
            let mut imported = Vec::new();
            for (path, result) in results {
                applog::log(&format!(
                    "UI: import_trainer({}) -> {result:?}",
                    path.display()
                ));
                match result {
                    Ok(dest) => imported.push(dest),
                    Err(e) => toast(window, cx, &format!("Import failed: {e}")),
                }
            }
            if !imported.is_empty() {
                prompt_appid_for_queue(ui, Rc::new(RefCell::new(imported)), window, cx);
            }
        },
    );
}

// ─────────────────────────────────────────────────────────────────────────────
//  About
// ─────────────────────────────────────────────────────────────────────────────

const ABOUT_TITLE: &str = "About Steam Punk";

fn show_about_dialog(window: &mut Window, cx: &mut App) {
    let icon = Arc::new(Image::from_bytes(
        ImageFormat::Png,
        include_bytes!("../data/steam-punk-128.png").to_vec(),
    ));
    let focus = InitialFocus::new(cx);
    window.open_dialog(cx, move |dialog, window, cx| {
        focus.apply(window, cx);
        let muted = cx.theme().muted_foreground;
        dialog
            .title(dialog_title(ABOUT_TITLE))
            .overlay(false)
            .w(px(380.))
            .child(
                named_group("about", ABOUT_TITLE)
                    .child(
                        v_flex()
                            .id("body")
                            .max_h(dialog_room(window))
                            .overflow_y_scroll()
                            .gap_1()
                            .items_center()
                            .child(img(icon.clone()).w(px(64.)).h(px(64.)))
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .child(label("name", "Steam Punk")),
                            )
                            .child(
                                div()
                                    .text_color(muted)
                                    .child(label("version", env!("CARGO_PKG_VERSION"))),
                            )
                            .child(div().text_center().child(label(
                                "summary",
                                "Launches Windows game trainers through Proton against a running \
                             Steam game's wine session.",
                            )))
                            .child(
                                h_flex()
                                    .gap_4()
                                    .child(
                                        Link::new("about-website").href(WEBSITE).child("Website"),
                                    )
                                    .child(
                                        Link::new("about-issues")
                                            .href(ISSUES)
                                            .child("Report an Issue"),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_1()
                                    .items_center()
                                    .text_sm()
                                    .text_color(muted)
                                    .text_center()
                                    .child(label("developer", "Developer: Linnard Alex Brown Jr."))
                                    .child(label(
                                        "built-with",
                                        "Built with: Claude Code (Anthropic), Codex (OpenAI)",
                                    ))
                                    .child(label("license", "License: AGPL-3.0-or-later")),
                            ),
                    )
                    .child(
                        dialog_buttons().child(
                            Button::new("about-close")
                                .track_focus(&focus.handle)
                                .label("Close")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                    ),
            )
    });
}

// ─────────────────────────────────────────────────────────────────────────────
//  Optional AppID prompt — shown once per just-imported trainer, right
//  after import. Purely additive: skipping (or entering nothing) leaves the
//  trainer exactly as it was before this feature existed — filename title,
//  no art. Drives itself through `queue` one file at a time so a
//  multi-file drop prompts for each in turn instead of only the first.
//
//  The dialog has several independent ways to finish (a search-result click,
//  the "Add" button, "Skip", Enter in the search field, or closing it with
//  Escape or its close button). Every one of them ends in `advance`, guarded
//  to run exactly once — so closing it any other way counts as Skip, and no
//  path can advance twice.
// ─────────────────────────────────────────────────────────────────────────────

type Finish = Rc<dyn Fn(&mut Window, &mut App)>;

fn prompt_appid_for_queue(
    ui: Ui,
    queue: Rc<RefCell<Vec<PathBuf>>>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(trainer_path) = queue.borrow_mut().pop() else {
        ui.refresh(window, cx);
        toast(window, cx, "Trainer imported");
        return;
    };
    let Some(filename) = trainer_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    else {
        // No filename to key metadata by — skip straight to the next.
        prompt_appid_for_queue(ui, queue, window, cx);
        return;
    };
    let stem = trainer_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| filename.clone());

    // Moves on to the next queued trainer (or finishes the queue), at most
    // once, after the dialog is out of the way.
    let advance: Finish = {
        let ui = ui.clone();
        let advanced = Cell::new(false);
        Rc::new(move |window, cx| {
            if advanced.replace(true) {
                return;
            }
            let ui = ui.clone();
            let queue = queue.clone();
            window.defer(cx, move |window, cx| {
                prompt_appid_for_queue(ui, queue, window, cx)
            });
        })
    };
    // What the dialog's own buttons and picks call: close it, then advance.
    let finish: Finish = {
        let advance = advance.clone();
        Rc::new(move |window, cx| {
            window.close_dialog(cx);
            advance(window, cx);
        })
    };

    let title: SharedString = format!("Add Steam AppID for \u{201c}{filename}\u{201d}?").into();
    let prompt = cx.new(|cx| {
        let mut prompt = AppIdPrompt::new(ui, filename, &stem, finish, window, cx);
        prompt.title = title.clone();
        prompt
    });
    let initial_focus = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if initial_focus.replace(false) {
            prompt.read(cx).input.focus_handle(cx).focus(window, cx);
        }
        let advance = advance.clone();
        dialog
            .title(dialog_title(title.clone()))
            .overlay(false)
            .w(px(420.))
            .child(prompt.clone())
            // Enter: in the search field it is the field's (it picks a game
            // there, and the dialog's own "confirm" would close it as a
            // skip); on Skip, Add or a Use button it presses that button.
            .on_ok({
                let prompt = prompt.clone();
                move |_, window, cx| {
                    prompt.update(cx, |prompt, cx| prompt.enter_pressed(window, cx))
                }
            })
            // Escape and the close button land here.
            .on_close(move |_, window, cx| advance(window, cx))
    });
}

struct AppIdPrompt {
    ui: Ui,
    /// The dialog's title, which names its content for screen readers.
    title: SharedString,
    filename: String,
    input: Entity<InputState>,
    /// The installed game matching the trainer's name, if any; the user still
    /// has to pick it.
    suggestion: Option<(u32, String)>,
    results: Vec<gamedata::SearchResult>,
    /// Thumbnails by URL; each loads async and drops in once it arrives (or
    /// never, if the fetch fails — the row is still usable without it).
    thumbnails: HashMap<String, Arc<Image>>,
    /// Only the first pick counts.
    picked: bool,
    finish: Finish,
    /// Debounce: only search once typing has paused for 250ms, so a burst of
    /// keystrokes produces one request instead of one per key. Replacing the
    /// task cancels the one before.
    pending_search: Option<Task<()>>,
    /// The focus of Skip, Add and each game's Use button, so Enter can act
    /// as the focused one (see `EnterKey`).
    skip_focus: FocusHandle,
    add_focus: FocusHandle,
    use_focus: RefCell<HashMap<(&'static str, u32), FocusHandle>>,
    _subscriptions: Vec<Subscription>,
}

/// What Enter does in the AppID dialog, by where the keyboard focus is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum AppIdEnter {
    /// In the search field: the field handles it; the dialog stays.
    Field,
    Skip,
    Add,
    Use(u32),
    /// Anywhere else (the close button): the dialog closes, which skips.
    Close,
}

fn appid_enter(field: bool, skip: bool, add: bool, used: Option<u32>) -> AppIdEnter {
    match (field, skip, add, used) {
        (true, ..) => AppIdEnter::Field,
        (_, true, ..) => AppIdEnter::Skip,
        (_, _, true, _) => AppIdEnter::Add,
        (_, _, _, Some(appid)) => AppIdEnter::Use(appid),
        _ => AppIdEnter::Close,
    }
}

impl AppIdPrompt {
    fn new(
        ui: Ui,
        filename: String,
        stem: &str,
        finish: Finish,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let guessed_term = gamedata::guess_search_term(stem);
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx)
                .placeholder("Search for a game, or enter a numeric AppID")
                .default_value(guessed_term.clone());
            // The guess comes selected, so typing replaces it and End or a
            // click keeps it.
            input.select_all(window, cx);
            input
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let term = input.read(cx).value().to_string();
                    this.pending_search = Some(cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(250))
                            .await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.run_search(term, window, cx);
                        });
                    }));
                }
                InputEvent::PressEnter { .. } => {
                    // A raw numeric AppID takes priority (matches the Add
                    // button's behavior); otherwise Enter picks whatever's
                    // currently the top search result, if any.
                    let text = input.read(cx).value();
                    if let Ok(appid) = text.trim().parse::<u32>() {
                        this.confirm(appid, window, cx);
                    } else if let Some(appid) = this.results.first().map(|r| r.appid) {
                        this.confirm(appid, window, cx);
                    }
                }
                _ => {}
            },
        );

        let mut prompt = Self {
            ui,
            title: SharedString::default(),
            filename,
            input,
            suggestion: None,
            results: Vec::new(),
            thumbnails: HashMap::new(),
            picked: false,
            finish,
            pending_search: None,
            skip_focus: cx.focus_handle(),
            add_focus: cx.focus_handle(),
            use_focus: RefCell::default(),
            _subscriptions: vec![subscription],
        };

        // Fire the initial search immediately with the guessed term, so
        // results are already present when the dialog opens.
        prompt.run_search(guessed_term, window, cx);

        // Match the trainer's name against the games installed locally, off
        // the UI thread (it reads every appmanifest).
        let this = cx.weak_entity();
        let stem = stem.to_string();
        spawn_async(
            window,
            cx,
            async move {
                tokio::task::spawn_blocking(move || {
                    gamedata::suggest_installed(&stem, &steam::all_installed_games())
                })
                .await
            },
            move |result, _, cx| {
                let Ok(Some(suggestion)) = result else { return };
                let _ = this.update(cx, |this, cx| {
                    this.suggestion = Some(suggestion);
                    cx.notify();
                });
            },
        );
        prompt
    }

    /// Fires a search, guarded against out-of-order replies to fast typing:
    /// a result set is only applied if the field's text still matches the
    /// term that produced it by the time the network call returns.
    fn run_search(&mut self, term: String, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let term_for_check = term.clone();
        spawn_async(
            window,
            cx,
            async move { gamedata::search(&term).await },
            move |results, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    if this.input.read(cx).value().as_ref() != term_for_check {
                        return;
                    }
                    match results {
                        Ok(results) => {
                            this.results = results;
                            this.load_thumbnails(window, cx);
                        }
                        Err(e) => {
                            this.results.clear();
                            applog::log(&format!(
                                "gamedata: search failed for {term_for_check:?}: {e}"
                            ));
                        }
                    }
                    cx.notify();
                });
            },
        );
    }

    fn load_thumbnails(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for url in self.results.iter().filter_map(|r| r.thumbnail_url.clone()) {
            if self.thumbnails.contains_key(&url) {
                continue;
            }
            let this = cx.weak_entity();
            let key = url.clone();
            spawn_async(
                window,
                cx,
                async move { gamedata::fetch_thumbnail(&url).await },
                move |bytes, _, cx| {
                    let Some(image) = bytes.ok().and_then(image_from_bytes) else {
                        return;
                    };
                    let _ = this.update(cx, |this, cx| {
                        this.thumbnails.insert(key, image);
                        cx.notify();
                    });
                },
            );
        }
    }

    /// Every way of picking an AppID — a search-result row, a suggestion or
    /// the Add button — funnels through here: saves the association, kicks
    /// off the (fire-and-forget) name/art fetch, and closes the dialog, which
    /// advances the queue. Fetch failure only logs and still advances —
    /// matches the existing "don't block adding" behavior.
    fn confirm(&mut self, appid: u32, window: &mut Window, cx: &mut Context<Self>) {
        if self.picked {
            return;
        }
        self.picked = true;
        match library::set_trainer_appid(&self.filename, appid) {
            Ok(()) => {
                let ui = self.ui.clone();
                spawn_async(
                    window,
                    cx,
                    gamedata::fetch_and_cache(appid),
                    move |result, window, cx| {
                        if let Err(e) = result {
                            applog::log(&format!("gamedata: fetch failed for AppID {appid}: {e}"));
                        }
                        ui.refresh(window, cx);
                    },
                );
            }
            Err(e) => toast(window, cx, &format!("Could not save AppID: {e:#}")),
        }
        (self.finish)(window, cx);
    }

    /// Enter, handed over by the dialog. True lets the dialog close itself.
    fn enter_pressed(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let used = self
            .use_focus
            .borrow()
            .iter()
            .find(|(_, focus)| focus.is_focused(window))
            .map(|((_, appid), _)| *appid);
        let action = appid_enter(
            self.input.focus_handle(cx).contains_focused(window, cx),
            self.skip_focus.is_focused(window),
            self.add_focus.is_focused(window),
            used,
        );
        // The buttons' own paths close the dialog, as a click on them does.
        match action {
            AppIdEnter::Field => {}
            AppIdEnter::Skip => (self.finish)(window, cx),
            AppIdEnter::Add => self.add_clicked(window, cx),
            AppIdEnter::Use(appid) => self.confirm(appid, window, cx),
            AppIdEnter::Close => return true,
        }
        false
    }

    fn add_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            (self.finish)(window, cx);
            return;
        }
        match trimmed.parse::<u32>() {
            Ok(appid) => self.confirm(appid, window, cx),
            Err(_) => {
                toast(
                    window,
                    cx,
                    "That doesn't look like a numeric AppID — skipped",
                );
                (self.finish)(window, cx);
            }
        }
    }

    /// One game the user can pick: a search result or the installed-game
    /// suggestion. The whole row is clickable, and has a Use button.
    #[allow(clippy::too_many_arguments)]
    fn render_choice(
        &self,
        key: &'static str,
        appid: u32,
        name: &str,
        subtitle: String,
        thumbnail: Option<Arc<Image>>,
        reserve_thumbnail: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id((key, appid as usize))
            .w_full()
            .gap_3()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .hover(|style| style.bg(cx.theme().accent))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.confirm(appid, window, cx)))
            .when(reserve_thumbnail, |row| {
                row.child(div().w(px(60.)).h(px(34.)).flex_none().when_some(
                    thumbnail,
                    |slot, image| {
                        slot.child(
                            img(image)
                                .size_full()
                                .rounded(cx.theme().radius)
                                .object_fit(ObjectFit::Cover),
                        )
                    },
                ))
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().child(label("name", name.to_string())))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(label("subtitle", subtitle)),
                    ),
            )
            .child(
                Button::new((key, appid as usize + (1 << 32)))
                    .track_focus(
                        self.use_focus
                            .borrow_mut()
                            .entry((key, appid))
                            .or_insert_with(|| cx.focus_handle()),
                    )
                    .small()
                    .label("Use")
                    .accessibility_label(format!("Use {name}"))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.confirm(appid, window, cx)),
                    ),
            )
    }
}

impl Render for AppIdPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The results take what the window leaves, so Skip and Add stay in reach.
        let results_height = (window.viewport_size().height - px(410.)).clamp(px(48.), px(170.));
        let boxed = |cx: &Context<Self>| {
            v_flex()
                .w_full()
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius)
        };
        let suggestion = self.suggestion.clone().map(|(appid, name)| {
            boxed(cx).child(self.render_choice(
                "suggestion",
                appid,
                &name,
                format!("Suggested from your installed games, AppID {appid}"),
                None,
                false,
                cx,
            ))
        });
        let results: Vec<_> = self
            .results
            .iter()
            .map(|result| {
                let thumbnail = result
                    .thumbnail_url
                    .as_ref()
                    .and_then(|url| self.thumbnails.get(url).cloned());
                self.render_choice(
                    "result",
                    result.appid,
                    &result.name,
                    format!("AppID {}", result.appid),
                    thumbnail,
                    result.thumbnail_url.is_some(),
                    cx,
                )
                .into_any_element()
            })
            .collect();

        named_group("appid", self.title.clone())
            .gap_2()
            .child(label(
                "hint",
                "Optionally search for this game to show its name and cover art in \
                 the list instead of the raw trainer filename. Leave blank to skip \
                 — the trainer still works either way.",
            ))
            .child(
                Input::new(&self.input)
                    .cleanable(true)
                    .prefix(Icon::new(gpui_kit::component::IconName::Search).small()),
            )
            .children(suggestion)
            .child(
                div()
                    .id("appid-results")
                    .h(results_height)
                    .overflow_y_scroll()
                    .when(!results.is_empty(), |list| {
                        list.child(boxed(cx).children(results))
                    }),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("appid-skip")
                            .track_focus(&self.skip_focus)
                            .label("Skip")
                            .on_click(cx.listener(|this, _, window, cx| {
                                (this.finish)(window, cx);
                            })),
                    )
                    .child(
                        Button::new("appid-add")
                            .track_focus(&self.add_focus)
                            .primary()
                            .label("Add")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_clicked(window, cx);
                            })),
                    ),
            )
    }
}

fn image_from_bytes(bytes: Vec<u8>) -> Option<Arc<Image>> {
    let format = if bytes.starts_with(&[0xFF, 0xD8]) {
        ImageFormat::Jpeg
    } else if bytes.starts_with(b"\x89PNG") {
        ImageFormat::Png
    } else if bytes.starts_with(b"GIF8") {
        ImageFormat::Gif
    } else if bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        ImageFormat::Webp
    } else {
        return None;
    };
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

// ─────────────────────────────────────────────────────────────────────────────
//  Launch button wiring
// ─────────────────────────────────────────────────────────────────────────────

type Proceed = Rc<dyn Fn(LaunchTarget, &mut Window, &mut App)>;

/// Work out which running game to act on and hand the resolved target to
/// `proceed`. When exactly one game is running it's used directly; when
/// several are, the user picks. Shared by the launch and troubleshoot paths so
/// both disambiguate the same way instead of silently taking whichever game
/// /proc happened to list first.
fn with_launch_target(ui: &Ui, proceed: Proceed, window: &mut Window, cx: &mut App) {
    let ui = ui.clone();
    spawn_async(
        window,
        cx,
        async { tokio::task::spawn_blocking(launcher::running_games).await },
        move |result, window, cx| {
            let games = match result {
                Ok(games) => games,
                Err(e) => {
                    toast(window, cx, &format!("Task error: {e}"));
                    return;
                }
            };

            match games.len() {
                0 => toast(
                    window,
                    cx,
                    "Start the game first, load past the menus, then launch the trainer.",
                ),
                1 => resolve_target_then(games[0].appid, proceed, window, cx),
                _ => show_game_picker(&ui, games, proceed, window, cx),
            }
        },
    );
}

fn resolve_target_then(appid: u32, proceed: Proceed, window: &mut Window, cx: &mut App) {
    spawn_async(
        window,
        cx,
        async move { tokio::task::spawn_blocking(move || launcher::resolve_launch_target(appid)).await },
        move |result, window, cx| match result {
            Ok(Ok(target)) => proceed(target, window, cx),
            Ok(Err(e)) => toast(window, cx, &e.to_string()),
            Err(e) => toast(window, cx, &format!("Task error: {e}")),
        },
    );
}

/// One button per running game. Trainers are game-specific, so guessing here
/// would attach to the wrong game's prefix rather than fail visibly — worth
/// an extra click in the rare case two games are open.
const PICKER_TITLE: &str = "Which Game?";

fn show_game_picker(
    _ui: &Ui,
    games: Vec<launcher::RunningGame>,
    proceed: Proceed,
    window: &mut Window,
    cx: &mut App,
) {
    // Each game's button with its own focus, so Enter can pick the one the
    // focus is on (see `EnterKey`).
    let games: Rc<[(u32, SharedString, FocusHandle)]> = games
        .into_iter()
        .map(|game| (game.appid, SharedString::from(game.name), cx.focus_handle()))
        .collect();
    let cancel_focus = cx.focus_handle();
    let first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            if let Some((_, _, focus)) = games.first() {
                focus.focus(window, cx);
            }
        }
        let buttons = games.iter().map(|(appid, name, focus)| {
            let appid = *appid;
            let proceed = proceed.clone();
            Button::new(("game", appid as usize))
                .track_focus(focus)
                .label(name.clone())
                .w_full()
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    resolve_target_then(appid, proceed.clone(), window, cx);
                })
        });
        let (enter_games, enter_proceed) = (games.clone(), proceed.clone());
        let on_cancel = cancel_focus.clone();
        dialog
            .title(dialog_title(PICKER_TITLE))
            .overlay(false)
            .w(px(380.))
            .on_ok(move |_, window, cx| {
                let focused = enter_games
                    .iter()
                    .find(|(_, _, focus)| focus.is_focused(window))
                    .map(|(appid, _, _)| *appid);
                match focused {
                    Some(appid) => {
                        let proceed = enter_proceed.clone();
                        window.defer(cx, move |window, cx| {
                            resolve_target_then(appid, proceed, window, cx)
                        });
                        true
                    }
                    // Enter on Cancel cancels; anywhere else it picks nothing.
                    None => on_cancel.is_focused(window),
                }
            })
            .child(
                named_group("game-picker", PICKER_TITLE)
                    .gap_2()
                    .child(label(
                        "hint",
                        "More than one game is running. Pick the one this trainer is for.",
                    ))
                    .children(buttons)
                    .child(
                        dialog_buttons().child(
                            Button::new("game-cancel")
                                .label("Cancel")
                                .track_focus(&cancel_focus)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                    ),
            )
    });
}

/// Stops tracking `path`'s launch `pgid`, but only if that is still the
/// launch tracked for it: a trainer stopped (or gone) and launched again
/// meanwhile keeps its new launch. True when it was removed.
fn untrack(running: &mut HashMap<PathBuf, u32>, path: &std::path::Path, pgid: u32) -> bool {
    if running.get(path) == Some(&pgid) {
        running.remove(path);
        true
    } else {
        false
    }
}

/// Marks a trainer as "launch pending" (pgid 0 in `running`) from the moment
/// Launch is clicked, so a second click before the launch resolves is a no-op
/// instead of starting a second, untracked process. The marker is removed when
/// the last clone of the guard drops — whichever way the flow ends (toast,
/// cancelled dialog, failure). A real pgid recorded meanwhile is left alone.
struct PendingLaunch {
    running: RunningMap,
    path: PathBuf,
}

impl Drop for PendingLaunch {
    fn drop(&mut self) {
        let mut r = self.running.borrow_mut();
        if r.get(&self.path) == Some(&0) {
            r.remove(&self.path);
        }
    }
}

thread_local! {
    /// The prefixes a .NET setup (clone or installer) is working on.
    static REPAIRING: RefCell<HashSet<PathBuf>> = RefCell::default();
}

/// One .NET setup at a time per prefix: two trainers of one game share it, and two setups
/// copying into it (or the installer over a clone) would leave it half done.
struct PrefixRepair(PathBuf);

impl PrefixRepair {
    fn begin(prefix: PathBuf) -> Option<Self> {
        REPAIRING
            .with(|r| r.borrow_mut().insert(prefix.clone()))
            .then_some(Self(prefix))
    }
}

impl Drop for PrefixRepair {
    fn drop(&mut self) {
        REPAIRING.with(|r| r.borrow_mut().remove(&self.0));
    }
}

/// A step that runs at most once, later, on the UI thread.
type Continuation = Box<dyn FnOnce(&mut Window, &mut App)>;

fn launch_clicked(ui: &Ui, trainer: &Trainer, window: &mut Window, cx: &mut App) {
    if ui.running.borrow().contains_key(&trainer.path) {
        return;
    }
    ui.running.borrow_mut().insert(trainer.path.clone(), 0);
    let guard = Rc::new(PendingLaunch {
        running: ui.running.clone(),
        path: trainer.path.clone(),
    });
    let trainer = trainer.clone();
    let flow_ui = ui.clone();

    with_launch_target(
        ui,
        Rc::new(move |target, window, cx| {
            let trainer = trainer.clone();
            let ui = flow_ui.clone();
            let guard = guard.clone();
            // The prefix health check parses the prefix's whole system.reg
            // (several MB) — keep it off the UI thread. The anti-cheat lookup
            // reads the game's install folder.
            spawn_async(
                window,
                cx,
                async move {
                    tokio::task::spawn_blocking(move || {
                        let (usable, health) = launcher::prefix_health(&target);
                        let notice = launcher::anticheat_notice(&target);
                        (target, usable, health, notice)
                    })
                    .await
                },
                move |result, window, cx| match result {
                    Ok((target, usable, health, notice)) => {
                        let proceed: Continuation = Box::new(move |window, cx| {
                            if usable {
                                launch_trainer_now(target, trainer, ui, guard, window, cx)
                            } else {
                                show_dotnet_dialog(target, trainer, ui, guard, &health, window, cx)
                            }
                        });
                        match notice {
                            Some(game) => show_anticheat_dialog(&game, proceed, window, cx),
                            None => proceed(window, cx),
                        }
                    }
                    Err(e) => toast(window, cx, &format!("Task error: {e}")),
                },
            );
        }),
        window,
        cx,
    );
}

/// One-time-per-session warning for games with an online anti-cheat. Runs
/// `on_continue` only if the user chooses to launch anyway; cancelling drops
/// it, which also releases the pending-launch marker it holds.
fn show_anticheat_dialog(game: &str, on_continue: Continuation, window: &mut Window, cx: &mut App) {
    let body: SharedString = format!(
        "{game} has an online mode with anti-cheat. Use trainers in single-player \
only; using one online can get your account banned."
    )
    .into();
    show_confirm_dialog(
        Question {
            id: "anticheat",
            title: "Online Anti-Cheat",
            body,
            ok_text: "Launch Anyway",
            // As in every release before: the safe answer is the default.
            default: DefaultButton::Cancel,
        },
        on_continue,
        window,
        cx,
    );
}

/// Launch a trainer against an already-resolved target, off the UI thread.
/// Shared by the normal launch path and by the post-setup auto-launch once
/// the one-time .NET install succeeds. On success, the returned pgid is
/// recorded in `running` (see launcher::launch_trainer) and the list is
/// refreshed so the row picks up the running badge/Stop button; the periodic
/// poll in MainView takes over from there.
fn launch_trainer_now(
    target: LaunchTarget,
    trainer: Trainer,
    ui: Ui,
    guard: Rc<PendingLaunch>,
    window: &mut Window,
    cx: &mut App,
) {
    let trainer_name = trainer.name.clone();
    let trainer_path = trainer.path.clone();
    let log = applog::log_path();
    let appid = target.appid;
    applog::log(&format!("UI: Launch clicked for trainer {trainer_name}"));

    spawn_async(
        window,
        cx,
        async move {
            tokio::task::spawn_blocking(move || {
                launcher::launch_trainer(&target, &trainer_path, &log)
            })
            .await
        },
        move |launch_result, window, cx| match launch_result {
            Ok(Ok(pgid)) => {
                let _guard = &guard;
                toast(
                    window,
                    cx,
                    &format!("Launched {trainer_name} (AppId {appid})"),
                );
                ui.running.borrow_mut().insert(trainer.path.clone(), pgid);
                ui.refresh(window, cx);
            }
            Ok(Err(e)) => {
                launcher::record_failure(&format!(
                    "Could not start the trainer through Proton: {e:#}"
                ));
                toast(window, cx, &format!("Launch failed: {e}"));
            }
            Err(e) => toast(window, cx, &format!("Task error: {e}")),
        },
    );
}

/// Stop a running trainer: SIGTERM/SIGKILL its whole process group off the
/// UI thread (see launcher::stop_trainer), then drop it from `running`
/// and refresh the list so the row goes back to a Launch button.
fn stop_clicked(ui: &Ui, trainer_path: PathBuf, pgid: u32, window: &mut Window, cx: &mut App) {
    let ui = ui.clone();
    applog::log(&format!("UI: Stop clicked for pgid {pgid}"));

    spawn_async(
        window,
        cx,
        async move { tokio::task::spawn_blocking(move || launcher::stop_trainer(pgid)).await },
        move |result, window, cx| {
            if let Err(e) = result {
                toast(window, cx, &format!("Stop task error: {e}"));
            } else {
                toast(window, cx, "Trainer stopped");
            }
            // The poll may have dropped the exited group already and the
            // trainer been launched again before this ran: that new
            // launch stays tracked.
            untrack(&mut ui.running.borrow_mut(), &trainer_path, pgid);
            ui.refresh(window, cx);
        },
    );
}

// ─────────────────────────────────────────────────────────────────────────────
//  .NET one-time setup dialog
// ─────────────────────────────────────────────────────────────────────────────

/// The dialog's body: the explanation, and the manual commands behind a
/// disclosure (open from the start where there is no automatic setup).
struct DotnetPrompt {
    body: SharedString,
    commands: SharedString,
    automatic: bool,
    expanded: bool,
}

impl Render for DotnetPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chevron = if self.expanded {
            gpui_kit::component::IconName::ChevronDown
        } else {
            gpui_kit::component::IconName::ChevronRight
        };
        let commands = self.commands.clone();
        v_flex()
            .gap_3()
            // "Show manual commands instead" swaps the explanation for the
            // commands, so they are in view at once; where the commands are
            // all there is, both show.
            .when(!(self.automatic && self.expanded), |prompt| {
                prompt.child(label("body", self.body.clone()))
            })
            .child(
                Collapsible::new()
                    .open(self.expanded)
                    .gap_2()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                Button::new("dotnet-commands-toggle")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(chevron))
                                    .label(if self.automatic {
                                        "Show manual commands instead"
                                    } else {
                                        "Manual commands"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.expanded = !this.expanded;
                                        cx.notify();
                                    })),
                            )
                            .when(self.expanded, |row| {
                                row.child(
                                    Button::new("dotnet-commands-copy")
                                        .ghost()
                                        .small()
                                        .icon(Icon::new(gpui_kit::component::IconName::Copy))
                                        .tooltip("Copy the commands")
                                        .accessibility_label("Copy the commands")
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                commands.to_string(),
                                            ));
                                        }),
                                )
                            }),
                    )
                    .content(
                        div()
                            .id("dotnet-commands")
                            .max_h(px(220.))
                            .overflow_y_scroll()
                            .child(Unseen("dotnet-commands-spoken", self.commands.clone()))
                            .child(
                                TextView::markdown(
                                    "dotnet-commands-text",
                                    format!("```sh\n{}\n```", self.commands),
                                )
                                .selectable(true),
                            ),
                    ),
            )
    }
}

const DOTNET_TITLE: &str = "One-Time Setup Needed";

fn show_dotnet_dialog(
    target: LaunchTarget,
    trainer: Trainer,
    ui: Ui,
    guard: Rc<PendingLaunch>,
    health: &str,
    window: &mut Window,
    cx: &mut App,
) {
    // The privileged setup script is apt-only; elsewhere offer just the
    // manual command list rather than a password prompt that then fails.
    let automatic = setup::apt_available();
    let body = if automatic {
        "This game needs a one-time Windows compatibility component before trainers \
will run. Setup copies it from another game on this system that has it. If none does, \
it is installed instead, which needs the game closed and may ask for your password. \
It takes a minute or two."
    } else {
        "This game needs a one-time Windows compatibility component before trainers \
will run. Automatic setup is only available on Debian/Ubuntu-based systems; run the \
commands below yourself, then launch the trainer again."
    };
    let prompt = cx.new(|_| DotnetPrompt {
        body: format!("{body}\n\nPrefix check: {health}.").into(),
        commands: setup::manual_commands(&target, automatic).into(),
        automatic,
        expanded: !automatic,
    });

    // The dialog is rebuilt on every frame, so the non-Clone target/trainer
    // are handed off through a RefCell rather than moved directly — only the
    // first response takes them. The guard goes along, so the pending marker
    // lasts as long as the dialog (and the setup it triggers).
    let state = Rc::new(RefCell::new(Some((target, trainer, guard))));

    let focus = InitialFocus::new(cx);
    let cancel_focus = cx.focus_handle();
    window.open_dialog(cx, move |dialog, window, cx| {
        focus.apply(window, cx);
        let dialog = dialog
            .title(dialog_title(DOTNET_TITLE))
            .overlay(false)
            .w(px(440.));
        let content = named_group("dotnet", DOTNET_TITLE).child(
            div()
                .id("body")
                .max_h(dialog_room(window))
                .child(prompt.clone())
                // With a scrollbar: the manual commands open below the fold.
                .overflow_y_scrollbar(),
        );
        if !automatic {
            let on_close_button = focus.handle.clone();
            // Enter closes only from the Close button, not from the commands.
            let dialog = dialog.on_ok(move |_, window, _| on_close_button.is_focused(window));
            return dialog.child(
                content.child(
                    dialog_buttons().child(
                        Button::new("dotnet-close")
                            .track_focus(&focus.handle)
                            .label("Close")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                ),
            );
        }
        let set_up = {
            let state = state.clone();
            let ui = ui.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some((target, trainer, guard)) = state.borrow_mut().take() {
                    let ui = ui.clone();
                    // Once this dialog is out of the way: the setup may open the next one.
                    window.defer(cx, move |window, cx| {
                        run_automatic_setup(target, trainer, ui, guard, window, cx)
                    });
                }
            }
        };
        let enter = set_up.clone();
        let enter_cancel = state.clone();
        let cancel = state.clone();
        let dismissed = state.clone();
        let (on_action, on_cancel) = (focus.handle.clone(), cancel_focus.clone());
        dialog
            // Enter: what the focused button says (see `EnterKey`); on the
            // commands' disclosure or copy button it decides nothing.
            .on_ok(move |_, window, cx| {
                match enter_key(on_action.is_focused(window), on_cancel.is_focused(window)) {
                    EnterKey::Confirm => enter(window, cx),
                    EnterKey::Cancel => {
                        enter_cancel.borrow_mut().take();
                    }
                    EnterKey::Nothing => return false,
                }
                true
            })
            // Escape and the close button drop the pending launch with the dialog.
            .on_close(move |_, _, _| {
                dismissed.borrow_mut().take();
            })
            .child(
                content.child(
                    dialog_buttons()
                        .child(
                            Button::new("dotnet-cancel")
                                .label("Cancel")
                                .track_focus(&cancel_focus)
                                .on_click(move |_, window, cx| {
                                    cancel.borrow_mut().take();
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("dotnet-setup")
                                .primary()
                                .label("Set Up Automatically")
                                .track_focus(&focus.handle)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    set_up(window, cx);
                                }),
                        ),
                ),
            )
    });
}

/// What the first, game-running part of the automatic setup found.
enum CloneOutcome {
    /// .NET was copied in from another prefix and checks out: launch.
    Ready(LaunchTarget),
    /// Nothing on this system to copy from: the installer is needed.
    NoDonor(LaunchTarget),
}

/// Runs the automatic setup off the UI thread. First, with the game still
/// running, it clones a working .NET from another game's prefix and, if that
/// checks out, launches the trainer. Cloning is tried first, and the
/// Microsoft installer only as a fallback: on Wine's new wow64 mode that
/// installer fails outright and its rollback strips .NET back out, leaving
/// the prefix worse than it started. Cloning needs no system packages
/// either, so the pkexec prompt is only reached when there's nothing to
/// clone. Errors surface via a notification with the real error message; the
/// setup script and install_dotnet48 also log to /var/log/steam-punk.log.
fn run_automatic_setup(
    target: LaunchTarget,
    trainer: Trainer,
    ui: Ui,
    guard: Rc<PendingLaunch>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(repair) = PrefixRepair::begin(target.prefix_dir()) else {
        toast(window, cx, "A .NET setup for this game is already running");
        return;
    };
    toast(
        window,
        cx,
        "Setting up .NET — this may take a minute or two…",
    );

    spawn_async(
        window,
        cx,
        async move {
            tokio::task::spawn_blocking(move || -> anyhow::Result<CloneOutcome> {
                if !launcher::repair_dotnet_from_sibling_prefix(&target)? {
                    return Ok(CloneOutcome::NoDonor(target));
                }
                let (usable, health) = launcher::prefix_health(&target);
                if !usable {
                    anyhow::bail!(
                        "Copied .NET from another game's prefix, but this one still isn't ready \
                         ({health}). The details are in ~/.local/share/steam-punk/steam-punk.log."
                    );
                }
                Ok(CloneOutcome::Ready(target))
            })
            .await
        },
        move |result, window, cx| {
            // Held until the result is handled: no other setup starts on this prefix meanwhile.
            let _repair = repair;
            match result {
                Ok(Ok(CloneOutcome::Ready(target))) => {
                    launch_trainer_now(target, trainer, ui, guard, window, cx)
                }
                Ok(Ok(CloneOutcome::NoDonor(target))) => {
                    show_quit_game_dialog(target, guard, false, window, cx)
                }
                Ok(Err(e)) => toast(window, cx, &format!("Setup failed: {e:#}")),
                Err(e) => toast(window, cx, &format!("Task error: {e}")),
            }
        },
    );
}

/// How long Install waits for the game's wineserver to go away after the
/// user quit the game (it lingers a few seconds after the last process).
const QUIT_WAIT: Duration = Duration::from_secs(30);

/// The installer fallback. winetricks can't run against a prefix a game is
/// using (see setup::install_dotnet48), so the user quits the game first;
/// the trainer is launched by hand once the game is running again.
fn show_quit_game_dialog(
    target: LaunchTarget,
    guard: Rc<PendingLaunch>,
    still_running: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let mut body = "No other game on this system has a working .NET runtime to copy, so it \
has to be installed with Microsoft's installer, which can only run while the game is closed. \
Quit the game, then press Install. When it is done, start the game again and launch the \
trainer."
        .to_string();
    if still_running {
        body.push_str("\n\nThe game is still running.");
    }
    let body: SharedString = body.into();

    show_confirm_dialog(
        Question {
            id: "quit-game",
            title: "Quit the Game to Continue",
            body,
            ok_text: "Install",
            default: DefaultButton::Action,
        },
        // Cancelling drops this, and with it the pending-launch marker.
        Box::new(move |window, cx| run_installer(target, guard, window, cx)),
        window,
        cx,
    );
}

enum InstallOutcome {
    Done,
    StillRunning(LaunchTarget),
}

fn run_installer(
    target: LaunchTarget,
    guard: Rc<PendingLaunch>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(repair) = PrefixRepair::begin(target.prefix_dir()) else {
        toast(window, cx, "A .NET setup for this game is already running");
        return;
    };
    toast(window, cx, "Installing .NET — this takes a few minutes…");
    spawn_async(
        window,
        cx,
        async move {
            tokio::task::spawn_blocking(move || -> anyhow::Result<InstallOutcome> {
                let prefix = target.prefix_dir();
                let deadline = std::time::Instant::now() + QUIT_WAIT;
                while launcher::prefix_in_use(&prefix) {
                    if std::time::Instant::now() >= deadline {
                        return Ok(InstallOutcome::StillRunning(target));
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                if !setup::system_prereqs_present() {
                    setup::run_system_setup()?;
                }
                setup::install_dotnet48(&target)?;
                let (usable, health) = launcher::prefix_health(&target);
                if !usable {
                    anyhow::bail!(
                        "The .NET installer finished, but the prefix still isn't ready ({health}). \
                         The details are in ~/.local/share/steam-punk/steam-punk.log."
                    );
                }
                Ok(InstallOutcome::Done)
            })
            .await
        },
        move |result, window, cx| {
            // Released first: the quit-game dialog's Install starts a setup of its own.
            drop(repair);
            match result {
                Ok(Ok(InstallOutcome::Done)) => toast(
                    window,
                    cx,
                    ".NET is installed. Start the game again, then launch the trainer.",
                ),
                Ok(Ok(InstallOutcome::StillRunning(target))) => {
                    show_quit_game_dialog(target, guard, true, window, cx)
                }
                Ok(Err(e)) => toast(window, cx, &format!("Setup failed: {e:#}")),
                Err(e) => toast(window, cx, &format!("Task error: {e}")),
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{
        enter_key, first_installed, unreported, untrack, EnterKey, PrefixRepair, MONO_FONTS,
        UI_FONTS,
    };
    use super::{helper_showed, no_window_message};

    #[test]
    fn an_error_dialog_that_was_seen_is_not_shown_twice() {
        assert!(helper_showed("zenity", Some(0)));
        assert!(
            helper_showed("zenity", Some(1)),
            "closed without OK: still seen"
        );
        assert!(helper_showed("kdialog", Some(1)));
        assert!(
            !helper_showed("zenity", Some(255)),
            "could not open a display"
        );
        assert!(!helper_showed("kdialog", None), "killed by a signal");
        assert!(helper_showed("notify-send", Some(0)));
        assert!(
            !helper_showed("notify-send", Some(1)),
            "no notification service"
        );
    }

    #[test]
    fn the_no_window_message_names_the_real_cause() {
        let gpu = no_window_message("x", true);
        assert!(gpu.contains("graphics driver") && gpu.ends_with("Details: x"));
        let headless = no_window_message("x", false);
        assert!(headless.contains("no display") && !headless.contains("graphics driver"));
    }

    #[test]
    fn enter_in_the_appid_dialog_presses_the_focused_button() {
        use super::{appid_enter, AppIdEnter};
        assert_eq!(appid_enter(true, false, false, None), AppIdEnter::Field);
        assert_eq!(appid_enter(false, true, false, None), AppIdEnter::Skip);
        assert_eq!(appid_enter(false, false, true, None), AppIdEnter::Add);
        assert_eq!(
            appid_enter(false, false, false, Some(7)),
            AppIdEnter::Use(7)
        );
        assert_eq!(
            appid_enter(false, false, false, None),
            AppIdEnter::Close,
            "on the close button Enter closes, which skips"
        );
    }

    #[test]
    fn enter_acts_as_the_focused_button() {
        assert_eq!(enter_key(true, false), EnterKey::Confirm);
        assert_eq!(
            enter_key(false, true),
            EnterKey::Cancel,
            "Enter on Cancel cancels"
        );
        assert_eq!(
            enter_key(false, false),
            EnterKey::Nothing,
            "Enter on anything else decides nothing"
        );
    }

    #[test]
    fn a_trainers_file_problem_is_reported_until_it_was_shown() {
        let damaged = Some("trainers.json is damaged".to_string());
        assert_eq!(unreported(&damaged, &None), damaged.as_ref());
        assert_eq!(unreported(&damaged, &damaged), None, "shown once");
        assert_eq!(unreported(&None, &damaged), None, "nothing wrong any more");
        let other = Some("another problem".to_string());
        assert_eq!(unreported(&other, &damaged), other.as_ref());
    }

    #[test]
    fn fonts_are_the_first_installed_of_the_list() {
        let installed = ["Noto Sans Mono", "Noto Sans", "Cantarell"].map(String::from);
        assert_eq!(first_installed(&UI_FONTS, &installed), Some("Cantarell"));
        assert_eq!(
            first_installed(&MONO_FONTS, &installed),
            Some("Noto Sans Mono")
        );
        assert_eq!(first_installed(&UI_FONTS, &[]), None);
    }

    #[test]
    fn one_dotnet_setup_at_a_time_per_prefix() {
        let a = PathBuf::from("/compatdata/1/pfx");
        let first = PrefixRepair::begin(a.clone()).expect("the first setup starts");
        assert!(
            PrefixRepair::begin(a.clone()).is_none(),
            "a second one on the same prefix does not"
        );
        let other = PrefixRepair::begin(PathBuf::from("/compatdata/2/pfx"));
        assert!(other.is_some(), "another game's prefix is independent");
        drop(first);
        assert!(
            PrefixRepair::begin(a).is_some(),
            "free again once the first has finished"
        );
    }
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn stopping_a_launch_leaves_a_newer_one_tracked() {
        let path = PathBuf::from("/trainers/a.exe");
        let mut running = HashMap::from([(path.clone(), 200)]);
        // Stop of the old launch (pgid 100) finishing after a relaunch.
        assert!(!untrack(&mut running, &path, 100));
        assert_eq!(
            running.get(&path),
            Some(&200),
            "the new launch stays tracked"
        );
        assert!(untrack(&mut running, &path, 200));
        assert!(running.is_empty());
        assert!(
            !untrack(&mut running, &path, 200),
            "nothing tracked: nothing removed"
        );
    }
}
