//! Updates of the Windows build, through Velopack: its installer puts Ferret in
//! %LOCALAPPDATA%\Petru101.Ferret (not Ferret's data folder, which an uninstall would delete),
//! and its feed is on the update site. Linux players get updates from Ferret's own flatpak repo
//! (data/flatpak/publish.sh), through Flatpak itself.

use std::sync::atomic::{AtomicBool, Ordering};
use velopack::{sources::FileSource, sources::HttpSource, UpdateCheck, UpdateManager, VelopackApp};

/// The feed `vpk pack` makes (releases.win.json and the packages), on the update site.
const FEED: &str = "https://petru101.github.io/ferret-updates/windows/";

/// The player asked for the downloaded build now (the banner's Restart): applied once the
/// window has closed and the helper with it.
static APPLY: AtomicBool = AtomicBool::new(false);

/// Velopack's part of every start, first thing: its install, update and uninstall hooks (they
/// exit), and a build downloaded during an earlier run applied before Ferret starts (so never
/// while the helper might be tracing a game).
pub fn on_start() {
    VelopackApp::build().run();
}

/// The feed: the update site's, or FERRET_UPDATE_FEED's (tests: a folder `vpk pack` wrote to).
fn manager() -> Option<UpdateManager> {
    match std::env::var("FERRET_UPDATE_FEED") {
        Ok(dir) if !dir.is_empty() => UpdateManager::new(FileSource::new(dir), None, None).ok(),
        _ => UpdateManager::new(HttpSource::new(FEED), None, None).ok(),
    }
}

/// Downloads a newer build, if there is one, and says its version. None when there isn't, when
/// the check failed, or when Ferret wasn't installed by its installer (a build run from where
/// it was built).
pub fn download() -> Option<String> {
    let um = manager()?;
    if let Some(pending) = um.get_update_pending_restart() {
        return Some(pending.Version);
    }
    match um.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(info)) => {
            um.download_updates(&info, None).inspect_err(|e| eprintln!("update: {e}")).ok()?;
            Some(info.TargetFullRelease.Version.clone())
        }
        Ok(_) => None,
        Err(e) => {
            eprintln!("update: {e}");
            None
        }
    }
}

/// The banner's Restart: the window quits, then `apply_if_asked` installs the new build and
/// starts it.
pub fn ask_to_apply() {
    APPLY.store(true, Ordering::Relaxed);
}

/// After the window has closed: installs the downloaded build and starts Ferret again, if the
/// player asked (otherwise the next start installs it).
pub fn apply_if_asked() {
    if !APPLY.load(Ordering::Relaxed) {
        return;
    }
    if let Some(um) = manager() {
        if let Some(pending) = um.get_update_pending_restart() {
            if let Err(e) = um.apply_updates_and_restart(pending) {
                eprintln!("update: {e}");
            }
        }
    }
}
