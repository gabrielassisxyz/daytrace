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

    /// Whether a playing media item is excluded, matching each category against its own field.
    ///
    /// The desktop `should_skip` tests title terms AND domain terms against one text argument,
    /// which is right for a window title but wrong for media: a domain entry that excludes a
    /// bank would start matching an artist, and a title entry would start matching an address.
    /// Media checks by category instead: application entries against the normalized key, title
    /// entries against the title, each artist and the album, and domain entries against the
    /// address alone.
    pub fn should_skip_media(
        &self,
        player_key: &str,
        title: Option<&str>,
        artists: &[String],
        album: Option<&str>,
        item_url: Option<&str>,
    ) -> bool {
        let key = player_key.to_ascii_lowercase();
        if self.app_classes.iter().any(|blocked| key.contains(blocked)) {
            return true;
        }

        let title_matches = |text: &str| {
            let text = text.to_ascii_lowercase();
            self.title_terms
                .iter()
                .any(|blocked| text.contains(blocked))
        };

        if title.is_some_and(&title_matches)
            || artists.iter().any(|artist| title_matches(artist))
            || album.is_some_and(&title_matches)
        {
            return true;
        }

        item_url.is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            self.domain_terms
                .iter()
                .any(|blocked| value.contains(blocked))
        })
    }
}

/// The suffixes that mark a query or fragment parameter as carrying a secret. Shared by the
/// free-text scan (`redact_title`) and the address scan (`redact_address`) so the two agree
/// on which names carry a secret: both match a name that ends in one of these,
/// case-insensitively.
const SENSITIVE_KEYWORD_SUFFIXES: [&str; 5] = ["token", "secret", "key", "code", "password"];

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
        // Escape each keyword: a future keyword carrying a regex metacharacter must be
        // matched literally, not change the pattern's meaning.
        let keywords = SENSITIVE_KEYWORD_SUFFIXES
            .iter()
            .map(|suffix| regex::escape(suffix))
            .collect::<Vec<_>>()
            .join("|");
        Regex::new(&format!(r"(?i)([A-Za-z0-9_-]*(?:{keywords}))=([^\s&]+)"))
            .expect("secret regex should compile")
    });

    let without_urls = url_re.replace_all(title, "[redacted-url]");
    secret_re
        .replace_all(&without_urls, "$1=[redacted]")
        .to_string()
}

/// Redacts a credential embedded in the authority, plus the values of sensitive query and
/// fragment parameters, keeping everything else byte for byte.
///
/// `redact_title` cannot serve here: its URL regex replaces a whole address with
/// `[redacted-url]`, which on a field whose entire value is the address would say a track
/// was played and refuse to say which. This scan keeps the address and replaces only the
/// userinfo of its authority (`user:pass@host`) and the values of parameters whose name says
/// they carry a secret, so the stored string still names what was played without carrying the
/// credentials that reached it.
#[allow(dead_code)] // not yet called: the media capture path will use it once media is polled
pub fn redact_address(address: &str) -> String {
    let Some(scheme_end) = uri_scheme_end(address) else {
        return address.to_string();
    };
    let address = redact_userinfo(address, scheme_end);
    match address.split_once('#') {
        Some((main, fragment)) => format!("{}#{}", redact_query(main), redact_fragment(fragment)),
        None => redact_query(&address),
    }
}

/// The byte index of the scheme-terminating `:`, if `value` starts with a valid URI scheme
/// (`[A-Za-z][A-Za-z0-9+.-]*:`). A relative or malformed value has none, and is left alone
/// rather than having its `?`-suffixed text read as a query.
fn uri_scheme_end(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    for (index, &byte) in bytes.iter().enumerate().skip(1) {
        if byte == b':' {
            return Some(index);
        }
        if !(byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'-' || byte == b'.') {
            return None;
        }
    }
    None
}

/// Replaces a credential in the authority (`user:pass@host`, `user@host`) with `[redacted]`
/// on each side of the colon, preserving the colon only when the input carried one.
///
/// The authority is the span between the scheme's `//` and the next `/`, `?`, `#`, or the end
/// of the address. An `@` outside that span belongs to a path segment or a query value, not to
/// a credential; splitting on any `@` in the whole address would treat it as one anyway, which
/// is exactly the over-redaction this scoping avoids.
fn redact_userinfo(address: &str, scheme_end: usize) -> String {
    let after_scheme = &address[scheme_end + 1..];
    if !after_scheme.starts_with("//") {
        return address.to_string();
    }

    let authority_start = scheme_end + 3;
    let rest = &address[authority_start..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];

    // The LAST `@` in the authority, because that is where the host begins: an unencoded `@`
    // inside a credential is malformed but arrives anyway, and splitting on the first one
    // leaves everything between the two in the clear. `https://user@evil.test:pass@host.test/`
    // stored as `https://[redacted]@evil.test:pass@host.test/` keeps the password.
    let Some((userinfo, _host)) = authority.rsplit_once('@') else {
        return address.to_string();
    };

    let redacted_userinfo = if userinfo.contains(':') {
        "[redacted]:[redacted]"
    } else {
        "[redacted]"
    };

    format!(
        "{}{redacted_userinfo}@{}",
        &address[..authority_start],
        &rest[userinfo.len() + 1..]
    )
}

/// Redacts the query of a URI part: everything after the first `?`, if any.
fn redact_query(part: &str) -> String {
    match part.split_once('?') {
        Some((base, query)) => format!("{base}?{}", redact_parameters(query)),
        None => part.to_string(),
    }
}

/// Redacts a fragment. A fragment is a sequence of parameter zones separated by `?` (a routed
/// query) or `#` (a nested fragment); each zone is scanned as a parameter list, so a sensitive
/// parameter before a later separator is not left standing.
fn redact_fragment(fragment: &str) -> String {
    let mut redacted = String::with_capacity(fragment.len());
    let mut rest = fragment;
    while let Some(index) = rest.find(['?', '#']) {
        let (zone, tail) = rest.split_at(index);
        redacted.push_str(&redact_parameters(zone));
        redacted.push(rest.as_bytes()[index] as char);
        rest = &tail[1..];
    }
    redacted.push_str(&redact_parameters(rest));
    redacted
}

fn redact_parameters(parameters: &str) -> String {
    parameters
        .split('&')
        .map(redact_parameter)
        .collect::<Vec<_>>()
        .join("&")
}

fn redact_parameter(parameter: &str) -> String {
    match parameter.split_once('=') {
        Some((name, _)) if is_sensitive_parameter_name(name) => format!("{name}=[redacted]"),
        _ => parameter.to_string(),
    }
}

fn is_sensitive_parameter_name(name: &str) -> bool {
    let decoded = percent_decode(name);
    let lower = decoded.to_ascii_lowercase();
    SENSITIVE_KEYWORD_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// Decodes `%XX` escapes in a parameter name, for the comparison only. The stored address
/// keeps its original spelling, because the address that gets stored has to stay the address
/// that was played.
fn percent_decode(input: &str) -> String {
    let mut decoded = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            decoded.push(high << 4 | low);
            i += 3;
            continue;
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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
    use super::{Blacklist, DEFAULT_RETENTION_DAYS, redact_address, redact_title, retention_days};

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
    fn redact_address_keeps_an_address_without_sensitive_parameters_unchanged() {
        for address in [
            "https://open.spotify.com/track/6IgfQZGOB4ZdlzG19MvYtX",
            "file:///home/user/Videos/talk.mkv",
            "spotify:track:6IgfQZGOB4ZdlzG19MvYtX",
        ] {
            assert_eq!(redact_address(address), address);
        }
    }

    #[test]
    fn redact_address_strips_only_the_sensitive_parameter_values() {
        let cases = [
            (
                "https://host.test/cb?access_token=abc&list=RD123",
                "https://host.test/cb?access_token=[redacted]&list=RD123",
            ),
            (
                "https://host.test/cb?a=1#access_token=abc",
                "https://host.test/cb?a=1#access_token=[redacted]",
            ),
            (
                "https://host.test/#/callback?access_token=abc",
                "https://host.test/#/callback?access_token=[redacted]",
            ),
            (
                "https://host.test/cb?access_token=abc&api_key=xyz&list=RD123",
                "https://host.test/cb?access_token=[redacted]&api_key=[redacted]&list=RD123",
            ),
            (
                "https://host.test/cb?access%5Ftoken=abc",
                "https://host.test/cb?access%5Ftoken=[redacted]",
            ),
            (
                "file:///home/user/Videos/talk.mkv?key=abc",
                "file:///home/user/Videos/talk.mkv?key=[redacted]",
            ),
            (
                "spotify:track:abc?token=xyz#frag?code=123",
                "spotify:track:abc?token=[redacted]#frag?code=[redacted]",
            ),
            (
                "https://host.test/cb#access_token=abc&next=/home?tab=1",
                "https://host.test/cb#access_token=[redacted]&next=/home?tab=1",
            ),
            (
                "https://host.test/cb?a=1#token=x&next=/p?q=1",
                "https://host.test/cb?a=1#token=[redacted]&next=/p?q=1",
            ),
            (
                "https://host.test/cb#a=1#access_token=abc",
                "https://host.test/cb#a=1#access_token=[redacted]",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_address(input), expected);
        }
    }

    #[test]
    fn redact_address_matches_exact_spellings_case_insensitively() {
        for name in [
            "token", "key", "secret", "password", "code", "TOKEN", "Secret", "CODE",
        ] {
            let input = format!("https://host.test/cb?{name}=abc");
            assert_eq!(
                redact_address(&input),
                format!("https://host.test/cb?{name}=[redacted]")
            );
        }
    }

    #[test]
    fn redact_address_matches_prefixed_spellings() {
        // A word boundary cannot follow an underscore, so the prefixed spellings are the
        // ones a naive guard misses.
        for name in [
            "access_token",
            "api_key",
            "client_secret",
            "user_password",
            "auth_code",
        ] {
            let input = format!("https://host.test/cb?{name}=abc");
            assert_eq!(
                redact_address(&input),
                format!("https://host.test/cb?{name}=[redacted]")
            );
        }
    }

    #[test]
    fn redact_address_matches_dotted_names_like_the_free_text_scan() {
        // The free-text regex is unanchored, so a dotted name like `auth.token` matches by
        // its suffix. The address scan must agree, or the two scans drift apart.
        for name in ["auth.token", "ns.access_token", "oauth.token"] {
            let input = format!("https://host.test/cb?{name}=abc");
            assert_eq!(
                redact_address(&input),
                format!("https://host.test/cb?{name}=[redacted]")
            );
        }
    }

    #[test]
    fn redact_address_redacts_a_credential_in_the_authority() {
        let cases = [
            (
                "https://user:s3cret@host.test/path",
                "https://[redacted]:[redacted]@host.test/path",
            ),
            // A password-less userinfo does not gain a colon it never had.
            (
                "https://user@host.test/path",
                "https://[redacted]@host.test/path",
            ),
            // An explicit, empty password is still a colon the input carried, so the shape
            // (one colon) survives: both halves become `[redacted]` exactly as a non-empty
            // password would, rather than treating "present but empty" as "absent".
            (
                "https://user:@host.test/path",
                "https://[redacted]:[redacted]@host.test/path",
            ),
            // Percent-encoded credential text: the marker replaces it outright, so nothing
            // about the original encoding is left to preserve.
            (
                "https://%75ser:%70a%24s@host.test/path",
                "https://[redacted]:[redacted]@host.test/path",
            ),
            // A credential and a sensitive query parameter, redacted in one pass.
            (
                "https://user:s3cret@host.test/cb?access_token=abc",
                "https://[redacted]:[redacted]@host.test/cb?access_token=[redacted]",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_address(input), expected);
        }
    }

    #[test]
    fn redact_address_redacts_a_credential_carrying_an_unencoded_at_sign() {
        // The host is what follows the LAST `@` of the authority, so everything before it is
        // credential text. Splitting on the first `@` instead leaves the password readable.
        assert_eq!(
            redact_address("https://user@evil.test:pass@host.test/p"),
            "https://[redacted]:[redacted]@host.test/p"
        );
        assert_eq!(
            redact_address("https://a@b@host.test/p"),
            "https://[redacted]@host.test/p"
        );
    }

    #[test]
    fn redact_address_does_not_treat_an_out_of_authority_at_sign_as_a_credential_delimiter() {
        // `@` in the path and in a query value. Splitting the whole address on the first or
        // the last `@` would catch one of these and corrupt the address; scoping the search
        // to the authority catches neither.
        let address = "https://host.test/p@th?to=a@b.test";
        assert_eq!(redact_address(address), address);
    }

    #[test]
    fn redact_address_without_userinfo_is_untouched() {
        // Over-redaction is exactly the failure this scan exists to avoid: an address with no
        // credential must come back byte-identical, never gaining a `[redacted]` it never
        // carried, including one with no authority at all.
        for address in [
            "https://host.test/path",
            "file:///home/user/Videos/talk.mkv",
        ] {
            assert_eq!(redact_address(address), address);
        }

        // A sensitive query parameter is still redacted as before, but the address had no
        // `@` anywhere, so the output must not gain one: the query scan and the userinfo
        // scan run over the same address without inventing a credential for each other.
        let with_sensitive_query = "https://host.test/cb?access_token=abc";
        let output = redact_address(with_sensitive_query);
        assert_eq!(output, "https://host.test/cb?access_token=[redacted]");
        assert!(!output.contains('@'), "invented a credential: {output}");
    }

    #[test]
    fn redact_address_leaves_relative_or_malformed_values_unchanged() {
        for value in [
            "relative/path?token=abc",
            "not an address",
            "//host.test/path?token=abc",
            "",
        ] {
            assert_eq!(redact_address(value), value);
        }
    }

    #[test]
    fn redact_address_preserves_every_byte_outside_sensitive_values() {
        // Generated addresses: every combination of scheme, userinfo, path, query and
        // fragment, with a sensitive and a benign parameter interleaved. The invariant is
        // that the output differs from the input only where a sensitive value stood, and
        // as of this bead userinfo counts as a sensitive value too.
        let schemes = ["https://", "spotify:", "file:///"];
        // Only `https://`'s paths are host-shaped. Inserting userinfo before a `spotify:` or
        // `file:///` path would land in the path (or, for `file:///`, after an authority the
        // scheme's own third slash already closed empty) rather than in an authority, so a
        // non-empty userinfo is exercised for `https://` only; the other two keep the single
        // no-userinfo case they already had.
        let userinfos = ["", "user@", "user:pass@"];
        let paths = [
            "host.test/cb",
            "open.spotify.com/track/abc",
            "home/user/Videos/talk.mkv",
        ];
        let sensitive = [
            "access_token",
            "api_key",
            "client_secret",
            "user_password",
            "auth_code",
        ];
        let benign = ["list", "v", "t", "id"];
        // Both fragment shapes, plus the flat list with a later `?` that used to leave the
        // sensitive parameter standing. Each pair is (input fragment, fragment with the
        // sensitive value emptied).
        let fragments = [
            ("frag?{s}=leak", "frag?{s}="),
            ("{s}=leak", "{s}="),
            ("{s}=leak&next=/home?tab=1", "{s}=&next=/home?tab=1"),
        ];

        for scheme in schemes {
            for userinfo in userinfos {
                if !userinfo.is_empty() && scheme != "https://" {
                    continue;
                }
                // What a redacted userinfo leaves behind once its `[redacted]` markers are
                // stripped for the byte-preservation check below: the colon only if the
                // input carried one, then the `@` every non-empty userinfo carries.
                let userinfo_shape = if userinfo.is_empty() {
                    ""
                } else if userinfo.contains(':') {
                    ":@"
                } else {
                    "@"
                };
                let userinfo_marker_count = if userinfo.is_empty() {
                    0
                } else if userinfo.contains(':') {
                    2
                } else {
                    1
                };
                for path in paths {
                    for sensitive_name in sensitive {
                        for benign_name in benign {
                            for (fragment, emptied) in fragments {
                                let fragment = fragment.replace("{s}", sensitive_name);
                                let emptied = emptied.replace("{s}", sensitive_name);
                                let input = format!(
                                    "{scheme}{userinfo}{path}?{benign_name}=keep&{sensitive_name}=drop#{fragment}"
                                );
                                let output = redact_address(&input);

                                // The sensitive values are gone, replaced by the marker.
                                assert!(!output.contains("=drop"), "value leaked: {output}");
                                assert!(!output.contains("=leak"), "value leaked: {output}");
                                assert!(
                                    userinfo.is_empty() || !output.contains(userinfo),
                                    "credential leaked: {output}"
                                );
                                assert_eq!(
                                    output.matches("[redacted]").count(),
                                    2 + userinfo_marker_count,
                                    "{output}"
                                );

                                // Every byte outside a sensitive value survives, in order:
                                // removing the markers leaves the input with the sensitive
                                // values emptied.
                                let expected = format!(
                                    "{scheme}{userinfo_shape}{path}?{benign_name}=keep&{sensitive_name}=#{emptied}"
                                );
                                assert_eq!(output.replace("[redacted]", ""), expected, "{output}");
                            }
                        }
                    }
                }
            }
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

    #[test]
    fn media_blacklists_match_by_category() {
        let blacklist = Blacklist::new(
            vec!["spotify".to_string()],
            vec!["secret".to_string()],
            vec!["bank.test".to_string()],
        );

        // Application entries match the normalized key.
        assert!(blacklist.should_skip_media("spotify", None, &[], None, None));
        // Title entries match the title, each artist and the album.
        assert!(blacklist.should_skip_media("vlc", Some("secret track"), &[], None, None));
        assert!(blacklist.should_skip_media(
            "vlc",
            None,
            &["secret artist".to_string()],
            None,
            None
        ));
        assert!(blacklist.should_skip_media("vlc", None, &[], Some("secret album"), None));
        // Domain entries match the address alone.
        assert!(blacklist.should_skip_media("vlc", None, &[], None, Some("https://bank.test/x")));
        // A domain term appearing only in an artist does not skip the player.
        assert!(!blacklist.should_skip_media("vlc", None, &["bank.test".to_string()], None, None));
        // A title term appearing only in the address does not skip the player.
        assert!(!blacklist.should_skip_media(
            "vlc",
            None,
            &[],
            None,
            Some("https://open.spotify.com/secret")
        ));
    }

    #[test]
    fn a_term_matching_across_fields_does_not_skip() {
        let blacklist = Blacklist::new(Vec::new(), vec!["x y".to_string()], Vec::new());
        // The title ends in x and the artist begins with y, so "x y" spans the boundary between
        // two fields and must not match either one.
        assert!(!blacklist.should_skip_media(
            "vlc",
            Some("foo x"),
            &["y bar".to_string()],
            None,
            None,
        ));
    }
}
