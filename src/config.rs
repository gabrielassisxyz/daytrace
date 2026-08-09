use regex::Regex;
use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub db_path: PathBuf,
    pub secure_data_dir: Option<PathBuf>,
    pub idle_after: Duration,
    pub poll_interval: Duration,
    pub retention_days: u32,
    pub blacklist: Blacklist,
}

/// How many days of activity the retention window keeps by default.
///
/// A quarter is long enough that a bare `daytrace prune` cannot surprise anyone who wanted
/// last month back, and short enough that the store is no longer a permanent record of every
/// window that ever held focus. Nothing enforces it on its own: it is the window `prune`
/// applies when asked, so the value that matters is the one a reader can predict before
/// running an irreversible command.
const DEFAULT_RETENTION_DAYS: u32 = 90;

#[derive(Clone, Debug, Default)]
pub struct Blacklist {
    app_classes: Vec<String>,
    title_terms: Vec<String>,
    domain_terms: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let db_location = db_path_from_env()?;
        Ok(Self {
            db_path: db_location.path,
            secure_data_dir: db_location.secure_data_dir,
            idle_after: duration_from_env("DAYTRACE_IDLE_AFTER_SECONDS", 300)?,
            poll_interval: duration_from_env("DAYTRACE_POLL_SECONDS", 1)?,
            retention_days: retention_days(env::var("DAYTRACE_RETENTION_DAYS").ok().as_deref())?,
            blacklist: Blacklist::from_env(),
        })
    }
}

impl Blacklist {
    pub fn new(
        app_classes: Vec<String>,
        title_terms: Vec<String>,
        domain_terms: Vec<String>,
    ) -> Self {
        Self {
            app_classes: normalize_list(app_classes),
            title_terms: normalize_list(title_terms),
            domain_terms: normalize_list(domain_terms),
        }
    }

    pub fn from_env() -> Self {
        Self::new(
            list_from_env("DAYTRACE_BLACKLIST_APPS"),
            list_from_env("DAYTRACE_BLACKLIST_TITLES"),
            list_from_env("DAYTRACE_BLACKLIST_DOMAINS"),
        )
    }

    pub fn should_skip(&self, app_class: Option<&str>, title: Option<&str>) -> bool {
        let app_class = app_class.map(str::to_ascii_lowercase);
        let title = title.map(str::to_ascii_lowercase);

        // Substring, not equality: compositors report reverse-DNS classes, so an entry has to
        // match `org.keepassxc.KeePassXC` when it says `keepassxc`. Requiring the whole string
        // meant a blacklisted password manager was recorded anyway, in silence.
        app_class.as_deref().is_some_and(|value| {
            self.app_classes
                .iter()
                .any(|blocked| value.contains(blocked))
        }) || title.as_deref().is_some_and(|value| {
            self.title_terms
                .iter()
                .any(|blocked| value.contains(blocked))
                || self
                    .domain_terms
                    .iter()
                    .any(|blocked| value.contains(blocked))
        })
    }
}

pub fn redact_title(title: &str) -> String {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    static SECRET_RE: OnceLock<Regex> = OnceLock::new();

    let url_re = URL_RE.get_or_init(|| {
        Regex::new(r"(?i)\b((https?://|www\.)[^\s]+)").expect("URL regex should compile")
    });
    // The keyword may carry a prefix (`access_token`, `api_key`). A word boundary cannot
    // express that: `_` is itself a word character, so `\b` never matched after one and the
    // most common real spellings passed through unredacted. Over-matching a word that merely
    // ends in a keyword is the safe direction for a guard like this.
    let secret_re = SECRET_RE.get_or_init(|| {
        Regex::new(r"(?i)([A-Za-z0-9_-]*(?:token|secret|key|code|password))=([^\s&]+)")
            .expect("secret regex should compile")
    });

    let without_urls = url_re.replace_all(title, "[redacted-url]");
    secret_re
        .replace_all(&without_urls, "$1=[redacted]")
        .to_string()
}

struct DbLocation {
    path: PathBuf,
    secure_data_dir: Option<PathBuf>,
}

fn db_path_from_env() -> Result<DbLocation, String> {
    // `var_os` rather than `var`: a path is bytes on this platform, and a value that fails
    // UTF-8 decoding is still a real, usable path. Reading it with `var` would treat a
    // non-UTF-8 override the same as an unset one and silently open the default store
    // instead, which is the one outcome this override exists to prevent.
    if let Some(path) = env::var_os("DAYTRACE_DB_PATH") {
        return Ok(DbLocation {
            path: PathBuf::from(path),
            secure_data_dir: None,
        });
    }

    let data_home = env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map_err(|_| "DAYTRACE_DB_PATH or HOME must be set".to_string())?;

    let secure_data_dir = data_home.join("daytrace");
    Ok(DbLocation {
        path: secure_data_dir.join("daytrace.db"),
        secure_data_dir: Some(secure_data_dir),
    })
}

fn duration_from_env(name: &str, default_seconds: u64) -> Result<Duration, String> {
    let seconds = match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| format!("{name} must be an integer number of seconds"))?,
        Err(_) => default_seconds,
    };
    Ok(Duration::from_secs(seconds.max(1)))
}

/// The retention window in days, from `DAYTRACE_RETENTION_DAYS` or the documented default.
///
/// A window of zero is rejected rather than clamped, which is where this departs from the
/// other numeric settings. Raising a zero poll interval to one second changes nothing a user
/// can lose; reading a zero retention window as one day would silently disagree with a
/// variable that, as written, means "keep only today" and would take everything else with it.
fn retention_days(configured: Option<&str>) -> Result<u32, String> {
    // An empty value means unset, not invalid. `Environment=DAYTRACE_RETENTION_DAYS=` in a
    // systemd drop-in produces exactly this, and every command reads the whole configuration, so
    // refusing it would stop capture over a setting only `prune` ever uses.
    let value = configured.map(str::trim).filter(|value| !value.is_empty());
    let Some(value) = value else {
        return Ok(DEFAULT_RETENTION_DAYS);
    };

    match value.parse::<u32>() {
        Ok(0) | Err(_) => {
            Err("DAYTRACE_RETENTION_DAYS must be a whole number of days, at least 1".to_string())
        }
        Ok(days) => Ok(days),
    }
}

fn list_from_env(name: &str) -> Vec<String> {
    env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn normalize_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Blacklist, DEFAULT_RETENTION_DAYS, redact_title, retention_days};

    #[test]
    fn an_unset_retention_window_falls_back_to_the_documented_default() {
        assert_eq!(
            retention_days(None).expect("an unset window has a default"),
            DEFAULT_RETENTION_DAYS
        );
        assert_eq!(
            DEFAULT_RETENTION_DAYS, 90,
            "the default is documented in the README, so a change to it is a change to a \
             published promise rather than to a constant"
        );
    }

    #[test]
    fn a_configured_retention_window_is_read_as_days() {
        assert_eq!(retention_days(Some("14")).expect("a valid window"), 14);
        assert_eq!(
            retention_days(Some(" 14 ")).expect("a padded window"),
            14,
            "a value that arrives with whitespace, as one written in a systemd drop-in can, \
             must not fall back to the default in silence"
        );
    }

    #[test]
    fn an_empty_retention_window_reads_as_unset_rather_than_as_an_error() {
        for value in ["", "  "] {
            assert_eq!(
                retention_days(Some(value)).unwrap_or_else(|error| panic!(
                    "an empty setting must not stop every command, including capture: {error}"
                )),
                DEFAULT_RETENTION_DAYS
            );
        }
    }

    #[test]
    fn a_retention_window_that_would_delete_everything_is_refused() {
        for value in ["0", "-1", "ninety", "90 days"] {
            let error = retention_days(Some(value))
                .expect_err(&format!("{value} is not a number of days to keep"));
            assert!(
                error.contains("at least 1"),
                "{value} was rejected without saying what a window should be: {error}"
            );
        }
    }

    #[test]
    fn redacts_urls_and_token_values_from_titles() {
        let title = "Issue https://example.test/path?token=abc token=secret key=value";
        assert_eq!(
            redact_title(title),
            "Issue [redacted-url] token=[redacted] key=[redacted]"
        );
    }

    #[test]
    fn redacts_secret_keys_that_carry_a_prefix() {
        for title in [
            "callback access_token=abc123",
            "request api_key=sk-live-9999",
            "form user_password=hunter2",
        ] {
            let redacted = redact_title(title);
            assert!(
                redacted.ends_with("=[redacted]"),
                "expected {title} to be redacted, got {redacted}"
            );
        }
    }

    #[test]
    fn blacklist_matches_reverse_dns_application_classes() {
        let blacklist = Blacklist::new(vec!["keepassxc".to_string()], Vec::new(), Vec::new());

        assert!(blacklist.should_skip(Some("org.keepassxc.KeePassXC"), Some("Passwords")));
        assert!(!blacklist.should_skip(Some("com.mitchellh.ghostty"), Some("tmux")));
    }

    #[test]
    fn blacklist_matches_app_class_and_title_terms() {
        let blacklist = Blacklist::new(
            vec!["keepassxc".to_string()],
            vec!["private".to_string()],
            vec!["bank.test".to_string()],
        );

        assert!(blacklist.should_skip(Some("KeePassXC"), None));
        assert!(blacklist.should_skip(None, Some("Private Browsing")));
        assert!(blacklist.should_skip(None, Some("https://bank.test/session")));
        assert!(!blacklist.should_skip(Some("Ghostty"), Some("tmux")));
    }
}
