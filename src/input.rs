use evdev::{Device, EventType};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread;
use std::time::Duration;

pub struct InputActivity {
    last_activity_at: Arc<AtomicI64>,
}

impl InputActivity {
    pub fn start(running: Arc<AtomicBool>, initial_timestamp: i64) -> Result<Self, String> {
        let last_activity_at = Arc::new(AtomicI64::new(initial_timestamp));
        let devices = discover_activity_devices("/dev/input");
        if devices.is_empty() {
            return Err(
                "no readable input devices found under /dev/input; AFK tracking needs input-event read access"
                    .to_string(),
            );
        }

        for path in devices {
            let last_activity_at = Arc::clone(&last_activity_at);
            let running = Arc::clone(&running);
            thread::spawn(move || watch_device(path, running, last_activity_at));
        }

        Ok(Self { last_activity_at })
    }

    pub fn last_activity_at(&self) -> i64 {
        self.last_activity_at.load(Ordering::Relaxed)
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

fn watch_device(path: PathBuf, running: Arc<AtomicBool>, last_activity_at: Arc<AtomicI64>) {
    let Ok(mut device) = Device::open(path) else {
        return;
    };
    if device.set_nonblocking(true).is_err() {
        return;
    }

    while running.load(Ordering::Relaxed) {
        match device.fetch_events() {
            Ok(events) => {
                if events.into_iter().any(is_activity_event) {
                    last_activity_at.store(crate::timeline::unix_now(), Ordering::Relaxed);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return,
        }
    }
}

fn is_activity_event(event: evdev::InputEvent) -> bool {
    matches!(
        event.event_type(),
        EventType::KEY | EventType::RELATIVE | EventType::ABSOLUTE
    )
}

#[cfg(test)]
mod tests {
    use super::discover_activity_devices;

    #[test]
    fn missing_input_dir_has_no_devices() {
        assert!(discover_activity_devices("/path/that/does/not/exist").is_empty());
    }
}
