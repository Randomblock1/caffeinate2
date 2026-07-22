//! The "Wait for apps…" picker: a modeless window for choosing which programs a
//! session waits on. Multi-select with checkboxes, app icons, a search filter, a
//! "show all programs" / "show system apps" toggle, and helper PIDs grouped
//! under their parent `.app`.
//!
//! The window is driven by the existing hand-pumped run loop in
//! [`crate::tray::app`] — never a nested modal loop — so the tray countdown and
//! the upgrade watcher keep running while it is open. The selection is delivered
//! back over an `mpsc` channel and applied at the top of the run loop (not
//! synchronously inside a button action, which would re-enter `AppState`).

use crate::tray::app_target::WatchTarget;
use crate::tray::macos_apps;
use crate::tray::process_enum::{self, BundleRef, ProgramRow};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn,
    NSControlTextEditingDelegate, NSImage, NSImageView, NSModalResponse, NSModalResponseOK,
    NSOpenPanel, NSRunningApplication, NSScrollView, NSSearchField, NSTableColumn, NSTableView,
    NSTableViewDataSource, NSTableViewDelegate, NSTextField, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSInteger, NSNotification, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString,
};
use objc2_uniform_type_identifiers::UTTypeApplicationBundle;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

const WIN_W: f64 = 440.0;
const WIN_H: f64 = 520.0;
const MARGIN: f64 = 12.0;
const ROW_HEIGHT: f64 = 24.0;
const INNER_W: f64 = WIN_W - 2.0 * MARGIN;
const COL_WIDTH: f64 = INNER_W - 4.0;
pub(crate) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// Coalesce a burst of `NSWorkspace` launch/quit notifications into a single
/// re-scan once activity settles, rather than scanning per notification.
const WORKSPACE_REFRESH_DEBOUNCE: Duration = Duration::from_millis(200);
const MAX_ICON_CACHE: usize = 64;

/// What the picker reports back to the run loop when it closes.
pub enum WaitWindowMsg {
    /// The user pressed Apply; carries the full desired selection.
    Apply(Vec<WatchTarget>),
    /// Dismissed without applying (Cancel, red button, or Esc).
    Cancel,
}

/// One rendered line: a selectable program, an informational helper child, or a
/// non-interactive notice (e.g. the process scan failed). A `Notice` carries no
/// program index and renders no checkbox, so it can never be checked or persist
/// as a [`WatchTarget`].
#[derive(Clone)]
enum DisplayRow {
    Program { all_index: usize },
    Child { name: String, pid: i32 },
    Notice { text: String },
}

struct Ivars {
    /// Every program from the open-time scan, plus any not-running but
    /// previously-selected targets. Indexed by `DisplayRow::Program`.
    all: RefCell<Vec<ProgramRow>>,
    /// Flat display list (programs + helper children) after filtering.
    rows: RefCell<Vec<DisplayRow>>,
    /// Currently-checked targets, keyed by `WatchTarget::key`.
    checked: RefCell<HashMap<String, WatchTarget>>,
    /// Icon cache keyed by file path (`iconForFile` does disk I/O).
    icons: RefCell<HashMap<String, Retained<NSImage>>>,
    apps_only: Cell<bool>,
    show_system: Cell<bool>,
    query: RefCell<String>,
    search_pending: Cell<bool>,
    last_search_change: RefCell<Option<Instant>>,
    /// A workspace launch/quit refresh awaiting its debounce (see
    /// [`WORKSPACE_REFRESH_DEBOUNCE`]), coalescing a notification burst into one
    /// re-scan.
    refresh_pending: Cell<bool>,
    last_refresh_request: RefCell<Option<Instant>>,
    /// Set once Apply/Cancel/close has reported a result, so the window-close
    /// handler doesn't send a second message.
    decided: Cell<bool>,
    /// Set when the most recent process scan failed and left us without a good
    /// list to show, so [`WaitController::rebuild`] renders a retry notice
    /// instead of an empty picker.
    scan_failed: Cell<bool>,
    /// The app that was frontmost before the picker stole focus, so closing
    /// can hand activation back instead of leaving focus in limbo.
    previous_app: RefCell<Option<Retained<NSRunningApplication>>>,
    tx: Sender<WaitWindowMsg>,
    table: RefCell<Option<Retained<NSTableView>>>,
    window: RefCell<Option<Retained<NSWindow>>>,
    mtm: MainThreadMarker,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "Caffeinate2WaitController"]
    #[ivars = Ivars]
    struct WaitController;

    unsafe impl NSObjectProtocol for WaitController {}

    // NSTableViewDelegate inherits NSControlTextEditingDelegate; conform to it
    // (marker only — the search field uses target/action, not this delegate).
    unsafe impl NSControlTextEditingDelegate for WaitController {}

    impl WaitController {
        #[unsafe(method(searchChanged:))]
        fn search_changed(&self, sender: &NSSearchField) {
            *self.ivars().query.borrow_mut() = sender.stringValue().to_string();
            self.ivars().search_pending.set(true);
            *self.ivars().last_search_change.borrow_mut() = Some(Instant::now());
        }

        #[unsafe(method(showAllToggled:))]
        fn show_all_toggled(&self, sender: &NSButton) {
            self.ivars().apps_only.set(sender.state() != NSControlStateValueOn);
            self.rebuild();
        }

        #[unsafe(method(showSystemToggled:))]
        fn show_system_toggled(&self, sender: &NSButton) {
            self.ivars()
                .show_system
                .set(sender.state() == NSControlStateValueOn);
            self.rebuild();
        }

        #[unsafe(method(rowCheckboxToggled:))]
        fn row_checkbox_toggled(&self, sender: &NSButton) {
            let all_index = sender.tag().cast_unsigned();
            let entry = {
                let all = self.ivars().all.borrow();
                all.get(all_index)
                    .map(|prog| (prog.target.key().to_string(), prog.target.clone()))
            };
            let Some((key, target)) = entry else { return };
            let mut checked = self.ivars().checked.borrow_mut();
            if sender.state() == NSControlStateValueOn {
                checked.insert(key, target);
            } else {
                checked.remove(&key);
            }
        }

        #[unsafe(method(applyClicked:))]
        fn apply_clicked(&self, _sender: &NSButton) {
            if !self.ivars().decided.replace(true) {
                let mut targets: Vec<WatchTarget> =
                    self.ivars().checked.borrow().values().cloned().collect();
                // The selection map has a randomized hash seed, so persist in a
                // stable order (by each target's unique key); otherwise the saved
                // list and the tooltip's "first 3" permute between identical edits.
                targets.sort_by(|a, b| a.key().cmp(b.key()));
                let _ = self.ivars().tx.send(WaitWindowMsg::Apply(targets));
            }
            self.close_window();
        }

        #[unsafe(method(cancelClicked:))]
        fn cancel_clicked(&self, _sender: &NSButton) {
            if !self.ivars().decided.replace(true) {
                let _ = self.ivars().tx.send(WaitWindowMsg::Cancel);
            }
            self.close_window();
        }

        #[unsafe(method(refreshClicked:))]
        fn refresh_clicked(&self, _sender: &NSButton) {
            self.refresh();
        }

        #[unsafe(method(chooseAppClicked:))]
        fn choose_app_clicked(&self, _sender: &NSButton) {
            self.choose_app();
        }
    }

    unsafe impl NSTableViewDataSource for WaitController {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.ivars().rows.borrow().len().cast_signed()
        }
    }

    unsafe impl NSTableViewDelegate for WaitController {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for_row(
            &self,
            _table: &NSTableView,
            _column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            let display = self.ivars().rows.borrow().get(row.cast_unsigned()).cloned();
            match display {
                Some(DisplayRow::Program { all_index }) => Some(self.program_cell(all_index)),
                Some(DisplayRow::Child { name, pid }) => Some(self.child_cell(&name, pid)),
                Some(DisplayRow::Notice { text }) => Some(self.notice_cell(&text)),
                None => None,
            }
        }
    }

    unsafe impl NSWindowDelegate for WaitController {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            // Any close path must report a result so the run loop drops its
            // handle, and must restore Accessory so no Dock icon lingers.
            if !self.ivars().decided.replace(true) {
                let _ = self.ivars().tx.send(WaitWindowMsg::Cancel);
            }
            let app = NSApplication::sharedApplication(self.ivars().mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            // Hand focus back explicitly: flipping Regular -> Accessory while
            // frontmost otherwise strands keyboard focus until the user
            // clicks another app.
            if let Some(prev) = self.ivars().previous_app.borrow_mut().take()
                && !prev.isTerminated()
            {
                prev.activateWithOptions(NSApplicationActivationOptions::empty());
            }
            crate::tray::macos_activation::wake_event_loop();
        }
    }
);

impl WaitController {
    fn new(
        mtm: MainThreadMarker,
        all: Vec<ProgramRow>,
        checked: HashMap<String, WatchTarget>,
        tx: Sender<WaitWindowMsg>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            all: RefCell::new(all),
            rows: RefCell::new(Vec::new()),
            checked: RefCell::new(checked),
            icons: RefCell::new(HashMap::new()),
            apps_only: Cell::new(true),
            show_system: Cell::new(false),
            query: RefCell::new(String::new()),
            search_pending: Cell::new(false),
            last_search_change: RefCell::new(None),
            refresh_pending: Cell::new(false),
            last_refresh_request: RefCell::new(None),
            decided: Cell::new(false),
            scan_failed: Cell::new(false),
            previous_app: RefCell::new(None),
            tx,
            table: RefCell::new(None),
            window: RefCell::new(None),
            mtm,
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Re-derive the flat display list from the toggles + search and reload.
    /// A checked program is always shown (even if filtered out) so it can be
    /// unchecked.
    fn rebuild(&self) {
        let query = self.ivars().query.borrow().to_lowercase();
        let apps_only = self.ivars().apps_only.get();
        let show_system = self.ivars().show_system.get();
        let all = self.ivars().all.borrow();
        let checked = self.ivars().checked.borrow();
        let mut rows = Vec::new();
        if self.ivars().scan_failed.get() {
            rows.push(DisplayRow::Notice {
                text: "Couldn't read the running programs — press Refresh to try again."
                    .to_string(),
            });
        }
        for (idx, prog) in all.iter().enumerate() {
            if !checked.contains_key(prog.target.key()) {
                if apps_only && prog.bundle.is_none() {
                    continue;
                }
                if !show_system && prog.is_system {
                    continue;
                }
                if !query.is_empty()
                    && !prog.name.to_lowercase().contains(&query)
                    && !prog
                        .procs
                        .iter()
                        .any(|c| c.name.to_lowercase().contains(&query))
                {
                    continue;
                }
            }
            rows.push(DisplayRow::Program { all_index: idx });
            // Show helper processes beneath multi-process programs so the
            // grouping is visible (e.g. an Electron app's GPU renderer).
            if prog.procs.len() > 1 {
                for child in &prog.procs {
                    rows.push(DisplayRow::Child {
                        name: child.name.clone(),
                        pid: child.pid,
                    });
                }
            }
        }
        drop(all);
        drop(checked);
        *self.ivars().rows.borrow_mut() = rows;
        if let Some(table) = self.ivars().table.borrow().as_ref() {
            table.reloadData();
        }
    }

    /// Re-scan the process tree and reload, preserving the current selection,
    /// toggles, and search query. Runs immediately for the Refresh button
    /// (which also catches non-app/daemon changes the workspace observer
    /// misses) and for a coalesced workspace refresh once its debounce fires
    /// (see [`Self::schedule_refresh`]). Skipped once the window is closing so
    /// we don't rebuild a dying table.
    fn refresh(&self) {
        if self.ivars().decided.get() {
            return;
        }
        self.ivars().refresh_pending.set(false);
        let scanned = {
            let checked = self.ivars().checked.borrow();
            scan_rows(&checked)
        };
        match scanned {
            Ok(all) => {
                self.ivars().scan_failed.set(false);
                *self.ivars().all.borrow_mut() = all;
                self.rebuild();
            }
            Err(err) => {
                // Don't clobber a good list with an error result: if a prior scan
                // succeeded, keep those rows so a transient failure doesn't blank
                // the picker. Only when we have nothing good to show do we mark
                // the scan failed and let rebuild render the retry notice.
                let have_good_list =
                    !self.ivars().scan_failed.get() && !self.ivars().all.borrow().is_empty();
                if have_good_list {
                    tracing::warn!(error = %err, "wait picker: refresh scan failed; keeping current list");
                    return;
                }
                tracing::warn!(error = %err, "wait picker: refresh scan failed; showing retry notice");
                self.ivars().scan_failed.set(true);
                let mut all = Vec::new();
                append_checked_rows(&mut all, &self.ivars().checked.borrow());
                *self.ivars().all.borrow_mut() = all;
                self.rebuild();
            }
        }
    }

    /// Coalesce a workspace launch/quit notification into a single deferred
    /// re-scan (see [`WORKSPACE_REFRESH_DEBOUNCE`]); `poll_debounce` runs the
    /// scan once the burst settles. Scanning the whole process tree per
    /// notification would hammer the main thread during app-launch storms.
    fn schedule_refresh(&self) {
        if self.ivars().decided.get() {
            return;
        }
        self.ivars().refresh_pending.set(true);
        *self.ivars().last_refresh_request.borrow_mut() = Some(Instant::now());
    }

    fn apply_chosen_app_path(&self, path: String) {
        let Some(app) = macos_apps::bundle_from_app_path(&path) else {
            return;
        };

        let target = WatchTarget::from_app_target(app);
        let key = target.key().to_string();
        self.ivars()
            .checked
            .borrow_mut()
            .insert(key, target.clone());

        let mut all = self.ivars().all.borrow_mut();
        if !all.iter().any(|row| row.target.key() == target.key()) {
            all.push(not_running_row(target, path));
        }
        drop(all);
        self.rebuild();
    }

    fn choose_app(&self) {
        let panel = NSOpenPanel::openPanel(self.ivars().mtm);
        panel.setCanChooseFiles(false);
        panel.setCanChooseDirectories(true);
        panel.setAllowsMultipleSelection(false);
        panel.setResolvesAliases(true);
        panel.setTreatsFilePackagesAsDirectories(false);
        panel.setTitle(Some(&NSString::from_str("Choose application")));
        panel.setPrompt(Some(&NSString::from_str("Choose")));
        let app_types = unsafe { NSArray::arrayWithObject(UTTypeApplicationBundle) };
        panel.setAllowedContentTypes(&app_types);

        let Some(window) = self.ivars().window.borrow().clone() else {
            return;
        };
        let controller = self.retain();
        let panel_for_handler = panel.clone();
        let handler = RcBlock::new(move |response: NSModalResponse| {
            if response != NSModalResponseOK {
                return;
            }
            let urls = panel_for_handler.URLs();
            if urls.count() == 0 {
                return;
            }
            let Some(path) = urls.objectAtIndex(0).path().map(|path| path.to_string()) else {
                return;
            };
            controller.apply_chosen_app_path(path);
            crate::tray::macos_activation::wake_event_loop();
        });
        panel.beginSheetModalForWindow_completionHandler(&window, &handler);
    }

    fn poll_debounce(&self) {
        self.poll_search_debounce();
        self.poll_refresh_debounce();
    }

    fn poll_search_debounce(&self) {
        if !self.ivars().search_pending.get() {
            return;
        }
        let Some(start) = *self.ivars().last_search_change.borrow() else {
            return;
        };
        if start.elapsed() >= SEARCH_DEBOUNCE {
            self.ivars().search_pending.set(false);
            self.rebuild();
        }
    }

    fn poll_refresh_debounce(&self) {
        if !self.ivars().refresh_pending.get() {
            return;
        }
        let Some(start) = *self.ivars().last_refresh_request.borrow() else {
            return;
        };
        if start.elapsed() >= WORKSPACE_REFRESH_DEBOUNCE {
            self.refresh();
        }
    }

    fn close_window(&self) {
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.close();
        }
    }

    /// Record the app that is about to lose focus to the picker, unless it is
    /// us (re-raising an already-frontmost picker must not clobber the real
    /// hand-back target).
    fn remember_frontmost(&self) {
        let Some(front) = NSWorkspace::sharedWorkspace().frontmostApplication() else {
            return;
        };
        if front.processIdentifier() == std::process::id().cast_signed() {
            return;
        }
        *self.ivars().previous_app.borrow_mut() = Some(front);
    }

    /// Cached app/program icon at menu-bar size.
    fn icon_for(&self, path: &str) -> Option<Retained<NSImage>> {
        if path.is_empty() {
            return None;
        }
        if let Some(image) = self.ivars().icons.borrow().get(path) {
            return Some(image.clone());
        }
        let image = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(path));
        image.setSize(NSSize::new(16.0, 16.0));
        let mut icons = self.ivars().icons.borrow_mut();
        if icons.len() >= MAX_ICON_CACHE
            && !icons.contains_key(path)
            && let Some(stale) = icons.keys().next().cloned()
        {
            icons.remove(&stale);
        }
        icons.insert(path.to_string(), image.clone());
        Some(image)
    }

    fn program_cell(&self, all_index: usize) -> Retained<NSView> {
        let mtm = self.ivars().mtm;
        let (title, icon_path, checked) = {
            let all = self.ivars().all.borrow();
            let Some(prog) = all.get(all_index) else {
                return self.child_cell("Program list changed; refresh to reload", 0);
            };
            let title = if prog.procs.len() > 1 {
                format!("{}  ({} processes)", prog.name, prog.procs.len())
            } else {
                prog.name.clone()
            };
            let checked = self
                .ivars()
                .checked
                .borrow()
                .contains_key(prog.target.key());
            (title, prog.icon_path.clone(), checked)
        };

        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(COL_WIDTH, ROW_HEIGHT)),
        );
        if let Some(image) = self.icon_for(&icon_path) {
            let image_view = NSImageView::imageViewWithImage(&image, mtm);
            image_view.setFrame(NSRect::new(NSPoint::new(4.0, 4.0), NSSize::new(16.0, 16.0)));
            // Purely decorative: the adjacent checkbox carries the program
            // name, so an unlabeled AXImage stop before every row would only
            // slow VoiceOver users down.
            image_view.setAccessibilityElement(false);
            container.addSubview(&image_view);
        }
        let target: &AnyObject = self;
        let checkbox = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &NSString::from_str(&title),
                Some(target),
                Some(sel!(rowCheckboxToggled:)),
                mtm,
            )
        };
        checkbox.setFrame(NSRect::new(
            NSPoint::new(26.0, 1.0),
            NSSize::new(COL_WIDTH - 30.0, 22.0),
        ));
        checkbox.setTag(all_index.cast_signed());
        checkbox.setState(if checked {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        container.addSubview(&checkbox);
        container
    }

    fn child_cell(&self, name: &str, pid: i32) -> Retained<NSView> {
        let mtm = self.ivars().mtm;
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(COL_WIDTH, ROW_HEIGHT)),
        );
        let label =
            NSTextField::labelWithString(&NSString::from_str(&format!("{name} — pid {pid}")), mtm);
        label.setFrame(NSRect::new(
            NSPoint::new(46.0, 3.0),
            NSSize::new(COL_WIDTH - 50.0, 18.0),
        ));
        label.setTextColor(Some(&NSColor::secondaryLabelColor()));
        container.addSubview(&label);
        container
    }

    /// A non-interactive informational row (no checkbox), used for the
    /// process-scan-failed notice. Carries no `WatchTarget`, so it can never be
    /// checked or persisted.
    fn notice_cell(&self, text: &str) -> Retained<NSView> {
        let mtm = self.ivars().mtm;
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(COL_WIDTH, ROW_HEIGHT)),
        );
        let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        label.setFrame(NSRect::new(
            NSPoint::new(8.0, 3.0),
            NSSize::new(COL_WIDTH - 12.0, 18.0),
        ));
        label.setTextColor(Some(&NSColor::secondaryLabelColor()));
        container.addSubview(&label);
        container
    }
}

/// Live handle the run loop keeps so the window/controller stay alive. Dropping
/// it releases the controller (and the window it owns).
pub struct WaitWindow {
    controller: Retained<WaitController>,
}

impl WaitWindow {
    /// Schedule a coalesced re-scan of running programs, keeping the current
    /// selection, toggles, and search query. Called by the run loop on every
    /// `NSWorkspace` launch/quit while the picker is open; a burst of
    /// notifications collapses into a single scan once activity settles (see
    /// [`WaitController::schedule_refresh`]). `poll_debounce` performs the scan.
    pub fn refresh(&self) {
        self.controller.schedule_refresh();
    }

    /// Run any due debounced work: apply the search filter once the user pauses
    /// typing, and perform a coalesced workspace refresh once launch/quit
    /// activity settles.
    pub fn poll_debounce(&self) {
        self.controller.poll_debounce();
    }

    /// Whether any debounced picker work — a typed search change or a coalesced
    /// workspace refresh — is still waiting for its debounce to elapse. The run
    /// loop uses this to keep cycling (rather than blocking indefinitely) so
    /// `poll_debounce` actually fires once activity settles.
    pub fn search_pending(&self) -> bool {
        let ivars = self.controller.ivars();
        ivars.search_pending.get() || ivars.refresh_pending.get()
    }

    /// Bring an already-open picker back to the front instead of opening a second copy.
    pub fn bring_to_front(&self) {
        // Window operations after a decision (Apply/Cancel/close) are no-ops.
        if self.controller.ivars().decided.get() {
            return;
        }
        let mtm = self.controller.ivars().mtm;
        if let Some(window) = self.controller.ivars().window.borrow().as_ref() {
            self.controller.remember_frontmost();
            activate_and_order_front(mtm, window);
        }
    }
}

const fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// Flip to Regular and force the picker to the foreground. On macOS 14+
/// `NSApplication::activate` is cooperative — honored only if the frontmost
/// app yields — so relying on it leaves the window behind the active app.
/// The window is ordered front before activating so activation has an
/// on-screen window to focus; `orderFrontRegardless` then puts it visually
/// frontmost even when activation is deferred. The forceful legacy activation
/// grabs key focus on pre-Sonoma systems and degrades to plain `activate` on
/// 14+.
fn activate_and_order_front(mtm: MainThreadMarker, window: &NSWindow) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    window.orderFrontRegardless();
}

/// Scan the full process tree for the picker's model, or return the enumeration
/// error so the caller can surface it instead of showing an empty list. Any
/// checked target that is not currently running is re-appended as a
/// "(not running)" row so a live selection is never silently dropped when its
/// program quits while the window is open. Used both at open time and on every
/// [`WaitWindow::refresh`].
fn scan_rows(checked: &HashMap<String, WatchTarget>) -> std::io::Result<Vec<ProgramRow>> {
    let mut all = process_enum::program_rows(false, true)?;
    append_checked_rows(&mut all, checked);
    Ok(all)
}

/// Append a "(not running)" row for every checked target not already present in
/// `all`, so the persisted selection is always shown even when its program isn't
/// running (or the live scan failed and `all` holds only these rows).
fn append_checked_rows(all: &mut Vec<ProgramRow>, checked: &HashMap<String, WatchTarget>) {
    let present: HashSet<String> = all.iter().map(|p| p.target.key().to_string()).collect();
    for (key, target) in checked {
        if !present.contains(key) {
            all.push(not_running_row(target.clone(), String::new()));
        }
    }
}

fn not_running_row(target: WatchTarget, icon_path: String) -> ProgramRow {
    let bundle = match &target {
        WatchTarget::Bundle { bundle_id, name } => Some(BundleRef {
            bundle_id: bundle_id.clone(),
            name: name.clone(),
            app_path: icon_path.clone(),
        }),
        WatchTarget::Executable { .. } => None,
    };
    ProgramRow {
        name: format!("{} (not running)", target.name()),
        bundle,
        is_system: false,
        icon_path,
        target,
        procs: Vec::new(),
    }
}

fn install_wait_window_header(mtm: MainThreadMarker, content: &NSView, target: &AnyObject) -> f64 {
    let search = NSSearchField::new(mtm);
    search.setFrame(rect(MARGIN, WIN_H - MARGIN - 24.0, INNER_W, 24.0));
    unsafe {
        search.setTarget(Some(target));
        search.setAction(Some(sel!(searchChanged:)));
    }
    search.setSendsWholeSearchString(false);
    search.setSendsSearchStringImmediately(true);
    content.addSubview(&search);

    let toggles_y = WIN_H - MARGIN - 24.0 - 8.0 - 22.0;
    let show_all = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Show all programs"),
            Some(target),
            Some(sel!(showAllToggled:)),
            mtm,
        )
    };
    show_all.setFrame(rect(MARGIN, toggles_y, 200.0, 22.0));
    content.addSubview(&show_all);
    let show_system = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Show system apps"),
            Some(target),
            Some(sel!(showSystemToggled:)),
            mtm,
        )
    };
    show_system.setFrame(rect(MARGIN + 210.0, toggles_y, 200.0, 22.0));
    content.addSubview(&show_system);

    let apply = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Apply"),
            Some(target),
            Some(sel!(applyClicked:)),
            mtm,
        )
    };
    apply.setFrame(rect(WIN_W - MARGIN - 90.0, MARGIN, 90.0, 30.0));
    // Return triggers Apply (and renders it as the default button), matching
    // Esc → Cancel below.
    apply.setKeyEquivalent(&NSString::from_str("\r"));
    content.addSubview(&apply);
    let cancel = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Cancel"),
            Some(target),
            Some(sel!(cancelClicked:)),
            mtm,
        )
    };
    cancel.setFrame(rect(WIN_W - MARGIN - 90.0 - 8.0 - 90.0, MARGIN, 90.0, 30.0));
    // Esc triggers Cancel: the Escape character is the button's key equivalent.
    cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
    content.addSubview(&cancel);
    let refresh = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Refresh"),
            Some(target),
            Some(sel!(refreshClicked:)),
            mtm,
        )
    };
    refresh.setFrame(rect(MARGIN, MARGIN, 90.0, 30.0));
    content.addSubview(&refresh);
    let choose_app = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Choose app…"),
            Some(target),
            Some(sel!(chooseAppClicked:)),
            mtm,
        )
    };
    choose_app.setFrame(rect(MARGIN + 98.0, MARGIN, 110.0, 30.0));
    content.addSubview(&choose_app);
    toggles_y
}

fn install_wait_window_table(
    mtm: MainThreadMarker,
    content: &NSView,
    controller: &WaitController,
    toggles_y: f64,
) -> Retained<NSTableView> {
    let scroll_top = toggles_y - 8.0;
    let scroll_bottom = MARGIN + 30.0 + 8.0;
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        rect(MARGIN, scroll_bottom, INNER_W, scroll_top - scroll_bottom),
    );
    scroll.setHasVerticalScroller(true);

    let table = NSTableView::initWithFrame(
        NSTableView::alloc(mtm),
        rect(0.0, 0.0, INNER_W, scroll_top - scroll_bottom),
    );
    let column = NSTableColumn::initWithIdentifier(
        NSTableColumn::alloc(mtm),
        &NSString::from_str("program"),
    );
    column.setWidth(COL_WIDTH);
    table.addTableColumn(&column);
    table.setHeaderView(None);
    table.setRowHeight(ROW_HEIGHT);
    unsafe {
        table.setDataSource(Some(ProtocolObject::from_ref(controller)));
        table.setDelegate(Some(ProtocolObject::from_ref(controller)));
    }
    scroll.setDocumentView(Some(&table));
    content.addSubview(&scroll);
    table
}

/// Open the picker window pre-checked with `selected`, delivering the result
/// over `tx`. The caller must keep the returned [`WaitWindow`] alive for the
/// window's lifetime.
pub fn open(
    mtm: MainThreadMarker,
    selected: &[WatchTarget],
    tx: Sender<WaitWindowMsg>,
) -> WaitWindow {
    let checked: HashMap<String, WatchTarget> = selected
        .iter()
        .map(|t| (t.key().to_string(), t.clone()))
        .collect();
    // One process-tree scan; the toggles/search filter it in memory afterwards.
    // Re-scanned on workspace launch/quit while open (see `WaitWindow::refresh`).
    // On a transient enumeration failure, still surface the persisted selection
    // and flag the scan as failed so the picker shows a retry notice rather than
    // an empty list.
    let (all, scan_failed) = match scan_rows(&checked) {
        Ok(all) => (all, false),
        Err(err) => {
            tracing::warn!(error = %err, "wait picker: initial process scan failed");
            let mut all = Vec::new();
            append_checked_rows(&mut all, &checked);
            (all, true)
        }
    };

    let controller = WaitController::new(mtm, all, checked, tx);
    controller.ivars().scan_failed.set(scan_failed);
    let target: &AnyObject = &controller;

    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, WIN_W, WIN_H),
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("Wait for apps"));
    unsafe { window.setReleasedWhenClosed(false) };
    // Follow the user to the active Space; without this the picker opens on
    // a different Space — invisible — when the frontmost app is full-screen.
    window.setCollectionBehavior(NSWindowCollectionBehavior::MoveToActiveSpace);
    let content = window.contentView().expect("window has a content view");

    let toggles_y = install_wait_window_header(mtm, &content, target);
    let table = install_wait_window_table(mtm, &content, &controller, toggles_y);

    window.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    *controller.ivars().table.borrow_mut() = Some(table);
    *controller.ivars().window.borrow_mut() = Some(window.clone());

    controller.rebuild();

    // An Accessory app can't focus a window; flip to Regular to show it, then
    // restore Accessory in windowWillClose:.
    controller.remember_frontmost();
    window.center();
    activate_and_order_front(mtm, &window);

    WaitWindow { controller }
}
