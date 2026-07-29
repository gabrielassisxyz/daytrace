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
    pub blacklist: Blacklist,
}

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

        app_class
            .as_deref()
            .is_some_and(|value| self.app_classes.iter().any(|blocked| blocked == value))
            || title.as_deref().is_some_and(|value| {
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
    let secret_re = SECRET_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(token|secret|key|code|password)=([^\s&]+)")
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
    if let Ok(path) = env::var("DAYTRACE_DB_PATH") {
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
    use super::{Blacklist, redact_title};

    #[test]
    fn redacts_urls_and_token_values_from_titles() {
        let title = "Issue https://example.test/path?token=abc token=secret key=value";
        assert_eq!(
            redact_title(title),
            "Issue [redacted-url] token=[redacted] key=[redacted]"
        );
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
