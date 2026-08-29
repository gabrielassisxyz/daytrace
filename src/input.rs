use evdev::{Device, EventType};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How often the supervisor re-enumerates `/dev/input` for a device that was not present, or
/// not readable, the last time it looked: a Bluetooth keyboard reconnecting, a USB dock plugged
/// back in, or a permission that only appears after a udev rule runs.
///
/// A named constant, and measured in seconds rather than run on every poll, so the daemon never
/// pays a full directory scan on every capture tick: enumeration happens on its own interval,
/// independent of how often the desktop or media sources are polled.
const DEVICE_RESCAN_INTERVAL: Duration = Duration::from_secs(30);

/// The input-device boundary the supervisor and its watchers observe through.
///
/// A trait because a device that disappears, errors, or appears after the daemon started
/// cannot be staged against a real `/dev/input`, and the suite has to run headless.
trait InputDeviceSource: Send + Sync {
    /// The paths of every readable input device currently present.
    fn discover(&self) -> Vec<PathBuf>;

    /// Open one device for watching. Kept apart from setting non-blocking mode because both are
    /// real, distinct failure points a watcher can hit before it ever reads an event.
    fn open(&self, path: &Path) -> io::Result<Box<dyn WatchedDevice>>;
}

/// One already-open device, polled for whether it produced activity since the last poll.
trait WatchedDevice {
    fn set_nonblocking(&mut self) -> io::Result<()>;

    /// Whether an activity-carrying event (key, relative or absolute axis) arrived since the
    /// last poll. `Err` with `ErrorKind::WouldBlock` means "nothing yet, still alive"; any other
    /// error means the device is gone and the watcher retires.
    fn poll(&mut self) -> io::Result<bool>;
}

/// What one moment tells the capture loop about input: when it last saw any, and how many
/// watchers are alive to see more.
///
/// The two travel together because a frozen `last_activity_at` is only trustworthy while at
/// least one watcher is alive to freeze it honestly. Once the last watcher retires, the
/// timestamp stops advancing for a reason that has nothing to do with the user being away, and
/// a caller that reads the timestamp alone cannot tell the two apart.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct InputObservation {
    pub last_activity_at: i64,
    pub watchers_alive: usize,
}

impl InputObservation {
    /// Whether at least one watcher is alive to vouch for `last_activity_at`.
    pub fn is_observing(&self) -> bool {
        self.watchers_alive > 0
    }
}

pub struct InputActivity {
    last_activity_at: Arc<AtomicI64>,
    watcher_count: Arc<AtomicUsize>,
}

impl InputActivity {
    pub fn start(running: Arc<AtomicBool>, initial_timestamp: i64) -> Result<Self, String> {
        Self::start_with(
            running,
            initial_timestamp,
            Arc::new(EvdevDeviceSource),
            DEVICE_RESCAN_INTERVAL,
        )
    }

    /// The real implementation behind `start`, parameterized over the device boundary and the
    /// rescan cadence so a test can drive both without a real `/dev/input` and without waiting
    /// out a real 30 seconds.
    fn start_with(
        running: Arc<AtomicBool>,
        initial_timestamp: i64,
        source: Arc<dyn InputDeviceSource>,
        rescan_interval: Duration,
    ) -> Result<Self, String> {
        let last_activity_at = Arc::new(AtomicI64::new(initial_timestamp));
        let watcher_count = Arc::new(AtomicUsize::new(0));
        let watched_paths = Arc::new(Mutex::new(HashSet::new()));

        let devices = source.discover();
        if devices.is_empty() {
            return Err(
                "no readable input devices found under /dev/input; AFK tracking needs input-event read access"
                    .to_string(),
            );
        }

        for path in devices {
            spawn_watcher(
                path,
                Arc::clone(&source),
                Arc::clone(&running),
                Arc::clone(&last_activity_at),
                Arc::clone(&watcher_count),
                Arc::clone(&watched_paths),
            );
        }

        thread::spawn({
            let source = Arc::clone(&source);
            let running = Arc::clone(&running);
            let last_activity_at = Arc::clone(&last_activity_at);
            let watcher_count = Arc::clone(&watcher_count);
            let watched_paths = Arc::clone(&watched_paths);
            move || {
                supervise_devices(
                    source,
                    running,
                    last_activity_at,
                    watcher_count,
                    watched_paths,
                    rescan_interval,
                )
            }
        });

        Ok(Self {
            last_activity_at,
            watcher_count,
        })
    }

    pub fn observation(&self) -> InputObservation {
        InputObservation {
            last_activity_at: self.last_activity_at.load(Ordering::Relaxed),
            watchers_alive: self.watcher_count.load(Ordering::Relaxed),
        }
    }
}

/// Re-enumerate `/dev/input` every `rescan_interval` and spawn a watcher for anything found that
/// is not already being watched: a device that appeared after the daemon started, or one whose
/// earlier watcher retired and freed its path.
fn supervise_devices(
    source: Arc<dyn InputDeviceSource>,
    running: Arc<AtomicBool>,
    last_activity_at: Arc<AtomicI64>,
    watcher_count: Arc<AtomicUsize>,
    watched_paths: Arc<Mutex<HashSet<PathBuf>>>,
    rescan_interval: Duration,
) {
    while running.load(Ordering::Relaxed) {
        sleep_while_running(&running, rescan_interval);
        if !running.load(Ordering::Relaxed) {
            return;
        }

        let discovered = source.discover();
        let already_watched = watched_paths
            .lock()
            .map(|paths| paths.clone())
            .unwrap_or_default();

        for path in devices_needing_watchers(&discovered, &already_watched) {
            spawn_watcher(
                path,
                Arc::clone(&source),
                Arc::clone(&running),
                Arc::clone(&last_activity_at),
                Arc::clone(&watcher_count),
                Arc::clone(&watched_paths),
            );
        }
    }
}

/// Which of the currently discovered devices are not already being watched.
///
/// Pure and free of any device or filesystem access, so a device appearing after the daemon
/// started, or reappearing after its watcher retired, is provable without a real supervisor
/// thread.
fn devices_needing_watchers(discovered: &[PathBuf], watched: &HashSet<PathBuf>) -> Vec<PathBuf> {
    discovered
        .iter()
        .filter(|path| !watched.contains(*path))
        .cloned()
        .collect()
}

/// Sleep up to `interval`, breaking early if `running` turns false, so shutdown does not have to
/// wait out a full rescan interval before the supervisor thread notices.
fn sleep_while_running(running: &AtomicBool, interval: Duration) {
    let deadline = std::time::Instant::now() + interval;
    while running.load(Ordering::Relaxed) {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(200)));
    }
}

/// Claim `path` as watched and spawn its watcher thread.
///
/// The claim (inserting into `watched_paths` and incrementing `watcher_count`) happens here,
/// before the thread starts, so a rescan running concurrently with this spawn cannot also decide
/// the same path still needs a watcher. The `JoinHandle` is unused in production, where watchers
/// run for the life of the daemon, and exists so a test can wait for one to finish
/// deterministically.
fn spawn_watcher(
    path: PathBuf,
    source: Arc<dyn InputDeviceSource>,
    running: Arc<AtomicBool>,
    last_activity_at: Arc<AtomicI64>,
    watcher_count: Arc<AtomicUsize>,
    watched_paths: Arc<Mutex<HashSet<PathBuf>>>,
) -> thread::JoinHandle<()> {
    if let Ok(mut paths) = watched_paths.lock() {
        paths.insert(path.clone());
    }
    watcher_count.fetch_add(1, Ordering::Relaxed);

    thread::spawn(move || {
        // Retired on every return from `watch_device` below, including the open and
        // non-blocking failures at its very start: a watcher that never managed to observe
        // anything must not be counted as though it could, and its path must be free for the
        // next rescan to try again rather than staying claimed by a thread that has already
        // ended.
        let _retirement = WatcherRetirement {
            watcher_count,
            watched_paths,
            path: path.clone(),
        };
        watch_device(source.as_ref(), &path, &running, &last_activity_at);
    })
}

/// Frees a watcher's claim on its device, no matter which path through `watch_device` produced
/// the return that ends its thread.
struct WatcherRetirement {
    watcher_count: Arc<AtomicUsize>,
    watched_paths: Arc<Mutex<HashSet<PathBuf>>>,
    path: PathBuf,
}

impl Drop for WatcherRetirement {
    fn drop(&mut self) {
        self.watcher_count.fetch_sub(1, Ordering::Relaxed);
        if let Ok(mut paths) = self.watched_paths.lock() {
            paths.remove(&self.path);
        }
    }
}

fn watch_device(
    source: &dyn InputDeviceSource,
    path: &Path,
    running: &AtomicBool,
    last_activity_at: &AtomicI64,
) {
    let Ok(mut device) = source.open(path) else {
        return;
    };
    if device.set_nonblocking().is_err() {
        return;
    }

    while running.load(Ordering::Relaxed) {
        match device.poll() {
            Ok(true) => {
                last_activity_at.store(crate::timeline::unix_now(), Ordering::Relaxed);
            }
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return,
        }
    }
}

/// The real `/dev/input` boundary: evdev devices opened from disk.
struct EvdevDeviceSource;

impl InputDeviceSource for EvdevDeviceSource {
    fn discover(&self) -> Vec<PathBuf> {
        discover_activity_devices("/dev/input")
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn WatchedDevice>> {
        let device = Device::open(path)?;
        Ok(Box::new(EvdevWatchedDevice(device)))
    }
}

struct EvdevWatchedDevice(Device);

impl WatchedDevice for EvdevWatchedDevice {
    fn set_nonblocking(&mut self) -> io::Result<()> {
        self.0.set_nonblocking(true)
    }

    fn poll(&mut self) -> io::Result<bool> {
        self.0
            .fetch_events()
            .map(|events| events.into_iter().any(is_activity_event))
    }
}

fn discover_activity_devices(input_dir: impl AsRef<Path>) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(input_dir) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
        })
        .filter(|path| {
            Device::open(path)
                .map(|device| {
                    device.supported_keys().is_some()
                        || device.supported_relative_axes().is_some()
                        || device.supported_absolute_axes().is_some()
                })
                .unwrap_or(false)
        })
        .collect()
}

fn is_activity_event(event: evdev::InputEvent) -> bool {
    matches!(
        event.event_type(),
        EventType::KEY | EventType::RELATIVE | EventType::ABSOLUTE
    )
}

#[cfg(test)]
mod fakes {
    use super::{InputDeviceSource, WatchedDevice};
    use std::collections::{HashMap, VecDeque};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// What one poll of a scripted device reports, mirroring the three outcomes evdev can
    /// actually produce: an activity-carrying event, "nothing yet" (`WouldBlock`), or a read
    /// that failed outright.
    #[derive(Clone)]
    pub enum ScriptedPoll {
        Activity,
        WouldBlock,
        Error,
    }

    /// What happens when a scripted device is opened: one of the two failure points
    /// `watch_device` has before it ever polls, or a script of polls to replay once both
    /// succeed.
    #[derive(Clone)]
    pub enum DeviceScript {
        OpenFails,
        NonblockingFails,
        Polls(Vec<ScriptedPoll>),
    }

    struct ScriptedDevice {
        nonblocking_fails: bool,
        polls: VecDeque<ScriptedPoll>,
    }

    impl WatchedDevice for ScriptedDevice {
        fn set_nonblocking(&mut self) -> io::Result<()> {
            if self.nonblocking_fails {
                Err(io::Error::other("nonblocking setup refused"))
            } else {
                Ok(())
            }
        }

        fn poll(&mut self) -> io::Result<bool> {
            match self.polls.pop_front() {
                Some(ScriptedPoll::Activity) => Ok(true),
                // A script that ran out behaves like a real quiet device: nothing yet, still
                // alive, so the watcher sleeps and asks again rather than retiring.
                Some(ScriptedPoll::WouldBlock) | None => Err(io::ErrorKind::WouldBlock.into()),
                Some(ScriptedPoll::Error) => Err(io::Error::other("device read failed")),
            }
        }
    }

    /// A device source whose device set, and each device's open/poll script, are set by the
    /// test and can change between calls to `discover`: the shape a device appearing,
    /// disappearing or erroring after the daemon started needs, without a real `/dev/input`.
    #[derive(Default)]
    pub struct ScriptedDeviceSource {
        state: Mutex<ScriptedDeviceSourceState>,
    }

    #[derive(Default)]
    struct ScriptedDeviceSourceState {
        present: Vec<PathBuf>,
        scripts: HashMap<PathBuf, DeviceScript>,
    }

    impl ScriptedDeviceSource {
        pub fn new() -> Self {
            Self::default()
        }

        /// Make a device discoverable and set what opening and polling it does, replacing this
        /// test's earlier arrangement for the same path if there was one: how a device that
        /// errors and later recovers is staged.
        pub fn set_device(&self, path: &str, script: DeviceScript) {
            let mut state = self.state.lock().expect("scripted device source lock");
            let path = PathBuf::from(path);
            if !state.present.contains(&path) {
                state.present.push(path.clone());
            }
            state.scripts.insert(path, script);
        }
    }

    impl InputDeviceSource for ScriptedDeviceSource {
        fn discover(&self) -> Vec<PathBuf> {
            self.state
                .lock()
                .expect("scripted device source lock")
                .present
                .clone()
        }

        fn open(&self, path: &Path) -> io::Result<Box<dyn WatchedDevice>> {
            let state = self.state.lock().expect("scripted device source lock");
            match state.scripts.get(path) {
                Some(DeviceScript::OpenFails) | None => Err(io::Error::other("open refused")),
                Some(DeviceScript::NonblockingFails) => Ok(Box::new(ScriptedDevice {
                    nonblocking_fails: true,
                    polls: VecDeque::new(),
                })),
                Some(DeviceScript::Polls(polls)) => Ok(Box::new(ScriptedDevice {
                    nonblocking_fails: false,
                    polls: polls.clone().into(),
                })),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::{DeviceScript, ScriptedDeviceSource, ScriptedPoll};
    use super::{
        InputActivity, InputDeviceSource, InputObservation, devices_needing_watchers,
        discover_activity_devices, spawn_watcher, watch_device,
    };
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    /// Poll `condition` until it is true, or panic once `timeout` has passed: how these tests
    /// wait for a background thread's effect without pinning the wait to a fixed sleep.
    fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "condition never became true within {timeout:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn missing_input_dir_has_no_devices() {
        assert!(discover_activity_devices("/path/that/does/not/exist").is_empty());
    }

    #[test]
    fn devices_needing_watchers_skips_paths_already_watched() {
        let discovered = vec![PathBuf::from("/fake/event0"), PathBuf::from("/fake/event1")];
        let mut watched = HashSet::new();
        watched.insert(PathBuf::from("/fake/event0"));

        assert_eq!(
            devices_needing_watchers(&discovered, &watched),
            vec![PathBuf::from("/fake/event1")]
        );
    }

    #[test]
    fn watch_device_returns_immediately_when_open_fails() {
        let source = ScriptedDeviceSource::new();
        source.set_device("/fake/event0", DeviceScript::OpenFails);
        let running = AtomicBool::new(true);
        let last_activity_at = AtomicI64::new(0);

        watch_device(
            &source,
            Path::new("/fake/event0"),
            &running,
            &last_activity_at,
        );

        assert_eq!(last_activity_at.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn watch_device_returns_immediately_when_nonblocking_setup_fails() {
        let source = ScriptedDeviceSource::new();
        source.set_device("/fake/event0", DeviceScript::NonblockingFails);
        let running = AtomicBool::new(true);
        let last_activity_at = AtomicI64::new(0);

        watch_device(
            &source,
            Path::new("/fake/event0"),
            &running,
            &last_activity_at,
        );

        assert_eq!(last_activity_at.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn watch_device_records_activity_and_retires_on_a_read_error() {
        let source = ScriptedDeviceSource::new();
        source.set_device(
            "/fake/event0",
            DeviceScript::Polls(vec![ScriptedPoll::Activity, ScriptedPoll::Error]),
        );
        let running = AtomicBool::new(true);
        let last_activity_at = AtomicI64::new(0);
        let before = crate::timeline::unix_now();

        watch_device(
            &source,
            Path::new("/fake/event0"),
            &running,
            &last_activity_at,
        );

        assert!(
            last_activity_at.load(Ordering::Relaxed) >= before,
            "an activity poll must record the moment it was seen"
        );
    }

    #[test]
    fn watch_device_keeps_polling_through_would_block_rather_than_retiring() {
        let source = ScriptedDeviceSource::new();
        source.set_device(
            "/fake/event0",
            DeviceScript::Polls(vec![
                ScriptedPoll::WouldBlock,
                ScriptedPoll::WouldBlock,
                ScriptedPoll::Error,
            ]),
        );
        let running = AtomicBool::new(true);
        let last_activity_at = AtomicI64::new(0);

        // Returns only once the script reaches its terminal error, which is only reachable if
        // the two `WouldBlock` polls before it were retried rather than treated as fatal.
        watch_device(
            &source,
            Path::new("/fake/event0"),
            &running,
            &last_activity_at,
        );

        assert_eq!(last_activity_at.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_retired_watcher_frees_its_slot_in_the_alive_count() {
        let source = Arc::new(ScriptedDeviceSource::new());
        source.set_device("/fake/event0", DeviceScript::OpenFails);
        let trait_source: Arc<dyn InputDeviceSource> = source;
        let watcher_count = Arc::new(AtomicUsize::new(0));
        let watched_paths = Arc::new(Mutex::new(HashSet::new()));
        let last_activity_at = Arc::new(AtomicI64::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let handle = spawn_watcher(
            PathBuf::from("/fake/event0"),
            trait_source,
            Arc::clone(&running),
            Arc::clone(&last_activity_at),
            Arc::clone(&watcher_count),
            Arc::clone(&watched_paths),
        );
        handle.join().expect("watcher thread must not panic");

        assert_eq!(
            watcher_count.load(Ordering::Relaxed),
            0,
            "an open failure must not be counted as a live watcher"
        );
        assert!(
            !watched_paths
                .lock()
                .expect("watched paths lock")
                .contains(&PathBuf::from("/fake/event0")),
            "a retired watcher must free its path for the next rescan"
        );
    }

    #[test]
    fn a_successfully_opened_watcher_counts_as_alive_until_it_is_told_to_stop() {
        let source = Arc::new(ScriptedDeviceSource::new());
        source.set_device("/fake/event0", DeviceScript::Polls(Vec::new()));
        let trait_source: Arc<dyn InputDeviceSource> = source;
        let watcher_count = Arc::new(AtomicUsize::new(0));
        let watched_paths = Arc::new(Mutex::new(HashSet::new()));
        let last_activity_at = Arc::new(AtomicI64::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let handle = spawn_watcher(
            PathBuf::from("/fake/event0"),
            trait_source,
            Arc::clone(&running),
            Arc::clone(&last_activity_at),
            Arc::clone(&watcher_count),
            Arc::clone(&watched_paths),
        );

        wait_for(
            || watcher_count.load(Ordering::Relaxed) == 1,
            Duration::from_secs(1),
        );

        running.store(false, Ordering::Relaxed);
        handle.join().expect("watcher thread must not panic");

        assert_eq!(watcher_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_device_that_appears_after_start_is_watched() {
        let source = Arc::new(ScriptedDeviceSource::new());
        source.set_device("/fake/event0", DeviceScript::Polls(Vec::new()));
        let trait_source: Arc<dyn InputDeviceSource> =
            Arc::clone(&source) as Arc<dyn InputDeviceSource>;
        let running = Arc::new(AtomicBool::new(true));

        let activity = InputActivity::start_with(
            Arc::clone(&running),
            0,
            trait_source,
            Duration::from_millis(5),
        )
        .expect("starts with the one device already present");

        wait_for(
            || activity.observation().watchers_alive >= 1,
            Duration::from_secs(1),
        );

        // The second device is only discoverable from this point on: the daemon's initial
        // enumeration never saw it.
        source.set_device(
            "/fake/event1",
            DeviceScript::Polls(vec![ScriptedPoll::Activity]),
        );

        wait_for(
            || activity.observation().watchers_alive >= 2,
            Duration::from_secs(2),
        );
        wait_for(
            || activity.observation().last_activity_at > 0,
            Duration::from_secs(2),
        );

        running.store(false, Ordering::Relaxed);
    }

    #[test]
    fn a_watcher_retired_by_a_read_error_is_picked_up_again_by_the_next_enumeration() {
        let source = Arc::new(ScriptedDeviceSource::new());
        source.set_device(
            "/fake/event0",
            DeviceScript::Polls(vec![ScriptedPoll::Error]),
        );
        let trait_source: Arc<dyn InputDeviceSource> =
            Arc::clone(&source) as Arc<dyn InputDeviceSource>;
        let running = Arc::new(AtomicBool::new(true));

        let activity = InputActivity::start_with(
            Arc::clone(&running),
            0,
            trait_source,
            Duration::from_millis(5),
        )
        .expect("starts with the one device already present");

        // The initial watcher opens, hits the scripted read error on its first poll, and
        // retires; the defect this bead is about is that it would then stay retired forever.
        wait_for(
            || activity.observation().watchers_alive == 0,
            Duration::from_secs(1),
        );

        // The device recovers: the next rescan reopens the same path, now scripted to succeed.
        source.set_device(
            "/fake/event0",
            DeviceScript::Polls(vec![ScriptedPoll::Activity]),
        );

        wait_for(
            || activity.observation().last_activity_at > 0,
            Duration::from_secs(2),
        );

        running.store(false, Ordering::Relaxed);
    }

    #[test]
    fn an_observation_with_no_watchers_alive_is_not_observing() {
        let observation = InputObservation {
            last_activity_at: 1_000,
            watchers_alive: 0,
        };
        assert!(!observation.is_observing());

        let observation = InputObservation {
            last_activity_at: 1_000,
            watchers_alive: 1,
        };
        assert!(observation.is_observing());
    }
}
