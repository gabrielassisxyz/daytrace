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
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run {program}: {error}"))?;

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
