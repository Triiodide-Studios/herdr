//! Keeps the app loop's plugin registry current without reading a file on the loop.
//!
//! Event hooks used to reload the registry before every event: a lock file opened and locked, `plugins.json` read, and every
//! plugin manifest read again, all on the app loop. A file system that waits (an open on a busy volume) then held the loop for as
//! long as it waited. The registry is now loaded by a short-lived background thread, and the loop only adopts a finished result.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::api::schema::InstalledPluginInfo;
use crate::plugin_installations::{self, Leases};

type LoadedRegistry = (Vec<InstalledPluginInfo>, Leases);

/// A load starts at most this often, however many events ask for one. A registry change made by the CLI is seen by the hooks of
/// the first event after the load that follows it.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Shared {
    in_flight: bool,
    last_started: Option<Instant>,
    /// Bumped when the loop reads or writes the registry itself: a load that began before is stale and is dropped.
    epoch: u64,
    ready: Option<LoadedRegistry>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Default)]
pub(crate) struct PluginRegistryRefresh {
    shared: Arc<Mutex<Shared>>,
}

impl PluginRegistryRefresh {
    /// Starts a background load unless one is running or the last one started less than [`MIN_REFRESH_INTERVAL`] ago.
    pub(crate) fn request(&self) {
        let epoch = {
            let mut shared = lock(&self.shared);
            if shared.in_flight
                || shared
                    .last_started
                    .is_some_and(|started| started.elapsed() < MIN_REFRESH_INTERVAL)
            {
                return;
            }
            shared.in_flight = true;
            shared.last_started = Some(Instant::now());
            shared.epoch
        };
        let shared = Arc::clone(&self.shared);
        let spawned = std::thread::Builder::new()
            .name("plugin-registry-refresh".to_string())
            .spawn(move || {
                let loaded = load_registry();
                let mut shared = lock(&shared);
                shared.in_flight = false;
                match loaded {
                    Ok(entries) if shared.epoch == epoch => shared.ready = Some(entries),
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(err = %err, "failed to refresh the plugin registry in the background");
                    }
                }
            });
        if spawned.is_err() {
            lock(&self.shared).in_flight = false;
        }
    }

    /// Takes the registry the last finished load produced, if the loop has not read or written the registry itself since it began.
    pub(crate) fn take_ready(&self) -> Option<LoadedRegistry> {
        lock(&self.shared).ready.take()
    }

    /// The loop has just read or written the registry itself: a load that began before this call must not replace what it has.
    pub(crate) fn invalidate(&self) {
        let mut shared = lock(&self.shared);
        shared.epoch += 1;
        shared.ready = None;
    }

    #[cfg(test)]
    pub(crate) fn allow_immediate_request(&self) {
        lock(&self.shared).last_started = None;
    }

    #[cfg(test)]
    pub(crate) fn wait_idle(&self) {
        while lock(&self.shared).in_flight {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn load_registry() -> std::io::Result<LoadedRegistry> {
    let mut leases = Leases::new();
    let entries = plugin_installations::load(&mut leases)?;
    let entries = crate::persist::plugin_registry::reload_manifests(entries, |path, enabled| {
        super::load_plugin_manifest(path, enabled).map_err(|(_, message)| message)
    });
    Ok((entries, leases))
}
