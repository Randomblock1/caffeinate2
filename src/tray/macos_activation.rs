pub fn init_tray_app() {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::MainThreadMarker;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
}

pub fn wake_event_loop() {
    use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_core_foundation::CFRunLoop;
    use objc2_foundation::{MainThreadMarker, NSPoint};

    // `pump_event_loop` blocks in `nextEventMatchingMask:untilDate:…`, which
    // only returns when an actual event arrives or the date expires; a bare
    // run-loop wake-up is not an event and leaves it blocked. Post an empty
    // application-defined event to force it to return.
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        if let Some(event) =
            NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType::ApplicationDefined,
                NSPoint::new(0.0, 0.0),
                NSEventModifierFlags::empty(),
                0.0,
                0,
                None,
                0,
                0,
                0,
            )
        {
            app.postEvent_atStart(&event, true);
            return;
        }
    }

    // Off the main thread NSApplication is unavailable; nudging the main run
    // loop is the best we can do.
    if let Some(run_loop) = CFRunLoop::main() {
        run_loop.wake_up();
    }
}

/// The activation-policy promotion shared by every foreground surface (the
/// "Wait for apps…" picker, the upgrade dialog). Main-thread only, like the
/// policy it guards.
struct ForegroundState {
    holders: usize,
    /// The app to hand activation back to once the last holder drops.
    previous: Option<objc2::rc::Retained<objc2_app_kit::NSRunningApplication>>,
}

thread_local! {
    static FOREGROUND: std::cell::RefCell<ForegroundState> = const {
        std::cell::RefCell::new(ForegroundState {
            holders: 0,
            previous: None,
        })
    };
}

/// RAII promotion of this Accessory (menu-bar) app to a Regular foreground app
/// so a window or dialog can take key focus. Nesting-safe via a refcount: the
/// first holder records the app about to lose focus (never this process) and
/// flips the policy; the last drop flips it back, hands activation to the
/// recorded app, and wakes the event loop. A dialog opening over the picker
/// therefore neither demotes the app nor steals the picker's hand-back target.
pub(crate) struct ForegroundActivation {
    mtm: objc2_foundation::MainThreadMarker,
}

impl ForegroundActivation {
    pub(crate) fn enter(mtm: objc2_foundation::MainThreadMarker) -> Self {
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSWorkspace};

        FOREGROUND.with(|state| {
            let mut state = state.borrow_mut();
            if state.holders == 0 {
                // Record the hand-back target, skipping ourselves: promoting
                // while already frontmost must not clobber the real target.
                state.previous = NSWorkspace::sharedWorkspace()
                    .frontmostApplication()
                    .filter(|front| front.processIdentifier() != std::process::id().cast_signed());
                NSApplication::sharedApplication(mtm)
                    .setActivationPolicy(NSApplicationActivationPolicy::Regular);
            }
            state.holders += 1;
        });
        Self { mtm }
    }
}

impl Drop for ForegroundActivation {
    fn drop(&mut self) {
        use objc2_app_kit::{
            NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
        };

        FOREGROUND.with(|state| {
            let mut state = state.borrow_mut();
            state.holders -= 1;
            if state.holders > 0 {
                return;
            }
            NSApplication::sharedApplication(self.mtm)
                .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            // Hand focus back explicitly: flipping Regular -> Accessory while
            // frontmost otherwise strands keyboard focus until the user clicks
            // another app.
            if let Some(previous) = state.previous.take()
                && !previous.isTerminated()
            {
                previous.activateWithOptions(NSApplicationActivationOptions::empty());
            }
        });
        wake_event_loop();
    }
}

/// Forward tray/menu events to channels. `tray-icon` and `muda` only deliver
/// events through `set_event_handler`; `receiver()` is disabled once a handler is set.
#[must_use]
pub fn install_tray_event_handlers() -> (
    std::sync::mpsc::Receiver<tray_icon::TrayIconEvent>,
    std::sync::mpsc::Receiver<muda::MenuEvent>,
) {
    use muda::MenuEvent;
    use std::sync::mpsc;
    use tray_icon::TrayIconEvent;

    let (tray_tx, tray_rx) = mpsc::channel();
    let (menu_tx, menu_rx) = mpsc::channel();

    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tray_tx.send(event);
        wake_event_loop();
    }));
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_tx.send(event);
        wake_event_loop();
    }));

    (tray_rx, menu_rx)
}

/// Keeps `NSWorkspace` launch/terminate observers registered while it lives,
/// and unregisters them on drop. Today the tray holds one for the whole
/// process, so the drop runs only at exit — the impl exists so the type keeps
/// the promise its name makes, and a future shorter-lived guard doesn't leave
/// stacked callbacks registered forever.
pub struct WorkspaceObserverGuard {
    launch:
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2::runtime::NSObjectProtocol>>,
    terminate:
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2::runtime::NSObjectProtocol>>,
    _launch_block: block2::RcBlock<dyn Fn(std::ptr::NonNull<objc2_foundation::NSNotification>)>,
    _terminate_block: block2::RcBlock<dyn Fn(std::ptr::NonNull<objc2_foundation::NSNotification>)>,
}

impl Drop for WorkspaceObserverGuard {
    fn drop(&mut self) {
        use objc2_app_kit::NSWorkspace;
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        // msg_send: `removeObserver:` takes a plain object, and the block
        // observer is typed as a protocol object rather than AnyObject.
        unsafe {
            let _: () = objc2::msg_send![&center, removeObserver: &*self.launch];
            let _: () = objc2::msg_send![&center, removeObserver: &*self.terminate];
        }
    }
}

/// Register observers for app launch/quit. The returned flag is set (and the
/// event loop woken) whenever the running-apps set may have changed.
pub fn install_workspace_observers() -> (
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    WorkspaceObserverGuard,
) {
    use block2::RcBlock;
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidLaunchApplicationNotification,
        NSWorkspaceDidTerminateApplicationNotification,
    };
    use objc2_foundation::NSOperationQueue;
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dirty = Arc::new(AtomicBool::new(false));
    let dirty_launch = Arc::clone(&dirty);
    let dirty_terminate = Arc::clone(&dirty);

    let launch_block = RcBlock::new(
        move |_notification: NonNull<objc2_foundation::NSNotification>| {
            dirty_launch.store(true, Ordering::Relaxed);
            wake_event_loop();
        },
    );
    let terminate_block = RcBlock::new(
        move |_notification: NonNull<objc2_foundation::NSNotification>| {
            dirty_terminate.store(true, Ordering::Relaxed);
            wake_event_loop();
        },
    );

    let workspace = NSWorkspace::sharedWorkspace();
    let center = workspace.notificationCenter();
    let main_queue = NSOperationQueue::mainQueue();

    let launch_observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidLaunchApplicationNotification),
            None,
            Some(&main_queue),
            &launch_block,
        )
    };
    let terminate_observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidTerminateApplicationNotification),
            None,
            Some(&main_queue),
            &terminate_block,
        )
    };

    let guard = WorkspaceObserverGuard {
        launch: launch_observer,
        terminate: terminate_observer,
        _launch_block: launch_block,
        _terminate_block: terminate_block,
    };

    (dirty, guard)
}

/// Wait up to `timeout` for the first event, then drain any queued events and
/// return promptly so the caller can react (e.g. to tray/menu channel events)
/// without waiting out the full interval.
///
/// `None` blocks until an event arrives or [`wake_event_loop`] is called.
pub fn pump_event_loop(timeout: Option<std::time::Duration>) {
    use objc2_app_kit::{NSApplication, NSEventMask};
    use objc2_foundation::{MainThreadMarker, NSDate, NSDefaultRunLoopMode};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let deadline = timeout.map_or_else(NSDate::distantFuture, |timeout| {
        NSDate::now().dateByAddingTimeInterval(timeout.as_secs_f64())
    });
    let drain = NSDate::distantPast();
    let mode = unsafe { NSDefaultRunLoopMode };

    let mut wait_until = &*deadline;
    while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any,
        Some(wait_until),
        mode,
        true,
    ) {
        app.sendEvent(&event);
        wait_until = &drain;
    }
}
