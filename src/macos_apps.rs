//! Bundle lookup and running-app checks (tray).

use crate::app_target::AppTarget;
use objc2_app_kit::NSRunningApplication;
use objc2_foundation::{MainThreadMarker, NSBundle, NSString, NSURL};

pub fn is_bundle_running(bundle_id: &str) -> bool {
    if MainThreadMarker::new().is_none() {
        return false;
    }

    let bundle_id = NSString::from_str(bundle_id);
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle_id);
    apps.iter().any(|app| !app.isTerminated())
}

pub fn bundle_from_app_path(path: &str) -> Option<AppTarget> {
    let ns_path = NSString::from_str(path);
    let url = NSURL::fileURLWithPath(&ns_path);
    let bundle = NSBundle::bundleWithURL(&url)?;
    let bundle_id = bundle.bundleIdentifier()?.to_string();
    if bundle_id.is_empty() {
        return None;
    }
    let name = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&bundle_id)
        .to_string();
    Some(AppTarget { bundle_id, name })
}
