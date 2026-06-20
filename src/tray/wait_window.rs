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

use crate::app_target::WatchTarget;
use crate::macos_apps;
use crate::process_enum::{self, BundleRef, ProgramRow};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSControlTextEditingDelegate, NSImage,
    NSImageView, NSModalResponseOK, NSOpenPanel, NSScrollView, NSSearchField, NSTableColumn,
    NSTableView, NSTableViewDataSource, NSTableViewDelegate, NSTextField, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSInteger, NSNotification, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString,
};
use objc2_uniform_type_identifiers::UTTypeApplicationBundle;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;

const WIN_W: f64 = 440.0;
const WIN_H: f64 = 520.0;
const MARGIN: f64 = 12.0;
const ROW_HEIGHT: f64 = 24.0;
const INNER_W: f64 = WIN_W - 2.0 * MARGIN;
const COL_WIDTH: f64 = INNER_W - 4.0;

/// What the picker reports back to the run loop when it closes.
pub enum WaitWindowMsg {
    /// The user pressed Apply; carries the full desired selection.
    Apply(Vec<WatchTarget>),
    /// Dismissed without applying (Cancel, red button, or Esc).
    Cancel,
}

/// One rendered line: a selectable program, or an informational helper child.
#[derive(Clone)]
enum DisplayRow {
    Program { all_index: usize },
    Child { name: String, pid: i32 },
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
    /// Set once Apply/Cancel/close has reported a result, so the window-close
    /// handler doesn't send a second message.
    decided: Cell<bool>,
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
            self.rebuild();
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
            let all_index = sender.tag() as usize;
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
                let targets = self.ivars().checked.borrow().values().cloned().collect();
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
            self.ivars().rows.borrow().len() as NSInteger
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
            let display = self.ivars().rows.borrow().get(row as usize).cloned();
            match display {
                Some(DisplayRow::Program { all_index }) => Some(self.program_cell(all_index)),
                Some(DisplayRow::Child { name, pid }) => Some(self.child_cell(&name, pid)),
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
            crate::macos_activation::wake_event_loop();
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
            decided: Cell::new(false),
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
    /// toggles, and search query. Driven by the run loop on NSWorkspace
    /// launch/quit so the list stays live, and by the Refresh button (which
    /// also catches non-app/daemon changes the workspace observer misses).
    /// Skipped once the window is closing so we don't rebuild a dying table.
    fn refresh(&self) {
        if self.ivars().decided.get() {
            return;
        }
        let all = {
            let checked = self.ivars().checked.borrow();
            scan_rows(&checked)
        };
        *self.ivars().all.borrow_mut() = all;
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

        if panel.runModal() != NSModalResponseOK {
            return;
        }
        let urls = panel.URLs();
        if urls.count() == 0 {
            return;
        }
        let Some(path) = urls.objectAtIndex(0).path().map(|path| path.to_string()) else {
            return;
        };
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

    fn close_window(&self) {
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.close();
        }
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
        self.ivars()
            .icons
            .borrow_mut()
            .insert(path.to_string(), image.clone());
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
        checkbox.setTag(all_index as NSInteger);
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
}

/// Live handle the run loop keeps so the window/controller stay alive. Dropping
/// it releases the controller (and the window it owns).
pub struct WaitWindow {
    controller: Retained<WaitController>,
}

impl WaitWindow {
    /// Re-scan running programs and reload the list, keeping the current
    /// selection, toggles, and search query. Called by the run loop whenever
    /// an app launches or quits while the picker is open.
    pub fn refresh(&self) {
        self.controller.refresh();
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// Scan the full process tree for the picker's model. Any checked target that
/// is not currently running is re-appended as a "(not running)" row so a live
/// selection is never silently dropped when its program quits while the window
/// is open. Used both at open time and on every [`WaitWindow::refresh`].
fn scan_rows(checked: &HashMap<String, WatchTarget>) -> Vec<ProgramRow> {
    let mut all = process_enum::program_rows(false, true);
    let present: HashSet<String> = all.iter().map(|p| p.target.key().to_string()).collect();
    for (key, target) in checked {
        if !present.contains(key) {
            all.push(not_running_row(target.clone(), String::new()));
        }
    }
    all
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
    let all = scan_rows(&checked);

    let controller = WaitController::new(mtm, all, checked, tx);
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
    let content = window.contentView().expect("window has a content view");

    // Header: search field + the two toggles.
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

    // Footer: Cancel + Apply, bottom-right.
    let apply = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Apply"),
            Some(target),
            Some(sel!(applyClicked:)),
            mtm,
        )
    };
    apply.setFrame(rect(WIN_W - MARGIN - 90.0, MARGIN, 90.0, 30.0));
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
    content.addSubview(&cancel);
    // Bottom-left: manually re-scan (picks up daemons/non-app changes that the
    // NSWorkspace launch/quit observer doesn't fire for) or add a dormant app.
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

    // Body: a scrolling table between header and footer.
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
        table.setDataSource(Some(ProtocolObject::from_ref(&*controller)));
        table.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    }
    scroll.setDocumentView(Some(&table));
    content.addSubview(&scroll);

    window.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    *controller.ivars().table.borrow_mut() = Some(table);
    *controller.ivars().window.borrow_mut() = Some(window.clone());

    controller.rebuild();

    // An Accessory app can't focus a window; flip to Regular to show it, then
    // restore Accessory in windowWillClose:.
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.activate();
    window.center();
    window.makeKeyAndOrderFront(None);

    WaitWindow { controller }
}
