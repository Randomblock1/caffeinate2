//! Running application discovery and bundle selection (tray).

use crate::app_target::AppTarget;
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSModalResponseOK, NSOpenPanel, NSRunningApplication,
    NSWorkspace,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSBundle, NSString, NSURL};
use objc2_uniform_type_identifiers::UTTypeApplicationBundle;

pub fn running_app_choices() -> Vec<AppTarget> {
    if MainThreadMarker::new().is_none() {
        return Vec::new();
    }

    let workspace = NSWorkspace::sharedWorkspace();
    let apps = workspace.runningApplications();
    let mut choices: Vec<AppTarget> = Vec::new();

    for app in apps.iter() {
        if app.isTerminated() {
            continue;
        }
        let policy = app.activationPolicy();
        if policy == NSApplicationActivationPolicy::Prohibited {
            continue;
        }
        let Some(bundle_id) = app.bundleIdentifier() else {
            continue;
        };
        let bundle_id = bundle_id.to_string();
        if bundle_id.is_empty() {
            continue;
        }
        let name = app
            .localizedName()
            .map(|n| n.to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| bundle_id.clone());
        if let Some(existing) = choices.iter_mut().find(|c| c.bundle_id == bundle_id) {
            if existing.name.len() < name.len() {
                existing.name = name;
            }
        } else {
            choices.push(AppTarget { bundle_id, name });
        }
    }

    choices.sort_by_key(|choice| choice.name.to_lowercase());
    choices
}

pub fn is_bundle_running(bundle_id: &str) -> bool {
    if MainThreadMarker::new().is_none() {
        return false;
    }

    let bundle_id = NSString::from_str(bundle_id);
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle_id);
    apps.iter().any(|app| !app.isTerminated())
}

/// Modal open panel for an `.app` bundle (including apps that are not running).
pub fn choose_app_bundle() -> Option<AppTarget> {
    let mtm = MainThreadMarker::new()?;
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    // Keep .app bundles selectable as files; treating packages as directories
    // would make the panel browse into them instead.
    panel.setTreatsFilePackagesAsDirectories(false);
    let app_bundle_type = unsafe { UTTypeApplicationBundle };
    panel.setAllowedContentTypes(&NSArray::from_slice(&[app_bundle_type]));

    let response = panel.runModal();
    if response != NSModalResponseOK {
        return None;
    }

    let url = panel.URL()?;
    let path = url.path()?;
    let path = path.to_string();
    if !path.ends_with(".app") {
        return None;
    }
    bundle_from_app_path(&path)
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
