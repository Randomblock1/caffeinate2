#[cfg(all(target_os = "macos", feature = "tray"))]
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

#[cfg(all(target_os = "macos", feature = "tray"))]
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

/// Forward tray/menu events to channels. `tray-icon` and `muda` only deliver
/// events through `set_event_handler`; `receiver()` is disabled once a handler is set.
#[cfg(all(target_os = "macos", feature = "tray"))]
pub fn install_tray_event_handlers(
) -> (
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

/// Keeps NSWorkspace launch/terminate observers registered for the app lifetime.
#[cfg(all(target_os = "macos", feature = "tray"))]
pub struct WorkspaceObserverGuard {
    _launch: objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2::runtime::NSObjectProtocol>>,
    _terminate:
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2::runtime::NSObjectProtocol>>,
    _launch_block: block2::RcBlock<dyn Fn(std::ptr::NonNull<objc2_foundation::NSNotification>)>,
    _terminate_block: block2::RcBlock<dyn Fn(std::ptr::NonNull<objc2_foundation::NSNotification>)>,
}

/// Register observers for app launch/quit. The returned flag is set (and the
/// event loop woken) whenever the running-apps set may have changed.
#[cfg(all(target_os = "macos", feature = "tray"))]
pub fn install_workspace_observers(
) -> (
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

    let launch_block = RcBlock::new(move |_notification: NonNull<objc2_foundation::NSNotification>| {
        dirty_launch.store(true, Ordering::Relaxed);
        wake_event_loop();
    });
    let terminate_block =
        RcBlock::new(move |_notification: NonNull<objc2_foundation::NSNotification>| {
            dirty_terminate.store(true, Ordering::Relaxed);
            wake_event_loop();
        });

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
        _launch: launch_observer,
        _terminate: terminate_observer,
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
#[cfg(all(target_os = "macos", feature = "tray"))]
pub fn pump_event_loop(timeout: Option<std::time::Duration>) {
    use objc2_app_kit::{NSEventMask, NSApplication};
    use objc2_foundation::{MainThreadMarker, NSDate, NSDefaultRunLoopMode};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let deadline = match timeout {
        Some(timeout) => NSDate::now().dateByAddingTimeInterval(timeout.as_secs_f64()),
        None => NSDate::distantFuture(),
    };
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
