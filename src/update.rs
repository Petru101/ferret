//! Updates. Windows: Velopack, whose installer puts Ferret in %LOCALAPPDATA%\Petru101.Ferret (not
//! Ferret's data folder, which an uninstall would delete) and whose feed is on the update site.
//! Linux: Ferret's own flatpak repo (data/flatpak/publish.sh), through Flatpak itself; Ferret
//! installs nothing unless the player asks, and "off" is a Flatpak mask, which every updater
//! going through Flatpak honours.

/// What looking for a new version found.
pub enum Check {
    Newest,
    /// Linux: a newer build is on the update site, with its version when the site says.
    #[cfg(not(windows))]
    Available(Option<String>),
    /// Linux: the newest is installed, but this running one is older.
    #[cfg(not(windows))]
    Restart,
    /// Windows: a newer build, downloaded and waiting for a restart.
    #[cfg(windows)]
    Ready(String),
    /// Not a copy that can be updated (a build run from where it was built).
    NotInstalled,
}

#[cfg(windows)]
mod velopack_updates {
    use super::Check;
    use std::sync::atomic::{AtomicBool, Ordering};
    use velopack::{
        bundle::Manifest,
        sources::{FileSource, HttpSource, UpdateSource},
        Error, UpdateCheck, UpdateManager, VelopackApp, VelopackAsset, VelopackAssetFeed,
    };

    /// The feed `vpk pack` makes (releases.win.json and the packages), on the update site.
    const FEED: &str = "https://petru101.github.io/ferret-updates/windows/";

    /// The public half of the key the feed is signed with (data/release/from-github.sh; its
    /// secret half never goes on GitHub). The feed holds each package's hash, which Velopack
    /// checks, so a signed feed vouches for what it lists. A test build can be given another.
    const KEY: &str = match option_env!("FERRET_UPDATE_KEY") {
        Some(key) => key,
        None => "RWRljgNns5TlK0m+OWBxhqMKwbZMcz4+l2p2d+H3J8ZAjqTrDsBv8gut",
    };

    /// A feed read only if its signature holds; the packages come through the source as usual.
    struct Signed<S> {
        source: S,
        /// The feed's location: a URL or, in tests, a folder.
        at: String,
    }

    impl<S> Signed<S> {
        fn read(&self, name: &str) -> Result<String, String> {
            if !self.at.contains("://") {
                return std::fs::read_to_string(std::path::Path::new(&self.at).join(name)).map_err(|e| format!("{name}: {e}"));
            }
            let url = format!("{}{name}", self.at);
            let mut reply = ureq::get(&url).call().map_err(|e| format!("{name}: {e}"))?;
            reply.body_mut().read_to_string().map_err(|e| format!("{name}: {e}"))
        }
    }

    /// Whether `feed` is what was signed with `key`.
    fn signature_holds(feed: &str, signature: &str, key: &str) -> Result<(), String> {
        let key = minisign_verify::PublicKey::from_base64(key).map_err(|e| format!("Ferret's own update key can't be read: {e}"))?;
        let signature = minisign_verify::Signature::decode(signature).map_err(|e| format!("the update list's signature can't be read: {e}"))?;
        key.verify(feed.as_bytes(), &signature, false).map_err(|_| "the update list's signature doesn't match Ferret's key".to_owned())
    }

    impl<S: UpdateSource> UpdateSource for Signed<S> {
        fn get_release_feed(&self, channel: &str, _app: &Manifest, _staged_user_id: &str) -> Result<VelopackAssetFeed, Error> {
            let refused = |why: String| Error::Other(why);
            let name = format!("releases.{channel}.json");
            let feed = self.read(&name).map_err(refused)?;
            let signature = self.read(&format!("{name}.minisig")).map_err(refused)?;
            signature_holds(&feed, &signature, KEY).map_err(refused)?;
            Ok(serde_json::from_str(&feed)?)
        }

        fn download_release_entry(
            &self,
            asset: &VelopackAsset,
            local_file: &std::path::Path,
            progress: Option<std::sync::mpsc::Sender<i16>>,
        ) -> Result<(), Error> {
            self.source.download_release_entry(asset, local_file, progress)
        }
    }

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
            Ok(dir) if !dir.is_empty() => UpdateManager::new(Signed { source: FileSource::new(&dir), at: dir }, None, None).ok(),
            _ => UpdateManager::new(Signed { source: HttpSource::new(FEED), at: FEED.to_owned() }, None, None).ok(),
        }
    }

    pub fn installed() -> bool {
        manager().is_some()
    }

    /// Downloads a newer build, if there is one.
    pub fn check() -> Result<Check, String> {
        let Some(um) = manager() else { return Ok(Check::NotInstalled) };
        if let Some(pending) = um.get_update_pending_restart() {
            return Ok(Check::Ready(pending.Version));
        }
        match um.check_for_updates().map_err(|e| e.to_string())? {
            UpdateCheck::UpdateAvailable(info) => {
                um.download_updates(&info, None).map_err(|e| e.to_string())?;
                Ok(Check::Ready(info.TargetFullRelease.Version.clone()))
            }
            _ => Ok(Check::Newest),
        }
    }

    /// The check at start: the version of a newer build it downloaded. None when there isn't one,
    /// when the check failed, or when Ferret wasn't installed by its installer.
    pub fn download() -> Option<String> {
        match check() {
            Ok(Check::Ready(version)) => Some(version),
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
}

#[cfg(windows)]
pub use velopack_updates::*;

#[cfg(not(windows))]
mod flatpak_updates {
    use super::Check;
    use crate::core::flatpak_info;
    use std::process::{Command, Stdio};

    /// This install as Flatpak's commands name it.
    struct Install {
        /// --user or --system.
        level: &'static str,
        /// `<id>//<branch>`.
        reference: String,
        /// The pattern that masks only this branch: a test build's mask can't hold back the
        /// player's own.
        mask: String,
    }

    fn this_install() -> Option<Install> {
        let (id, branch) = (flatpak_info("name")?, flatpak_info("branch")?);
        let system = flatpak_info("app-path")?.starts_with("/var/lib/flatpak/");
        Some(Install {
            level: if system { "--system" } else { "--user" },
            reference: format!("{id}//{branch}"),
            mask: format!("app/{id}/*/{branch}"),
        })
    }

    /// A flatpak command on the host: what it printed, or why it failed.
    fn flatpak(args: &[&str]) -> Result<String, String> {
        let out = Command::new("flatpak-spawn")
            .args(["--host", "flatpak"])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }

    pub fn installed() -> bool {
        this_install().is_some()
    }

    /// Whether Flatpak may update Ferret: it isn't masked.
    pub fn automatic() -> bool {
        let Some(me) = this_install() else { return true };
        let list = flatpak(&["mask", me.level]).unwrap_or_default();
        !list.lines().any(|l| l.trim() == me.mask)
    }

    /// For a system-wide install the desktop asks for the admin password.
    pub fn set_automatic(on: bool) -> Result<(), String> {
        let Some(me) = this_install() else { return Ok(()) };
        let mut args = vec!["mask", me.level];
        if on {
            args.push("--remove");
        }
        args.push(&me.mask);
        flatpak(&args).map(|_| ())
    }

    /// Asks the update site about this install's branch.
    pub fn check() -> Result<Check, String> {
        let Some(me) = this_install() else { return Ok(Check::NotInstalled) };
        let origin = flatpak(&["info", me.level, "--show-origin", &me.reference])?;
        let newest = flatpak(&["remote-info", me.level, "--show-commit", &origin, &me.reference])?;
        let installed = flatpak(&["info", me.level, "--show-commit", &me.reference])?;
        if newest != installed {
            let about = flatpak(&["remote-info", me.level, &origin, &me.reference]).unwrap_or_default();
            let version = about.lines().find_map(|l| l.trim().strip_prefix("Version:")).map(|v| v.trim().to_owned());
            return Ok(Check::Available(version));
        }
        Ok(if flatpak_info("app-commit").is_some_and(|running| running != installed) { Check::Restart } else { Check::Newest })
    }

    /// Installs the newest build, whatever the switch says: a masked app is left alone even when
    /// named, so the mask comes off for the update and goes back on after it.
    pub fn install() -> Result<(), String> {
        let Some(me) = this_install() else { return Ok(()) };
        let off = !automatic();
        if off {
            set_automatic(true)?;
        }
        let done = flatpak(&["update", me.level, "--noninteractive", "-y", &me.reference]).map(|_| ());
        if off {
            set_automatic(false)?;
        }
        done
    }
}

#[cfg(not(windows))]
pub use flatpak_updates::*;
