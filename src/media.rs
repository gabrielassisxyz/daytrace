//! Reading the MPRIS players that are currently playing, over the user D-Bus.
//!
//! The desktop layer says which window held focus; this says what was playing behind it,
//! including in the background. It is read through `busctl --user`, the same subprocess shape
//! the compositor is read through, so a failing and then recovering bus can be staged in a
//! test the way a failing compositor is.
//!
//! Two operations, two failure kinds. Discovery lists the bus, then one property query runs
//! per player, and they are separate operations with a real window between them: a browser
//! can quit in that window while Spotify keeps playing. A failure to list is a whole-source
//! error; a property query that fails costs that one player and leaves the others.

use crate::config::Blacklist;
use serde::Deserialize;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long discovery plus every property query may take together.
///
/// The daemon's loop is synchronous and sleeps only after the poll's work, so a `busctl` that
/// hangs would stop desktop capture while the process stays alive and silent. One fixed budget
/// covers discovery and every property query: each child gets only what remains, so a player
/// count that grows with every browser instance cannot multiply the bound.
const MEDIA_POLL_BUDGET: Duration = Duration::from_secs(1);

/// A player that is currently playing, with the metadata the boundary keeps.
///
/// `mpris:artUrl` and `mpris:trackid` are deliberately absent: the first is a path into `/tmp`
/// or a cover image, and the second is redundant with the address where it is legible and
/// opaque where it is not.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayingMedia {
    /// The normalized player key, e.g. `brave` or `spotify`.
    pub player_key: String,
    /// The full bus name discovery returned, e.g. `org.mpris.MediaPlayer2.brave.instance834645`.
    pub bus_name: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub item_url: Option<String>,
}

/// What one discovered player turned out to be doing.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlayerOutcome {
    /// The player is playing, with its metadata.
    Playing(PlayingMedia),
    /// The player is present but not playing.
    NotPlaying,
    /// The property query for this player failed or returned unparseable output.
    Failed(String),
}

/// The media boundary the capture loop observes through.
///
/// Exists so the loop's failure handling can be driven by a fake, the way the compositor's is.
///
/// The capture loop that polls this is a later bead, so the trait, its client and the model
/// types are dead code in the binary until then; the allows below are removed when the loop
/// lands.
#[allow(dead_code)]
pub trait MediaSource {
    fn poll(&self, blacklist: &Blacklist) -> Result<Vec<PlayerOutcome>, String>;
}

/// A bus name and its normalized key.
#[derive(Clone, Debug, Eq, PartialEq)]
struct BusName {
    full: String,
    key: String,
}

/// The real client, reading the user bus through `busctl`.
#[allow(dead_code)]
pub struct BusctlClient {
    command: String,
}

#[allow(dead_code)]
impl BusctlClient {
    pub fn new() -> Self {
        Self {
            command: "busctl".to_string(),
        }
    }

    /// A client that runs a different command, so a test can stage a fake `busctl`.
    #[cfg(test)]
    fn with_command(command: String) -> Self {
        Self { command }
    }
}

impl MediaSource for BusctlClient {
    fn poll(&self, blacklist: &Blacklist) -> Result<Vec<PlayerOutcome>, String> {
        // One deadline for the whole poll, not one per invocation: a deadline per invocation
        // lets discovery plus N property queries stall the synchronous loop for (N+1) times the
        // budget, and N grows with every browser instance.
        let deadline = Instant::now() + MEDIA_POLL_BUDGET;
        let list_output = run_bounded(&mut discovery_command(&self.command), deadline)?;
        let bus_names = parse_discovery(&String::from_utf8_lossy(&list_output));

        let mut outcomes = Vec::with_capacity(bus_names.len());
        for bus_name in bus_names {
            let outcome = match run_bounded(
                &mut property_command(&self.command, &bus_name.full),
                deadline,
            ) {
                Ok(output) => {
                    parse_properties(&String::from_utf8_lossy(&output), &bus_name, blacklist)
                }
                Err(error) => PlayerOutcome::Failed(error),
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }
}

/// The `busctl` invocation that lists the bus.
fn discovery_command(command: &str) -> Command {
    let mut cmd = Command::new(command);
    cmd.args(["--user", "list", "--no-pager", "--no-legend", "--full"]);
    cmd
}

/// The `busctl` invocation that reads one player's status and metadata.
fn property_command(command: &str, bus_name: &str) -> Command {
    let mut cmd = Command::new(command);
    cmd.args([
        "--user",
        "--json=short",
        "get-property",
        bus_name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
        "PlaybackStatus",
        "Metadata",
    ]);
    cmd
}

/// The proxy that mirrors the last active player, excluded by bus name.
///
/// It republishes the mirrored player's metadata verbatim and reports that player's `Identity`
/// as its own, so a dedup by identity or by metadata does not see a duplicate at all.
const PLAYERCTLD_KEY: &str = "playerctld";

/// Parse the discovery listing into the MPRIS bus names it carries.
///
/// Reads only the first full-name column and accepts only the exact `org.mpris.MediaPlayer2.`
/// prefix, so a unique connection name like `:1.42` and MPRIS text in a later column select
/// nothing.
fn parse_discovery(output: &str) -> Vec<BusName> {
    output
        .lines()
        .filter_map(|line| {
            let full = line.split_whitespace().next()?;
            let key = normalize_bus_name(full)?;
            if key == PLAYERCTLD_KEY {
                return None;
            }
            Some(BusName {
                full: full.to_string(),
                key,
            })
        })
        .collect()
}

/// Normalize a full bus name to its stable player key.
///
/// Strips the exact `org.mpris.MediaPlayer2.` prefix, then a trailing `.instance<pid>`, which a
/// Chromium-family name carries and which is not stable across restarts. Every earlier byte is
/// preserved, so a stable name containing dots keeps them.
fn normalize_bus_name(full: &str) -> Option<String> {
    let rest = full.strip_prefix("org.mpris.MediaPlayer2.")?;
    let key = match rest.find(".instance") {
        Some(index)
            if rest[index + ".instance".len()..]
                .bytes()
                .all(|b| b.is_ascii_digit()) =>
        {
            &rest[..index]
        }
        _ => rest,
    };
    Some(key.to_string())
}

/// One `busctl --json=short` value: a type tag and the JSON it describes.
#[derive(Debug, Deserialize)]
struct BusctlValue {
    #[serde(rename = "type")]
    type_tag: String,
    data: serde_json::Value,
}

impl BusctlValue {
    fn as_string(&self) -> Option<&str> {
        if self.type_tag != "s" {
            return None;
        }
        self.data.as_str()
    }

    fn as_string_array(&self) -> Option<Vec<&str>> {
        if self.type_tag != "as" {
            return None;
        }
        self.data
            .as_array()
            .map(|array| array.iter().filter_map(serde_json::Value::as_str).collect())
    }
}

/// The metadata dictionary line: a `a{sv}` map of string keys to variant values.
#[derive(Debug, Deserialize)]
struct MetadataEnvelope {
    #[serde(rename = "type")]
    type_tag: String,
    data: HashMap<String, BusctlValue>,
}

/// Parse one player's property output into its outcome.
///
/// `PlaybackStatus` is the admission rule: only `Playing` counts, and a missing or wrongly
/// typed status is a failure for that player, because it is the evidence that the player is
/// playing at all. Metadata is best-effort: a playing player with no metadata is still returned
/// by name.
fn parse_properties(output: &str, bus_name: &BusName, blacklist: &Blacklist) -> PlayerOutcome {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());

    let status = match lines.next() {
        Some(line) => match serde_json::from_str::<BusctlValue>(line) {
            Ok(value) => value,
            Err(error) => {
                return PlayerOutcome::Failed(format!(
                    "{} returned unparseable PlaybackStatus: {error}",
                    bus_name.full
                ));
            }
        },
        None => {
            return PlayerOutcome::Failed(format!("{} returned no PlaybackStatus", bus_name.full));
        }
    };

    let status = match status.as_string() {
        Some(value) => value,
        None => {
            return PlayerOutcome::Failed(format!(
                "{} PlaybackStatus is not a string",
                bus_name.full
            ));
        }
    };

    if status != "Playing" {
        return PlayerOutcome::NotPlaying;
    }

    let metadata = match lines.next() {
        Some(line) => serde_json::from_str::<MetadataEnvelope>(line)
            .ok()
            .filter(|envelope| envelope.type_tag == "a{sv}")
            .map(|envelope| envelope.data)
            .unwrap_or_default(),
        None => HashMap::new(),
    };

    let title = string_field(&metadata, "xesam:title", &bus_name.full);
    let artists = artist_array(&metadata, &bus_name.full);
    let album = string_field(&metadata, "xesam:album", &bus_name.full);
    let item_url = string_field(&metadata, "xesam:url", &bus_name.full);

    if blacklist.should_skip_media(
        &bus_name.key,
        title.as_deref(),
        &artists,
        album.as_deref(),
        item_url.as_deref(),
    ) {
        return PlayerOutcome::NotPlaying;
    }

    PlayerOutcome::Playing(PlayingMedia {
        player_key: bus_name.key.clone(),
        bus_name: bus_name.full.clone(),
        title,
        artist: join_artists(&artists),
        album,
        item_url,
    })
}

/// Read one optional string field, dropping it when absent, empty or wrongly typed.
///
/// The warning names the player and the type, never the value, because the value is exactly
/// what a wrong-typed field might be carrying.
fn string_field(
    metadata: &HashMap<String, BusctlValue>,
    key: &str,
    player: &str,
) -> Option<String> {
    let value = metadata.get(key)?;
    let text = match value.as_string() {
        Some(text) => text,
        None => {
            eprintln!(
                "daytrace: {player} metadata field {key} has unexpected type {}, dropping it",
                value.type_tag
            );
            return None;
        }
    };
    (!text.is_empty()).then(|| text.to_string())
}

/// Read the artist array, dropping empty elements and a wrongly typed field.
fn artist_array(metadata: &HashMap<String, BusctlValue>, player: &str) -> Vec<String> {
    let Some(value) = metadata.get("xesam:artist") else {
        return Vec::new();
    };
    match value.as_string_array() {
        Some(artists) => artists
            .into_iter()
            .filter(|artist| !artist.is_empty())
            .map(str::to_string)
            .collect(),
        None => {
            eprintln!(
                "daytrace: {player} metadata field xesam:artist has unexpected type {}, dropping it",
                value.type_tag
            );
            Vec::new()
        }
    }
}

/// Join the non-empty artists in array order, or absent when none remain.
fn join_artists(artists: &[String]) -> Option<String> {
    let joined = artists.join(", ");
    (!joined.is_empty()).then_some(joined)
}

/// Run a command to completion or kill it at the deadline, returning its stdout.
///
/// The child is killed AND waited on when the deadline passes, so a daemon that times out often
/// does not accumulate zombies for its lifetime. Output is read only after the child exits,
/// which is safe here because `busctl` prints a few kilobytes at most, far under the pipe
/// buffer a larger writer could fill and block on.
fn run_bounded(command: &mut Command, deadline: Instant) -> Result<Vec<u8>, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            // A freshly written executable can be briefly busy (ETXTBSY) while its writeback
            // lands, so retry until the deadline rather than failing a poll over a transient
            // condition.
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                if Instant::now() >= deadline {
                    return Err(format!("failed to run {program}: {error}"));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(format!("failed to run {program}: {error}")),
        }
    };

    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("failed to read {program} output: {error}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("{program} failed: {}", stderr.trim()));
                }
                return Ok(output.stdout);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{program} timed out"));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                return Err(format!("failed to wait on {program}: {error}"));
            }
        }
    }
}

#[cfg(test)]
pub mod fakes {
    use super::{MediaSource, PlayerOutcome};
    use crate::config::Blacklist;
    use std::cell::RefCell;

    /// A media boundary that replays a fixed script of outcomes.
    ///
    /// Responses are popped from the back, so the last listed response is served first, matching
    /// the desktop fake. This is what later beads drive the capture loop with.
    pub struct ScriptedMediaSource {
        remaining: RefCell<Vec<Result<Vec<PlayerOutcome>, String>>>,
    }

    impl ScriptedMediaSource {
        pub fn new(responses: Vec<Result<Vec<PlayerOutcome>, String>>) -> Self {
            Self {
                remaining: RefCell::new(responses),
            }
        }
    }

    impl MediaSource for ScriptedMediaSource {
        fn poll(&self, _blacklist: &Blacklist) -> Result<Vec<PlayerOutcome>, String> {
            self.remaining
                .borrow_mut()
                .pop()
                .expect("script ran out of responses")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes;
    use super::{
        BusName, BusctlClient, MediaSource, PlayerOutcome, PlayingMedia, discovery_command,
        normalize_bus_name, parse_discovery, parse_properties, property_command, run_bounded,
    };
    use crate::config::Blacklist;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// The discovery listing as `busctl --user list --no-pager --no-legend --full` printed it,
    /// with the pid and process names replaced by fictional values.
    const DISCOVERY: &str = "\
org.mpris.MediaPlayer2.brave.instance100200                           100200 brave           user :1.1993       user@1000.service - -
org.mpris.MediaPlayer2.playerctld                                     100201 playerctld      user :1.10162      user@1000.service - -
";

    /// A Chromium-family property query, byte-for-byte the shape `busctl --json=short` printed,
    /// with the title and track id replaced by fictional values. Note the `o`-typed trackid, the
    /// artist array holding one empty string, and the absent `xesam:url`.
    const CHROMIUM_PAUSED: &str = r#"{"type":"s","data":"Paused"}
{"type":"a{sv}","data":{"mpris:length":{"type":"x","data":2596991999},"mpris:trackid":{"type":"o","data":"/com/brave/MediaPlayer2/TrackList/TrackFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"},"xesam:album":{"type":"s","data":""},"xesam:artist":{"type":"as","data":[""]},"xesam:title":{"type":"s","data":"Example Video Title"}}}"#;

    /// The same Chromium shape at `Playing`.
    const CHROMIUM_PLAYING: &str = r#"{"type":"s","data":"Playing"}
{"type":"a{sv}","data":{"mpris:length":{"type":"x","data":2596991999},"mpris:trackid":{"type":"o","data":"/com/brave/MediaPlayer2/TrackList/TrackFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"},"xesam:album":{"type":"s","data":""},"xesam:artist":{"type":"as","data":[""]},"xesam:title":{"type":"s","data":"Example Video Title"}}}"#;

    /// A Spotify property query, the sanitized capture of a real player: eleven keys, with the
    /// `t`, `d` and `i` typings a constructed fixture never carried. Every value is fictional;
    /// the structure and type tags are byte-for-byte what the player emitted.
    const SPOTIFY_PLAYING: &str = r#"{"type":"s","data":"Playing"}
{"type":"a{sv}","data":{"mpris:trackid":{"type":"s","data":"spotify:track:0000000000000000000000"},"mpris:length":{"type":"t","data":213000000},"mpris:artUrl":{"type":"s","data":"https://i.scdn.co/image/0000000000000000000000000000000000000000"},"xesam:album":{"type":"s","data":"A Fictional Album"},"xesam:albumArtist":{"type":"as","data":["A Fictional Artist","A Second Artist"]},"xesam:artist":{"type":"as","data":["A Fictional Artist","A Second Artist"]},"xesam:autoRating":{"type":"d","data":0.5},"xesam:discNumber":{"type":"i","data":1},"xesam:title":{"type":"s","data":"A Fictional Title"},"xesam:trackNumber":{"type":"i","data":1},"xesam:url":{"type":"s","data":"https://open.spotify.com/track/0000000000000000000000"}}}"#;

    fn parse(output: &str, full: &str, key: &str) -> PlayerOutcome {
        parse_properties(
            output,
            &BusName {
                full: full.to_string(),
                key: key.to_string(),
            },
            &Blacklist::default(),
        )
    }

    fn playing(
        key: &str,
        full: &str,
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        url: Option<&str>,
    ) -> PlayerOutcome {
        PlayerOutcome::Playing(PlayingMedia {
            player_key: key.to_string(),
            bus_name: full.to_string(),
            title: title.map(str::to_string),
            artist: artist.map(str::to_string),
            album: album.map(str::to_string),
            item_url: url.map(str::to_string),
        })
    }

    #[test]
    fn discovery_selects_the_mpris_names_and_their_keys() {
        assert_eq!(
            parse_discovery(DISCOVERY),
            vec![BusName {
                full: "org.mpris.MediaPlayer2.brave.instance100200".to_string(),
                key: "brave".to_string(),
            }]
        );
    }

    #[test]
    fn discovery_reads_only_the_first_column_and_the_exact_prefix() {
        let listing = "\
org.mpris.MediaPlayer20.fake 100 fake user :1.1 user@1000.service - -
:1.42 101 some user :1.2 user@1000.service - -
org.mpris.MediaPlayer2.spotify 102 spotify user :1.3 user@1000.service - -
some.service 103 org.mpris.MediaPlayer2.later user :1.4 user@1000.service - -
";
        assert_eq!(
            parse_discovery(listing),
            vec![BusName {
                full: "org.mpris.MediaPlayer2.spotify".to_string(),
                key: "spotify".to_string(),
            }]
        );
    }

    #[test]
    fn playerctld_is_excluded_by_bus_name_not_by_payload() {
        // The measured fixture: playerctld republished the browser's metadata byte-for-byte under
        // its own bus name, so a dedup by identity or metadata cannot see it. The exclusion has
        // to be by bus name, at discovery, before any property query runs.
        let names = parse_discovery(DISCOVERY);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].key, "brave");
    }

    #[test]
    fn an_empty_bus_is_an_empty_result() {
        assert!(parse_discovery("").is_empty());
        assert!(parse_discovery("   \n\t\n").is_empty());
    }

    #[test]
    fn a_bus_with_only_playerctld_is_an_empty_result() {
        let listing =
            "org.mpris.MediaPlayer2.playerctld 100 playerctld user :1.1 user@1000.service - -\n";
        assert!(parse_discovery(listing).is_empty());
    }

    #[test]
    fn normalization_strips_the_prefix_and_the_instance_suffix() {
        assert_eq!(
            normalize_bus_name("org.mpris.MediaPlayer2.spotify"),
            Some("spotify".to_string())
        );
        assert_eq!(
            normalize_bus_name("org.mpris.MediaPlayer2.brave.instance834645"),
            Some("brave".to_string())
        );
        // A stable name containing dots keeps them.
        assert_eq!(
            normalize_bus_name("org.mpris.MediaPlayer2.com.example.player"),
            Some("com.example.player".to_string())
        );
        // A name with no instance suffix is kept whole.
        assert_eq!(
            normalize_bus_name("org.mpris.MediaPlayer2.vlc"),
            Some("vlc".to_string())
        );
        assert_eq!(normalize_bus_name(":1.42"), None);
        assert_eq!(normalize_bus_name("org.mpris.MediaPlayer20.fake"), None);
    }

    #[test]
    fn two_brave_instances_yield_two_entries_with_the_same_key() {
        let listing = "\
org.mpris.MediaPlayer2.brave.instance1 100 brave user :1.1 user@1000.service - -
org.mpris.MediaPlayer2.brave.instance2 101 brave user :1.2 user@1000.service - -
";
        let names = parse_discovery(listing);
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].key, "brave");
        assert_eq!(names[1].key, "brave");
        assert_ne!(names[0].full, names[1].full);
    }

    #[test]
    fn a_paused_player_yields_no_segment() {
        assert_eq!(
            parse(
                CHROMIUM_PAUSED,
                "org.mpris.MediaPlayer2.brave.instance100200",
                "brave"
            ),
            PlayerOutcome::NotPlaying
        );
    }

    #[test]
    fn a_stopped_player_yields_no_segment() {
        let stopped = "{\"type\":\"s\",\"data\":\"Stopped\"}\n{\"type\":\"a{sv}\",\"data\":{}}";
        assert_eq!(
            parse(stopped, "org.mpris.MediaPlayer2.spotify", "spotify"),
            PlayerOutcome::NotPlaying
        );
    }

    #[test]
    fn a_missing_playback_status_is_a_failure() {
        assert!(matches!(
            parse("", "org.mpris.MediaPlayer2.spotify", "spotify"),
            PlayerOutcome::Failed(_)
        ));
    }

    #[test]
    fn a_wrongly_typed_playback_status_is_a_failure() {
        let wrong = "{\"type\":\"x\",\"data\":123}\n{\"type\":\"a{sv}\",\"data\":{}}";
        assert!(matches!(
            parse(wrong, "org.mpris.MediaPlayer2.spotify", "spotify"),
            PlayerOutcome::Failed(_)
        ));
    }

    #[test]
    fn unknown_metadata_keys_are_ignored() {
        let output = "{\"type\":\"s\",\"data\":\"Playing\"}\n{\"type\":\"a{sv}\",\"data\":{\"mpris:length\":{\"type\":\"x\",\"data\":123},\"some:unknown\":{\"type\":\"s\",\"data\":\"ignored\"},\"xesam:title\":{\"type\":\"s\",\"data\":\"Track\"}}}";
        assert_eq!(
            parse(output, "org.mpris.MediaPlayer2.spotify", "spotify"),
            playing(
                "spotify",
                "org.mpris.MediaPlayer2.spotify",
                Some("Track"),
                None,
                None,
                None
            )
        );
    }

    #[test]
    fn a_wrongly_typed_known_field_is_dropped_alone() {
        let output = "{\"type\":\"s\",\"data\":\"Playing\"}\n{\"type\":\"a{sv}\",\"data\":{\"xesam:title\":{\"type\":\"x\",\"data\":123},\"xesam:album\":{\"type\":\"s\",\"data\":\"Album\"}}}";
        assert_eq!(
            parse(output, "org.mpris.MediaPlayer2.spotify", "spotify"),
            playing(
                "spotify",
                "org.mpris.MediaPlayer2.spotify",
                None,
                None,
                Some("Album"),
                None
            )
        );
    }

    #[test]
    fn artists_join_in_order_and_empty_normalizes_to_absent() {
        // The measured Chromium capture publishes [""] and an empty album: the ordinary case.
        assert_eq!(
            parse(
                CHROMIUM_PLAYING,
                "org.mpris.MediaPlayer2.brave.instance100200",
                "brave"
            ),
            playing(
                "brave",
                "org.mpris.MediaPlayer2.brave.instance100200",
                Some("Example Video Title"),
                None,
                None,
                None
            )
        );

        let two = "{\"type\":\"s\",\"data\":\"Playing\"}\n{\"type\":\"a{sv}\",\"data\":{\"xesam:artist\":{\"type\":\"as\",\"data\":[\"First\",\"Second\"]}}}";
        assert_eq!(
            parse(two, "org.mpris.MediaPlayer2.spotify", "spotify"),
            playing(
                "spotify",
                "org.mpris.MediaPlayer2.spotify",
                None,
                Some("First, Second"),
                None,
                None
            )
        );

        let empty = "{\"type\":\"s\",\"data\":\"Playing\"}\n{\"type\":\"a{sv}\",\"data\":{\"xesam:artist\":{\"type\":\"as\",\"data\":[]}}}";
        assert_eq!(
            parse(empty, "org.mpris.MediaPlayer2.spotify", "spotify"),
            playing(
                "spotify",
                "org.mpris.MediaPlayer2.spotify",
                None,
                None,
                None,
                None
            )
        );
    }

    #[test]
    fn a_playing_player_with_no_metadata_is_returned_by_name() {
        let output = "{\"type\":\"s\",\"data\":\"Playing\"}\n{\"type\":\"a{sv}\",\"data\":{}}";
        assert_eq!(
            parse(output, "org.mpris.MediaPlayer2.spotify", "spotify"),
            playing(
                "spotify",
                "org.mpris.MediaPlayer2.spotify",
                None,
                None,
                None,
                None
            )
        );
    }

    #[test]
    fn arturl_and_trackid_never_reach_the_result() {
        // The Spotify fixture carries an `s`-typed trackid and an artUrl; the Chromium one an
        // `o`-typed trackid. Neither field exists on PlayingMedia, so the whole returned value is
        // asserted to prove both are dropped.
        //
        // The same assertion also proves the parser survives the real player's `t`, `d` and `i`
        // typings (a uint64 length, a double rating, int32 disc and track numbers) and skips the
        // fields this feature has no use for: autoRating, discNumber, trackNumber, length and
        // the second `as` field albumArtist, rather than failing on them.
        assert_eq!(
            parse(SPOTIFY_PLAYING, "org.mpris.MediaPlayer2.spotify", "spotify"),
            playing(
                "spotify",
                "org.mpris.MediaPlayer2.spotify",
                Some("A Fictional Title"),
                Some("A Fictional Artist, A Second Artist"),
                Some("A Fictional Album"),
                Some("https://open.spotify.com/track/0000000000000000000000"),
            )
        );
    }

    #[test]
    fn discovery_invokes_busctl_with_the_full_listing_flags() {
        let args: Vec<String> = discovery_command("busctl")
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec!["--user", "list", "--no-pager", "--no-legend", "--full"]
        );
    }

    #[test]
    fn property_query_invokes_busctl_with_json_short_and_both_properties() {
        let args: Vec<String> = property_command("busctl", "org.mpris.MediaPlayer2.spotify")
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "--user",
                "--json=short",
                "get-property",
                "org.mpris.MediaPlayer2.spotify",
                "/org/mpris/MediaPlayer2",
                "org.mpris.MediaPlayer2.Player",
                "PlaybackStatus",
                "Metadata",
            ]
        );
    }

    fn assert_hung_command_is_killed_and_reaped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let pid_path = pid_file.to_str().expect("utf8 path");
        let mut command = Command::new("sh");
        command.args(["-c", &format!("echo $$ > {pid_path}; exec sleep 60")]);
        let result = run_bounded(&mut command, Instant::now() + Duration::from_millis(50));
        assert!(
            result.is_err(),
            "a hung command must be reported as a failure"
        );
        assert!(result.unwrap_err().contains("timed out"));

        // The child was killed AND reaped: its pid is no longer alive, not even as a zombie.
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(!alive, "child {pid} was not reaped after the timeout");
    }

    #[test]
    fn a_command_that_never_exits_is_killed_and_reaped() {
        assert_hung_command_is_killed_and_reaped();
    }

    #[test]
    fn repeated_timeouts_leave_no_unreaped_child() {
        for _ in 0..3 {
            assert_hung_command_is_killed_and_reaped();
        }
    }

    fn fake_busctl(script: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("busctl");
        std::fs::write(&path, script).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        (dir, path.to_str().expect("utf8 path").to_string())
    }

    #[test]
    fn the_poll_budget_is_shared_across_discovery_and_every_player() {
        let script = "\
#!/bin/sh
if [ \"$2\" = \"list\" ]; then
    sleep 0.4
    echo 'org.mpris.MediaPlayer2.spotify 100 spotify user :1.1 user@1000.service - -'
    echo 'org.mpris.MediaPlayer2.brave.instance1 101 brave user :1.2 user@1000.service - -'
    echo 'org.mpris.MediaPlayer2.brave.instance2 102 brave user :1.3 user@1000.service - -'
else
    sleep 0.4
    echo '{\"type\":\"s\",\"data\":\"Playing\"}'
    echo '{\"type\":\"a{sv}\",\"data\":{}}'
fi
";
        let (_dir, path) = fake_busctl(script);
        let client = BusctlClient::with_command(path);
        let start = Instant::now();
        let result = client.poll(&Blacklist::default());
        let elapsed = start.elapsed();
        assert!(result.is_ok(), "the poll should complete: {result:?}");
        assert!(
            elapsed < Duration::from_millis(1300),
            "the poll took {elapsed:?}; a deadline per invocation would take ~1.6s"
        );
    }

    #[test]
    fn a_list_failure_is_a_whole_source_error() {
        let (_dir, path) = fake_busctl("#!/bin/sh\nexit 1\n");
        let client = BusctlClient::with_command(path);
        assert!(client.poll(&Blacklist::default()).is_err());
    }

    #[test]
    fn a_property_failure_costs_one_player_and_leaves_the_others() {
        let script = "\
#!/bin/sh
if [ \"$2\" = \"list\" ]; then
    echo 'org.mpris.MediaPlayer2.spotify 100 spotify user :1.1 user@1000.service - -'
    echo 'org.mpris.MediaPlayer2.brave.instance1 101 brave user :1.2 user@1000.service - -'
elif [ \"$4\" = \"org.mpris.MediaPlayer2.spotify\" ]; then
    echo '{\"type\":\"s\",\"data\":\"Playing\"}'
    echo '{\"type\":\"a{sv}\",\"data\":{\"xesam:title\":{\"type\":\"s\",\"data\":\"Track\"}}}'
else
    exit 1
fi
";
        let (_dir, path) = fake_busctl(script);
        let client = BusctlClient::with_command(path);
        let result = client.poll(&Blacklist::default()).expect("list succeeded");
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], PlayerOutcome::Playing(_)));
        assert!(matches!(&result[1], PlayerOutcome::Failed(_)));
    }

    #[test]
    fn two_items_differing_in_address_are_two_identities() {
        let a = PlayingMedia {
            player_key: "spotify".to_string(),
            bus_name: "org.mpris.MediaPlayer2.spotify".to_string(),
            title: Some("Episode".to_string()),
            artist: Some("Podcast".to_string()),
            album: Some("Show".to_string()),
            item_url: Some("https://open.spotify.com/episode/1".to_string()),
        };
        let b = PlayingMedia {
            item_url: Some("https://open.spotify.com/episode/2".to_string()),
            ..a.clone()
        };
        assert_ne!(a, b, "a different address is a different item");
    }

    #[test]
    fn the_scripted_media_source_replays_its_responses() {
        let playing = PlayerOutcome::Playing(PlayingMedia {
            player_key: "spotify".to_string(),
            bus_name: "org.mpris.MediaPlayer2.spotify".to_string(),
            title: Some("Track".to_string()),
            artist: None,
            album: None,
            item_url: None,
        });
        // Popped from the back, so the error is served first.
        let source = fakes::ScriptedMediaSource::new(vec![
            Ok(vec![playing.clone()]),
            Err("busctl list failed".to_string()),
        ]);
        assert_eq!(
            source.poll(&Blacklist::default()),
            Err("busctl list failed".to_string())
        );
        assert_eq!(source.poll(&Blacklist::default()), Ok(vec![playing]));
    }
}
