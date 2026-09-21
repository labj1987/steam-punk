use crate::applog;
use crate::gamedata;
use crate::launcher::{self, LaunchTarget};
use crate::library::{self, Trainer};
use crate::setup;

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, ContentFit, DropTarget, FileDialog, FileFilter, Label, ListBox,
    Orientation, Picture, ScrolledWindow, SearchEntry, SelectionMode, Stack,
};
use libadwaita::prelude::*;
use libadwaita::{
    AboutDialog, ActionRow, AlertDialog, Application, ApplicationWindow, Dialog as AdwDialog,
    ExpanderRow, HeaderBar, ResponseAppearance, StatusPage, Toast, ToastOverlay,
};

use std::cell::RefCell;
use std::rc::Rc;

// ─────────────────────────────────────────────────────────────────────────────
//  Async bridge: run a future on the shared Tokio runtime, deliver the result
//  back on the GTK main thread.
// ─────────────────────────────────────────────────────────────────────────────

// ActionRow/AdwDialog titles and subtitles are interpreted as Pango markup,
// not plain text — an unescaped `&` (e.g. in "Mount & Blade II: Bannerlord")
// breaks parsing and the label silently renders blank. Escape any string
// that didn't originate as a literal in this file before handing it to a
// title/subtitle/heading setter.
fn esc(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

fn spawn_async<F, T, CB>(future: F, callback: CB)
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
    CB: FnOnce(T) + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel::<T>();
    crate::runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    glib::MainContext::default().spawn_local(async move {
        if let Ok(val) = rx.await {
            callback(val);
        }
    });
}

// ─────────────────────────────────────────────────────────────────────────────
//  build_ui
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_ui(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("SteamPunk")
        .default_width(480)
        .default_height(560)
        .build();

    let toast_overlay = ToastOverlay::new();

    // Tracked pgid per running trainer (keyed by trainer file path), for the
    // lifetime of the app session — not just at launch time. See
    // launcher::launch_trainer's doc comment for why the process group,
    // not a single pid, is what's tracked.
    let running: Rc<RefCell<std::collections::HashMap<std::path::PathBuf, u32>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));

    // AppIDs whose cover-art fetch has already been retried this session —
    // caps retries at one per AppID per run rather than re-attempting every
    // refresh_list() if it keeps failing (e.g. genuinely offline).
    let cover_retry_attempted: Rc<RefCell<std::collections::HashSet<u32>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));

    // ── Header ───────────────────────────────────────────────────────────────
    let header = HeaderBar::new();

    let import_btn = Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Import trainer .exe")
        .build();
    header.pack_start(&import_btn);

    let stop_all_btn = Button::builder().label("Stop All").build();
    header.pack_start(&stop_all_btn);

    let about_btn = Button::builder()
        .icon_name("help-about-symbolic")
        .tooltip_text("About")
        .build();
    header.pack_end(&about_btn);

    let save_log_btn = Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Save Debug Log")
        .build();
    header.pack_end(&save_log_btn);

    let troubleshoot_btn = Button::builder().label("Fix Stale Instance").build();
    header.pack_end(&troubleshoot_btn);

    // ── Stack: empty status page <-> trainer list ───────────────────────────
    let stack = Stack::new();
    // Defensive: without this, Stack's default vhomogeneous=true couples
    // the "list" page's height request to the "empty" page's, even though
    // only one is ever visible. Not the fix for the list-compresses-instead-
    // of-scrolling bug (see max_content_height below for that), but no
    // reason to leave the coupling in place either.
    stack.set_vhomogeneous(false);

    let status_page = StatusPage::builder()
        .icon_name("input-gaming-symbolic")
        .title("No Trainers Yet")
        .description("Drop a trainer .exe here, or use the + button")
        .build();
    stack.add_named(&status_page, Some("empty"));

    let list_box = ListBox::new();
    list_box.set_selection_mode(SelectionMode::None);
    list_box.add_css_class("boxed-list");
    list_box.set_margin_top(12);
    list_box.set_margin_bottom(12);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    list_box.set_valign(Align::Start);
    let list_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .propagate_natural_height(false)
        // Root cause of rows compressing instead of scrolling, confirmed by
        // screenshotting a real build: without max-content-height, a
        // ScrolledWindow tries to grow to fit ALL of its content before
        // ever committing to scrolling — so on first launch, or on a large/
        // maximized window, GTK sizes the window (or WM caps it at the
        // screen edge) to fit as much as it can, and whatever doesn't fit
        // gets silently clipped rather than scrolled. Capping how tall the
        // list is allowed to grow forces it to commit to scrolling once
        // content exceeds this height, regardless of window/screen size.
        .max_content_height(600)
        .child(&list_box)
        .build();
    stack.add_named(&list_scroll, Some("list"));

    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&header);
    content.append(&stack);
    toast_overlay.set_child(Some(&content));
    window.set_content(Some(&toast_overlay));

    // ─────────────────────────────────────────────────────────────────────────
    //  refresh_list — re-read the trainers dir and rebuild the list/rows
    // ─────────────────────────────────────────────────────────────────────────

    // Forward-declaration slot so the Remove button (defined inside the
    // closure being built) can call refresh_list once it exists.
    let self_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    let refresh_list: Rc<dyn Fn()> = {
        let list_box = list_box.clone();
        let list_scroll = list_scroll.clone();
        let stack = stack.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let self_slot = self_slot.clone();
        let running = running.clone();
        let cover_retry_attempted = cover_retry_attempted.clone();

        Rc::new(move || {
            while let Some(c) = list_box.first_child() {
                list_box.remove(&c);
            }

            let trainers = library::list_trainers().unwrap_or_default();
            if trainers.is_empty() {
                stack.set_visible_child_name("empty");
                return;
            }
            stack.set_visible_child_name("list");

            for trainer in trainers {
                let row = ActionRow::builder().title(esc(trainer.display_name())).build();
                if let Some(appid) = trainer.appid {
                    row.set_tooltip_text(Some(&format!("Steam AppID {appid}")));
                }
                if let Some(cover_path) = &trainer.cover_path {
                    let picture = Picture::for_filename(cover_path);
                    picture.set_content_fit(ContentFit::Cover);
                    picture.set_size_request(80, 45);
                    picture.set_valign(Align::Center);
                    picture.add_css_class("card");
                    row.add_prefix(&picture);
                }
                // A name or cover that's still missing — the initial fetch
                // failed (e.g. imported offline), or an AppID whose guessed
                // CDN paths both 404 (confirmed live for some current-gen
                // games) — would otherwise stay missing forever, since
                // fetch_and_cache only runs at association time. Retry once
                // per AppID per session whenever either is absent.
                if let Some(appid) = trainer.appid {
                    if (trainer.game_name.is_none() || trainer.cover_path.is_none())
                        && cover_retry_attempted.borrow_mut().insert(appid)
                    {
                        let refresh = self_slot.clone();
                        spawn_async(gamedata::fetch_and_cache(appid), move |result| {
                            if let Err(e) = result {
                                applog::log(&format!(
                                    "gamedata: name/cover retry failed for AppID {appid}: {e}"
                                ));
                            }
                            if let Some(reload) = refresh.borrow().clone() {
                                reload();
                            }
                        });
                    }
                }

                // pgid 0 = launch pending (no process yet): show Launch, whose
                // click is a no-op until the launch resolves.
                let running_pgid = running
                    .borrow()
                    .get(&trainer.path)
                    .copied()
                    .filter(|&p| p != 0);

                if let Some(pgid) = running_pgid {
                    let running_badge = Label::new(Some("Running"));
                    running_badge.add_css_class("success");
                    running_badge.add_css_class("caption");
                    running_badge.set_valign(Align::Center);
                    row.add_suffix(&running_badge);

                    let stop_btn = Button::builder()
                        .label("Stop")
                        .valign(Align::Center)
                        .build();
                    stop_btn.add_css_class("destructive-action");
                    wire_stop_button(&stop_btn, trainer.path.clone(), pgid, running.clone(), self_slot.clone(), toast_overlay.clone());
                    row.add_suffix(&stop_btn);
                } else {
                    let launch_btn = Button::builder()
                        .label("Launch")
                        .valign(Align::Center)
                        .build();
                    launch_btn.add_css_class("suggested-action");
                    wire_launch_button(&launch_btn, trainer.clone(), window.clone(), toast_overlay.clone(), running.clone(), self_slot.clone());
                    row.add_suffix(&launch_btn);
                }

                let remove_btn = Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text("Remove")
                    .valign(Align::Center)
                    .build();
                remove_btn.add_css_class("flat");
                {
                    let trainer = trainer.clone();
                    let toast_overlay = toast_overlay.clone();
                    let self_slot = self_slot.clone();
                    remove_btn.connect_clicked(move |_| {
                        if let Err(e) = library::remove_trainer(&trainer.path) {
                            toast_overlay.add_toast(Toast::new(&format!("Remove failed: {e}")));
                            return;
                        }
                        if let Some(reload) = self_slot.borrow().clone() {
                            reload();
                        }
                    });
                }
                row.add_suffix(&remove_btn);

                list_box.append(&row);
            }

            // Force GTK to remeasure the list against its new row count
            // rather than potentially reusing a stale natural-height
            // measurement from before this rebuild — the append() calls
            // above should already invalidate this, but explicitly
            // queuing it here is cheap insurance against exactly the
            // "rows compress instead of scrolling" symptom this app has
            // shown outside of an interactive window resize (which does
            // force a fresh measure/allocate pass on its own).
            list_box.queue_resize();
            list_scroll.queue_resize();
        })
    };
    *self_slot.borrow_mut() = Some(refresh_list.clone());

    refresh_list();

    // ─────────────────────────────────────────────────────────────────────────
    //  Running-trainer poll — non-blocking try_wait() on a single pid isn't
    //  enough (see launcher::process_group_alive's doc comment), so this
    //  periodically re-checks each tracked pgid instead. Runs for the app's
    //  session lifetime; only touches the list when something actually
    //  changed, to avoid rebuilding rows every tick for nothing.
    // ─────────────────────────────────────────────────────────────────────────
    {
        let running = running.clone();
        let refresh_list = refresh_list.clone();
        // The /proc scan runs on a blocking thread, never the GTK thread; the
        // flag keeps a slow scan from stacking up overlapping ones.
        let in_flight = Rc::new(std::cell::Cell::new(false));
        glib::timeout_add_local(std::time::Duration::from_millis(1000), move || {
            if in_flight.get() {
                return glib::ControlFlow::Continue;
            }
            let tracked: Vec<(std::path::PathBuf, u32)> = running
                .borrow()
                .iter()
                .filter(|(_, &pgid)| pgid != 0)
                .map(|(path, &pgid)| (path.clone(), pgid))
                .collect();
            if tracked.is_empty() {
                return glib::ControlFlow::Continue;
            }
            in_flight.set(true);
            let running = running.clone();
            let refresh_list = refresh_list.clone();
            let in_flight = in_flight.clone();
            spawn_async(
                async move {
                    tokio::task::spawn_blocking(move || {
                        tracked
                            .into_iter()
                            .filter(|(_, pgid)| !launcher::process_group_alive(*pgid))
                            .collect::<Vec<_>>()
                    })
                    .await
                },
                move |result| {
                    in_flight.set(false);
                    let Ok(exited) = result else { return };
                    let mut changed = false;
                    {
                        let mut r = running.borrow_mut();
                        for (path, pgid) in &exited {
                            // Only if it's still the same launch (not stopped
                            // and relaunched while the scan ran).
                            if r.get(path) == Some(pgid) {
                                applog::log(&format!(
                                    "UI: trainer at {} exited on its own",
                                    path.display()
                                ));
                                r.remove(path);
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        refresh_list();
                    }
                },
            );
            glib::ControlFlow::Continue
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Import — file picker
    // ─────────────────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let refresh_list = refresh_list.clone();
        let toast_overlay = toast_overlay.clone();
        import_btn.connect_clicked(move |_| {
            let filter = FileFilter::new();
            filter.add_pattern("*.exe");
            filter.set_name(Some("Trainer executable (.exe)"));
            let filters = gio::ListStore::new::<FileFilter>();
            filters.append(&filter);

            let dialog = FileDialog::builder()
                .title("Import Trainer")
                .filters(&filters)
                .build();

            let refresh_list = refresh_list.clone();
            let toast_overlay = toast_overlay.clone();
            let window2 = window.clone();
            dialog.open(Some(&window), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                let result = library::import_trainer(&path);
                applog::log(&format!("UI: import_trainer({}) -> {result:?}", path.display()));
                match result {
                    Ok(dest) => {
                        let queue = Rc::new(RefCell::new(vec![dest]));
                        prompt_appid_for_queue(window2.clone(), queue, refresh_list.clone(), toast_overlay.clone());
                    }
                    Err(e) => toast_overlay.add_toast(Toast::new(&format!("Import failed: {e}"))),
                }
            });
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Drop target — accept dropped .exe files anywhere in the window
    // ─────────────────────────────────────────────────────────────────────────
    {
        let drop_target = DropTarget::new(gdk4::FileList::static_type(), gdk4::DragAction::COPY);
        let window = window.clone();
        let refresh_list = refresh_list.clone();
        let toast_overlay = toast_overlay.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            let Ok(file_list) = value.get::<gdk4::FileList>() else {
                return false;
            };
            let mut imported: Vec<std::path::PathBuf> = Vec::new();
            for file in file_list.files() {
                let Some(path) = file.path() else { continue };
                let is_exe = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("exe"))
                    .unwrap_or(false);
                if !is_exe {
                    continue;
                }
                match library::import_trainer(&path) {
                    Ok(dest) => imported.push(dest),
                    Err(e) => toast_overlay.add_toast(Toast::new(&format!("Import failed: {e}"))),
                }
            }
            let imported_any = !imported.is_empty();
            if imported_any {
                let queue = Rc::new(RefCell::new(imported));
                prompt_appid_for_queue(window.clone(), queue, refresh_list.clone(), toast_overlay.clone());
            }
            imported_any
        });
        content.add_controller(drop_target);
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Stop All
    // ─────────────────────────────────────────────────────────────────────────
    {
        let toast_overlay = toast_overlay.clone();
        let running = running.clone();
        let refresh_list = refresh_list.clone();
        stop_all_btn.connect_clicked(move |_| {
            let toast_overlay = toast_overlay.clone();
            let running = running.clone();
            let refresh_list = refresh_list.clone();
            // Only real, tracked process groups; pgid 0 is a launch that
            // hasn't produced a process yet.
            let pgids: Vec<u32> = running.borrow().values().copied().filter(|&p| p != 0).collect();
            spawn_async(
                async move {
                    tokio::task::spawn_blocking(move || launcher::stop_all(&pgids)).await
                },
                move |_| {
                    running.borrow_mut().retain(|_, &mut pgid| pgid == 0);
                    refresh_list();
                    toast_overlay.add_toast(Toast::new("Stopped all trainers"));
                },
            );
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Fix Stale Instance
    // ─────────────────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        troubleshoot_btn.connect_clicked(move |_| {
            show_troubleshoot(&window, &toast_overlay);
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  About
    // ─────────────────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        about_btn.connect_clicked(move |_| {
            let dialog = AboutDialog::builder()
                .application_name("SteamPunk")
                .version(env!("CARGO_PKG_VERSION"))
                .developers(vec!["Linnard Alex Brown Jr."])
                .comments(
                    "Launches Windows game trainers through Proton against a running \
                     Steam game's wine session.",
                )
                .build();
            dialog.add_acknowledgement_section(Some("Built with"), &["Claude Code (Anthropic)"]);
            dialog.present(Some(&window));
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Save Debug Log — bundles the app log + privileged setup log (if
    //  readable) into one file the user picks, for handing to whoever's
    //  troubleshooting a failed launch.
    // ─────────────────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        save_log_btn.connect_clicked(move |_| {
            let dialog = FileDialog::builder()
                .title("Save Debug Log")
                .initial_name(format!("steampunk-log-{}.txt", applog::filename_timestamp()))
                .build();

            let toast_overlay = toast_overlay.clone();
            dialog.save(Some(&window), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                match applog::export_to(&path) {
                    Ok(()) => toast_overlay.add_toast(Toast::new("Debug log saved")),
                    Err(e) => toast_overlay.add_toast(Toast::new(&format!("Save failed: {e}"))),
                }
            });
        });
    }

    window.present();
}

// ─────────────────────────────────────────────────────────────────────────────
//  Optional AppID prompt — shown once per just-imported trainer, right
//  after import. Purely additive: skipping (or entering nothing) leaves the
//  trainer exactly as it was before this feature existed — filename title,
//  no art. Drives itself through `queue` one file at a time so a
//  multi-file drop prompts for each in turn instead of only the first.
//
//  A custom `libadwaita::Dialog` rather than `AlertDialog`: this version
//  needs several independent ways to finish (a search-result click, the
//  manual "Add" button, "Skip"), and `AlertDialog` only supports firing one
//  of its own named responses. Building it from scratch means every one of
//  those paths is its own button handler that closes the dialog and
//  advances the queue exactly once — see `confirm`/`advance` below — so
//  there's no shared response signal two handlers could both fire.
// ─────────────────────────────────────────────────────────────────────────────

fn prompt_appid_for_queue(
    window: ApplicationWindow,
    queue: Rc<RefCell<Vec<std::path::PathBuf>>>,
    refresh_list: Rc<dyn Fn()>,
    toast_overlay: ToastOverlay,
) {
    let Some(trainer_path) = queue.borrow_mut().pop() else {
        refresh_list();
        toast_overlay.add_toast(Toast::new("Trainer imported"));
        return;
    };
    let Some(filename) = trainer_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    else {
        // No filename to key metadata by — skip straight to the next.
        prompt_appid_for_queue(window, queue, refresh_list, toast_overlay);
        return;
    };
    let stem = trainer_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| filename.clone());
    let guessed_term = gamedata::guess_search_term(&stem);

    // Moves on to the next queued trainer (or finishes the queue). Every
    // button/row handler below calls this exactly once, after closing the
    // dialog itself.
    let advance: Rc<dyn Fn()> = {
        let window = window.clone();
        let queue = queue.clone();
        let refresh_list = refresh_list.clone();
        let toast_overlay = toast_overlay.clone();
        Rc::new(move || {
            prompt_appid_for_queue(
                window.clone(),
                queue.clone(),
                refresh_list.clone(),
                toast_overlay.clone(),
            );
        })
    };

    let dialog = AdwDialog::builder()
        .title(format!("Add Steam AppID for \u{201c}{}\u{201d}?", esc(&filename)))
        .content_width(420)
        .content_height(480)
        .build();

    let outer = GtkBox::new(Orientation::Vertical, 8);
    outer.set_margin_top(12);
    outer.set_margin_bottom(12);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let hint = Label::new(Some(
        "Optionally search for this game to show its name and cover art in \
         the list instead of the raw trainer filename. Leave blank to skip \
         — the trainer still works either way.",
    ));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    outer.append(&hint);

    let search_entry = SearchEntry::new();
    search_entry.set_text(&guessed_term);
    search_entry.set_placeholder_text(Some("Search for a game, or enter a numeric AppID"));
    outer.append(&search_entry);

    let results_list = ListBox::new();
    results_list.set_selection_mode(SelectionMode::None);
    results_list.add_css_class("boxed-list");
    let results_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .child(&results_list)
        .build();
    outer.append(&results_scroll);

    let button_row = GtkBox::new(Orientation::Horizontal, 8);
    button_row.set_halign(Align::End);
    let skip_btn = Button::builder().label("Skip").build();
    let add_btn = Button::builder().label("Add").build();
    add_btn.add_css_class("suggested-action");
    button_row.append(&skip_btn);
    button_row.append(&add_btn);
    outer.append(&button_row);

    dialog.set_child(Some(&outer));

    // Every way of picking an AppID — a search-result row or the manual Add
    // button — funnels through here exactly once: closes the dialog, saves
    // the association, kicks off the (fire-and-forget) name/art fetch, and
    // advances the queue. Fetch failure only logs and still advances —
    // matches the existing "don't block adding" behavior.
    let confirm: Rc<dyn Fn(u32)> = {
        let dialog = dialog.clone();
        let filename = filename.clone();
        let refresh_list = refresh_list.clone();
        let toast_overlay = toast_overlay.clone();
        let advance = advance.clone();
        Rc::new(move |appid: u32| {
            dialog.close();
            match library::set_trainer_appid(&filename, appid) {
                Ok(()) => {
                    let refresh_list2 = refresh_list.clone();
                    spawn_async(gamedata::fetch_and_cache(appid), move |result| {
                        if let Err(e) = result {
                            applog::log(&format!("gamedata: fetch failed for AppID {appid}: {e}"));
                        }
                        refresh_list2();
                    });
                }
                Err(e) => {
                    toast_overlay.add_toast(Toast::new(&format!("Could not save AppID: {e}")));
                }
            }
            advance();
        })
    };

    // Renders one search result as an ActionRow; the thumbnail loads async
    // and drops in once it arrives (or never, if the fetch fails — the row
    // is still usable without it).
    let build_result_row = {
        let confirm = confirm.clone();
        move |result: gamedata::SearchResult| -> ActionRow {
            let row = ActionRow::builder()
                .title(esc(&result.name))
                .subtitle(format!("AppID {}", result.appid))
                .activatable(true)
                .build();

            if let Some(url) = result.thumbnail_url.clone() {
                let picture = Picture::new();
                picture.set_content_fit(ContentFit::Cover);
                picture.set_size_request(60, 34);
                picture.set_valign(Align::Center);
                picture.add_css_class("card");
                row.add_prefix(&picture);
                spawn_async(async move { gamedata::fetch_thumbnail(&url).await }, move |bytes| {
                    let Ok(bytes) = bytes else { return };
                    let gbytes = glib::Bytes::from(&bytes);
                    if let Ok(texture) = gdk4::Texture::from_bytes(&gbytes) {
                        picture.set_paintable(Some(&texture));
                    }
                });
            }

            let use_btn = Button::builder().label("Use").valign(Align::Center).build();
            {
                let confirm = confirm.clone();
                let appid = result.appid;
                use_btn.connect_clicked(move |_| confirm(appid));
            }
            row.add_suffix(&use_btn);

            {
                let confirm = confirm.clone();
                let appid = result.appid;
                row.connect_activated(move |_| confirm(appid));
            }

            row
        }
    };

    // Tracks the current top result's AppID so Enter can pick it — kept
    // separate from `results_list` itself since digging the first row's
    // AppID back out of the widget tree on every keypress would mean
    // stashing it as widget data anyway; a plain slot is simpler.
    let top_result: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));

    // Fires a search, guarded against out-of-order replies to fast typing:
    // a result set is only applied if the entry's text still matches the
    // term that produced it by the time the network call returns.
    let run_search: Rc<dyn Fn(String)> = {
        let search_entry = search_entry.clone();
        let results_list = results_list.clone();
        let build_result_row = build_result_row.clone();
        let top_result = top_result.clone();
        Rc::new(move |term: String| {
            let search_entry = search_entry.clone();
            let results_list = results_list.clone();
            let build_result_row = build_result_row.clone();
            let top_result = top_result.clone();
            let term_for_check = term.clone();
            spawn_async(async move { gamedata::search(&term).await }, move |results| {
                if search_entry.text().as_str() != term_for_check {
                    return;
                }
                while let Some(c) = results_list.first_child() {
                    results_list.remove(&c);
                }
                match results {
                    Ok(results) => {
                        *top_result.borrow_mut() = results.first().map(|r| r.appid);
                        for result in results {
                            results_list.append(&build_result_row(result));
                        }
                    }
                    Err(e) => {
                        *top_result.borrow_mut() = None;
                        applog::log(&format!(
                            "gamedata: search failed for {term_for_check:?}: {e}"
                        ));
                    }
                }
            });
        })
    };

    // Debounce: only search once typing has paused for 250ms, so a burst of
    // keystrokes produces one request instead of one per key.
    {
        let run_search = run_search.clone();
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        search_entry.connect_search_changed(move |entry| {
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            let term = entry.text().to_string();
            let run_search = run_search.clone();
            let slot = pending.clone();
            let id = glib::timeout_add_local_once(
                std::time::Duration::from_millis(250),
                move || {
                    // The source is spent once it fires; forget its id so a
                    // later keystroke doesn't try to remove it.
                    slot.borrow_mut().take();
                    run_search(term);
                },
            );
            *pending.borrow_mut() = Some(id);
        });
    }
    {
        let confirm = confirm.clone();
        let top_result = top_result.clone();
        search_entry.connect_activate(move |entry| {
            let text = entry.text();
            // A raw numeric AppID takes priority (matches the Add button's
            // behavior); otherwise Enter picks whatever's currently the top
            // search result, if any.
            if let Ok(appid) = text.trim().parse::<u32>() {
                confirm(appid);
            } else if let Some(appid) = *top_result.borrow() {
                confirm(appid);
            }
        });
    }

    // Fire the initial search immediately with the guessed term, so results
    // are already present when the dialog opens.
    run_search(guessed_term.clone());

    {
        let dialog = dialog.clone();
        let advance = advance.clone();
        skip_btn.connect_clicked(move |_| {
            dialog.close();
            advance();
        });
    }
    {
        let search_entry = search_entry.clone();
        let confirm = confirm.clone();
        let dialog = dialog.clone();
        let advance = advance.clone();
        let toast_overlay = toast_overlay.clone();
        add_btn.connect_clicked(move |_| {
            let text = search_entry.text();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                dialog.close();
                advance();
                return;
            }
            match trimmed.parse::<u32>() {
                Ok(appid) => confirm(appid),
                Err(_) => {
                    toast_overlay.add_toast(Toast::new(
                        "That doesn't look like a numeric AppID — skipped",
                    ));
                    dialog.close();
                    advance();
                }
            }
        });
    }

    dialog.present(Some(&window));
}

// ─────────────────────────────────────────────────────────────────────────────
//  Launch button wiring
// ─────────────────────────────────────────────────────────────────────────────

/// Work out which running game to act on and hand the resolved target to
/// `proceed`. When exactly one game is running it's used directly; when
/// several are, the user picks. Shared by the launch and troubleshoot paths so
/// both disambiguate the same way instead of silently taking whichever game
/// /proc happened to list first.
fn with_launch_target(
    window: &ApplicationWindow,
    toast_overlay: &ToastOverlay,
    proceed: Rc<dyn Fn(LaunchTarget)>,
) {
    let window = window.clone();
    let toast_overlay = toast_overlay.clone();

    spawn_async(
        async { tokio::task::spawn_blocking(launcher::running_games).await },
        move |result| {
            let games = match result {
                Ok(games) => games,
                Err(e) => {
                    toast_overlay.add_toast(Toast::new(&format!("Task error: {e}")));
                    return;
                }
            };

            match games.len() {
                0 => toast_overlay.add_toast(Toast::new(
                    "Start the game first, load past the menus, then launch the trainer.",
                )),
                1 => resolve_target_then(games[0].appid, &toast_overlay, proceed),
                _ => show_game_picker(&window, games, &toast_overlay, proceed),
            }
        },
    );
}

fn resolve_target_then(
    appid: u32,
    toast_overlay: &ToastOverlay,
    proceed: Rc<dyn Fn(LaunchTarget)>,
) {
    let toast_overlay = toast_overlay.clone();
    spawn_async(
        async move {
            tokio::task::spawn_blocking(move || launcher::resolve_launch_target(appid)).await
        },
        move |result| match result {
            Ok(Ok(target)) => proceed(target),
            Ok(Err(e)) => toast_overlay.add_toast(Toast::new(&e.to_string())),
            Err(e) => toast_overlay.add_toast(Toast::new(&format!("Task error: {e}"))),
        },
    );
}

/// One response button per running game. Trainers are game-specific, so
/// guessing here would attach to the wrong game's prefix rather than fail
/// visibly — worth an extra click in the rare case two games are open.
fn show_game_picker(
    window: &ApplicationWindow,
    games: Vec<launcher::RunningGame>,
    toast_overlay: &ToastOverlay,
    proceed: Rc<dyn Fn(LaunchTarget)>,
) {
    let dialog = AlertDialog::builder()
        .heading("Which Game?")
        .body("More than one game is running. Pick the one this trainer is for.")
        .build();

    for (i, game) in games.iter().enumerate() {
        dialog.add_response(&i.to_string(), &game.name);
    }
    dialog.add_response("cancel", "Cancel");
    dialog.set_close_response("cancel");

    let toast_overlay = toast_overlay.clone();
    dialog.connect_response(None, move |_dialog, response| {
        // "cancel" simply fails to parse as an index, which is the intent.
        let Ok(index) = response.parse::<usize>() else {
            return;
        };
        let Some(game) = games.get(index) else {
            return;
        };
        resolve_target_then(game.appid, &toast_overlay, proceed.clone());
    });

    dialog.present(Some(window));
}

type RunningMap = Rc<RefCell<std::collections::HashMap<std::path::PathBuf, u32>>>;

/// Marks a trainer as "launch pending" (pgid 0 in `running`) from the moment
/// Launch is clicked, so a second click before the launch resolves is a no-op
/// instead of starting a second, untracked process. The marker is removed when
/// the last clone of the guard drops — whichever way the flow ends (toast,
/// cancelled dialog, failure). A real pgid recorded meanwhile is left alone.
struct PendingLaunch {
    running: RunningMap,
    path: std::path::PathBuf,
}

impl Drop for PendingLaunch {
    fn drop(&mut self) {
        let mut r = self.running.borrow_mut();
        if r.get(&self.path) == Some(&0) {
            r.remove(&self.path);
        }
    }
}
type RefreshSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

fn wire_launch_button(
    btn: &Button,
    trainer: Trainer,
    window: ApplicationWindow,
    toast_overlay: ToastOverlay,
    running: RunningMap,
    refresh_slot: RefreshSlot,
) {
    btn.connect_clicked(move |_| {
        if running.borrow().contains_key(&trainer.path) {
            return;
        }
        running.borrow_mut().insert(trainer.path.clone(), 0);
        let guard = Rc::new(PendingLaunch {
            running: running.clone(),
            path: trainer.path.clone(),
        });
        let trainer = trainer.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let running = running.clone();
        let refresh_slot = refresh_slot.clone();

        let dialog_window = window.clone();
        let dialog_toasts = toast_overlay.clone();
        with_launch_target(
            &window,
            &toast_overlay,
            Rc::new(move |target| {
                let trainer = trainer.clone();
                let toasts = dialog_toasts.clone();
                let guard = guard.clone();
                let running = running.clone();
                let refresh_slot = refresh_slot.clone();
                let dialog_window = dialog_window.clone();
                // has_usable_dotnet parses the prefix's whole system.reg
                // (several MB) — keep it off the GTK thread.
                spawn_async(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let usable = launcher::has_usable_dotnet(&target);
                            (target, usable)
                        })
                        .await
                    },
                    move |result| match result {
                        Ok((target, true)) => {
                            launch_trainer_now(target, trainer, toasts, running, refresh_slot, guard)
                        }
                        Ok((target, false)) => show_dotnet_dialog(
                            &dialog_window, target, trainer, toasts, running, refresh_slot, guard,
                        ),
                        Err(e) => toasts.add_toast(Toast::new(&format!("Task error: {e}"))),
                    },
                );
            }),
        );
    });
}

/// Launch a trainer against an already-resolved target, off the main
/// thread. Shared by the normal launch path and by the post-setup
/// auto-launch once the one-time .NET install succeeds. On success, the
/// returned pgid is recorded in `running` (see launcher::launch_trainer)
/// and the list is refreshed so the row picks up the running badge/Stop
/// button; the periodic poll in build_ui takes over from there.
fn launch_trainer_now(
    target: LaunchTarget,
    trainer: Trainer,
    toast_overlay: ToastOverlay,
    running: RunningMap,
    refresh_slot: RefreshSlot,
    guard: Rc<PendingLaunch>,
) {
    let trainer_name = trainer.name.clone();
    let trainer_path = trainer.path.clone();
    let toast_overlay2 = toast_overlay.clone();
    let log = applog::log_path();
    let appid = target.appid;
    applog::log(&format!("UI: Launch clicked for trainer {trainer_name}"));

    spawn_async(
        async move {
            tokio::task::spawn_blocking(move || {
                launcher::launch_trainer(&target, &trainer_path, &log)
            })
            .await
        },
        move |launch_result| match launch_result {
            Ok(Ok(pgid)) => {
                let _guard = &guard;
                toast_overlay2.add_toast(Toast::new(&format!(
                    "Launched {trainer_name} (AppId {appid})"
                )));
                running.borrow_mut().insert(trainer.path.clone(), pgid);
                if let Some(refresh) = refresh_slot.borrow().clone() {
                    refresh();
                }
            }
            Ok(Err(e)) => toast_overlay2.add_toast(Toast::new(&format!("Launch failed: {e}"))),
            Err(e) => toast_overlay2.add_toast(Toast::new(&format!("Task error: {e}"))),
        },
    );
}

/// Stop a running trainer: SIGTERM/SIGKILL its whole process group off the
/// main thread (see launcher::stop_trainer), then drop it from `running`
/// and refresh the list so the row goes back to a Launch button.
fn wire_stop_button(
    btn: &Button,
    trainer_path: std::path::PathBuf,
    pgid: u32,
    running: RunningMap,
    refresh_slot: RefreshSlot,
    toast_overlay: ToastOverlay,
) {
    btn.connect_clicked(move |_| {
        let trainer_path = trainer_path.clone();
        let running = running.clone();
        let refresh_slot = refresh_slot.clone();
        let toast_overlay = toast_overlay.clone();
        applog::log(&format!("UI: Stop clicked for pgid {pgid}"));

        spawn_async(
            async move { tokio::task::spawn_blocking(move || launcher::stop_trainer(pgid)).await },
            move |result| {
                if let Err(e) = result {
                    toast_overlay.add_toast(Toast::new(&format!("Stop task error: {e}")));
                } else {
                    toast_overlay.add_toast(Toast::new("Trainer stopped"));
                }
                running.borrow_mut().remove(&trainer_path);
                if let Some(refresh) = refresh_slot.borrow().clone() {
                    refresh();
                }
            },
        );
    });
}

// ─────────────────────────────────────────────────────────────────────────────
//  .NET one-time setup dialog
// ─────────────────────────────────────────────────────────────────────────────

/// Exact manual commands, shown only inside the collapsed disclosure — the
/// default path is the "Set Up Automatically" button, which runs the same
/// commands itself via setup::run_system_setup / setup::install_dotnet48.
fn manual_commands(target: &LaunchTarget) -> String {
    let winetricks = format!(
        "WINEPREFIX={} winetricks -q dotnet48 win10",
        target.prefix_dir().display()
    );
    if setup::apt_available() {
        format!(
            "dpkg --add-architecture i386 && apt update && apt install -y winetricks cabextract wine32:i386\n{winetricks}"
        )
    } else {
        format!(
            "# Install winetricks, cabextract and 32-bit wine support with your\n\
# distribution's package manager, then:\n{winetricks}"
        )
    }
}

fn show_dotnet_dialog(
    window: &ApplicationWindow,
    target: LaunchTarget,
    trainer: Trainer,
    toast_overlay: ToastOverlay,
    running: RunningMap,
    refresh_slot: RefreshSlot,
    guard: Rc<PendingLaunch>,
) {
    // The privileged setup script is apt-only; elsewhere offer just the
    // manual command list rather than a password prompt that then fails.
    let automatic = setup::apt_available();
    let body = if automatic {
        "This game needs a one-time Windows compatibility component before trainers \
will run. This will ask for your password once, then take a minute or two."
    } else {
        "This game needs a one-time Windows compatibility component before trainers \
will run. Automatic setup is only available on Debian/Ubuntu-based systems; run the \
commands below yourself, then launch the trainer again."
    };

    let commands_label = Label::new(Some(&manual_commands(&target)));
    commands_label.set_wrap(true);
    commands_label.set_xalign(0.0);
    commands_label.set_selectable(true);
    commands_label.set_margin_top(6);
    commands_label.set_margin_bottom(6);
    commands_label.set_margin_start(12);
    commands_label.set_margin_end(12);

    let expander = ExpanderRow::builder()
        .title(if automatic { "Show manual commands instead" } else { "Manual commands" })
        .expanded(!automatic)
        .build();
    expander.add_row(&commands_label);

    let disclosure_list = ListBox::new();
    disclosure_list.set_selection_mode(SelectionMode::None);
    disclosure_list.add_css_class("boxed-list");
    disclosure_list.append(&expander);

    let dialog = AlertDialog::builder()
        .heading("One-Time Setup Needed")
        .body(body)
        .extra_child(&disclosure_list)
        .build();
    if automatic {
        dialog.add_responses(&[("cancel", "Cancel"), ("setup", "Set Up Automatically")]);
        dialog.set_response_appearance("setup", ResponseAppearance::Suggested);
        dialog.set_default_response(Some("setup"));
    } else {
        dialog.add_response("cancel", "Close");
    }
    dialog.set_close_response("cancel");

    // AlertDialog::connect_response requires Fn, not FnOnce, so the
    // non-Clone target/trainer are handed off through a RefCell rather than
    // moved directly — the dialog only ever fires one response in practice.
    let state: Rc<RefCell<Option<(LaunchTarget, Trainer)>>> =
        Rc::new(RefCell::new(Some((target, trainer))));
    // Held by the response handler so the pending marker lasts as long as the
    // dialog (and the setup it triggers, which takes the guard along).
    let guard_slot: Rc<RefCell<Option<Rc<PendingLaunch>>>> = Rc::new(RefCell::new(Some(guard)));

    dialog.connect_response(None, move |_dialog, response| {
        if response != "setup" {
            return;
        }
        let Some((target, trainer)) = state.borrow_mut().take() else {
            return;
        };
        let Some(guard) = guard_slot.borrow_mut().take() else {
            return;
        };
        run_automatic_setup(target, trainer, toast_overlay.clone(), running.clone(), refresh_slot.clone(), guard);
    });

    dialog.present(Some(window));
}

/// Runs the two-phase setup (privileged system packages, then user-level
/// winetricks install) off the main thread, then auto-launches the trainer
/// on success. Errors from either phase surface via a toast with the real
/// error message; the setup script and install_dotnet48 both also log to
/// /var/log/steampunk.log.
fn run_automatic_setup(
    target: LaunchTarget,
    trainer: Trainer,
    toast_overlay: ToastOverlay,
    running: RunningMap,
    refresh_slot: RefreshSlot,
    guard: Rc<PendingLaunch>,
) {
    toast_overlay.add_toast(Toast::new("Setting up .NET — this may take a minute or two…"));

    spawn_async(
        async move {
            tokio::task::spawn_blocking(move || -> anyhow::Result<LaunchTarget> {
                // Cloning from a prefix that already works is tried first, and
                // the Microsoft installer only as a fallback: on Wine's new
                // wow64 mode that installer fails outright and its rollback
                // strips .NET back out, leaving the prefix worse than it
                // started. Cloning needs no system packages either, so the
                // pkexec prompt is only reached when there's nothing to clone.
                if !launcher::repair_dotnet_from_sibling_prefix(&target)? {
                    if !setup::system_prereqs_present() {
                        setup::run_system_setup()?;
                    }
                    setup::install_dotnet48(&target.prefix_dir())?;
                }

                if !launcher::has_usable_dotnet(&target) {
                    anyhow::bail!(
                        "Couldn't get a working .NET runtime into this game's prefix. No other \
                         game's Proton prefix on this system had one to copy, and the installer \
                         didn't complete. See the debug log (Save Debug Log) for the details."
                    );
                }

                Ok(target)
            })
            .await
        },
        move |result| {
            let target = match result {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => {
                    toast_overlay.add_toast(Toast::new(&format!("Setup failed: {e}")));
                    return;
                }
                Err(e) => {
                    toast_overlay.add_toast(Toast::new(&format!("Task error: {e}")));
                    return;
                }
            };
            launch_trainer_now(target, trainer, toast_overlay, running, refresh_slot, guard);
        },
    );
}

// ─────────────────────────────────────────────────────────────────────────────
//  Stale-instance recovery dialog
// ─────────────────────────────────────────────────────────────────────────────

fn show_troubleshoot(window: &ApplicationWindow, toast_overlay: &ToastOverlay) {
    let window = window.clone();
    let toast_overlay = toast_overlay.clone();

    let picker_window = window.clone();
    let picker_toasts = toast_overlay.clone();
    with_launch_target(
        &picker_window,
        &picker_toasts,
        Rc::new(move |target| {
            let window = window.clone();
            let toast_overlay = toast_overlay.clone();

            let dirs = launcher::trainer_log_dirs(&target);
            if dirs.is_empty() {
                toast_overlay.add_toast(Toast::new("No trainer logs found for the running game"));
                return;
            }

            let dialog = AdwDialog::builder()
                .title("Clear Stale Trainer Instance")
                .content_width(420)
                .content_height(320)
                .build();

            let outer = GtkBox::new(Orientation::Vertical, 8);
            outer.set_margin_top(12);
            outer.set_margin_bottom(12);
            outer.set_margin_start(12);
            outer.set_margin_end(12);

            let hint = Label::new(Some(
                "If a trainer refuses to start, claiming a previous instance is running, \
                 clear its log folder below, then relaunch.",
            ));
            hint.set_wrap(true);
            hint.set_xalign(0.0);
            outer.append(&hint);

            let list = ListBox::new();
            list.set_selection_mode(SelectionMode::None);
            list.add_css_class("boxed-list");
            outer.append(&list);

            for dir in dirs {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let row = ActionRow::builder().title(esc(&name)).build();

                let clear_btn = Button::builder()
                    .label("Clear")
                    .valign(Align::Center)
                    .build();
                clear_btn.add_css_class("destructive-action");

                let toast_overlay2 = toast_overlay.clone();
                let dir2 = dir.clone();
                clear_btn.connect_clicked(move |_| {
                    let result = std::fs::remove_file(dir2.join("info.ini"));
                    applog::log(&format!(
                        "UI: cleared stale instance {} -> {result:?}",
                        dir2.display()
                    ));
                    match result {
                        Ok(()) => toast_overlay2.add_toast(Toast::new("Cleared")),
                        Err(e) => {
                            toast_overlay2.add_toast(Toast::new(&format!("Failed: {e}")))
                        }
                    }
                });
                row.add_suffix(&clear_btn);
                list.append(&row);
            }

            let scroll = ScrolledWindow::builder().vexpand(true).child(&outer).build();
            dialog.set_child(Some(&scroll));
            dialog.present(Some(&window));
        }),
    );
}
