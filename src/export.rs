use crate::activity::{MediaSegment, TimelineSegment};
use chrono::{Local, NaiveDate, SecondsFormat, TimeZone};
use serde::Serialize;

/// One day of activity, in the shape the export promises to keep.
///
/// `media` is a field of its own rather than a `lane` key added to `segments`: a consumer that
/// sums `segments` for a day's total must not have to know this tool's version to tell whether
/// a media row already lives in that list.
#[derive(Debug, Serialize)]
struct DayExport {
    date: String,
    segments: Vec<SegmentExport>,
    media: Vec<MediaExport>,
}

/// One stored segment.
///
/// A type of its own rather than serializing the stored model directly, so that the wire
/// shape is a decision made here: the storage model is free to grow a column without that
/// column silently becoming part of what leaves the machine.
#[derive(Debug, Serialize)]
struct SegmentExport {
    started_at: String,
    ended_at: String,
    duration_seconds: i64,
    kind: &'static str,
    app_class: Option<String>,
    title: Option<String>,
    workspace: Option<String>,
    monitor: Option<i64>,
}

/// One stored media segment.
///
/// The internal lane and the stored `kind` never appear here, for the same reason
/// `SegmentExport` exists apart from the storage model: what leaves the machine is a decision
/// made at this boundary, not a consequence of a column existing.
#[derive(Debug, Serialize)]
struct MediaExport {
    started_at: String,
    ended_at: String,
    duration_seconds: i64,
    player: Option<String>,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    item_url: Option<String>,
}

/// Emit one day of stored activity as JSON.
///
/// Instants are RFC 3339 with the local offset, so an exported day stays readable off the
/// machine that recorded it, where a bare unix second would need the reader to already know
/// which timezone produced it. `duration_seconds` comes along because summing a day should
/// not require parsing two timestamps per segment.
///
/// `media` is always present, empty when nothing played: a day exported before media existed
/// and a day that genuinely had none would otherwise be indistinguishable only by omission,
/// which asks a consumer to know the tool's version to tell them apart.
pub fn render_day_export(
    date: NaiveDate,
    segments: &[TimelineSegment],
    media: &[MediaSegment],
) -> Result<String, String> {
    let day = DayExport {
        date: date.to_string(),
        segments: segments
            .iter()
            .map(export_segment)
            .collect::<Result<Vec<_>, _>>()?,
        media: media
            .iter()
            .map(export_media_segment)
            .collect::<Result<Vec<_>, _>>()?,
    };

    let mut json = serde_json::to_string_pretty(&day)
        .map_err(|error| format!("failed to serialize the day: {error}"))?;
    json.push('\n');
    Ok(json)
}

fn export_segment(segment: &TimelineSegment) -> Result<SegmentExport, String> {
    Ok(SegmentExport {
        started_at: format_instant(segment.started_at)?,
        ended_at: format_instant(segment.ended_at)?,
        duration_seconds: segment.ended_at.saturating_sub(segment.started_at).max(0),
        kind: segment.snapshot.kind.as_str(),
        app_class: segment.snapshot.app_class.clone(),
        title: segment.snapshot.title.clone(),
        workspace: segment.snapshot.workspace.clone(),
        monitor: segment.snapshot.monitor,
    })
}

fn export_media_segment(segment: &MediaSegment) -> Result<MediaExport, String> {
    Ok(MediaExport {
        started_at: format_instant(segment.started_at)?,
        ended_at: format_instant(segment.ended_at)?,
        duration_seconds: segment.ended_at.saturating_sub(segment.started_at).max(0),
        player: segment.snapshot.player.clone(),
        title: segment.snapshot.title.clone(),
        artist: segment.snapshot.artist.clone(),
        album: segment.snapshot.album.clone(),
        item_url: segment.snapshot.item_url.clone(),
    })
}

fn format_instant(timestamp: i64) -> Result<String, String> {
    Ok(Local
        .timestamp_opt(timestamp, 0)
        .single()
        .ok_or_else(|| format!("stored timestamp {timestamp} is outside supported range"))?
        .to_rfc3339_opts(SecondsFormat::Secs, false))
}

#[cfg(test)]
mod tests {
    use super::render_day_export;
    use crate::activity::{ActivitySnapshot, MediaSegment, MediaSnapshot, TimelineSegment};
    use chrono::{DateTime, NaiveDate};
    use serde_json::Value;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 20).expect("valid date")
    }

    fn window_segment(started_at: i64, ended_at: i64) -> TimelineSegment {
        TimelineSegment {
            started_at,
            ended_at,
            snapshot: ActivitySnapshot::window(
                Some("ghostty".to_string()),
                Some("tmux".to_string()),
                Some("3".to_string()),
                Some(1),
            ),
        }
    }

    fn media_segment(started_at: i64, ended_at: i64) -> MediaSegment {
        MediaSegment {
            started_at,
            ended_at,
            snapshot: MediaSnapshot {
                player: Some("spotify".to_string()),
                title: Some("Track title".to_string()),
                artist: Some("Artist".to_string()),
                album: Some("Album".to_string()),
                item_url: Some("https://open.spotify.com/track/1".to_string()),
            },
        }
    }

    fn exported(segments: &[TimelineSegment], media: &[MediaSegment]) -> Value {
        let json = render_day_export(date(), segments, media).expect("export");
        assert!(json.ends_with('\n'), "output must be a complete line");
        serde_json::from_str(&json).expect("export must be valid JSON")
    }

    #[test]
    fn an_exported_day_names_the_day_and_lists_its_segments() {
        let value = exported(&[window_segment(1_784_000_000, 1_784_000_600)], &[]);

        assert_eq!(value["date"], "2026-07-20");
        assert_eq!(
            value["segments"].as_array().expect("segments array").len(),
            1
        );
        assert_eq!(value["segments"][0]["app_class"], "ghostty");
        assert_eq!(value["segments"][0]["title"], "tmux");
        assert_eq!(value["segments"][0]["kind"], "window");
    }

    #[test]
    fn an_exported_instant_is_unambiguous_without_knowing_the_machine() {
        let value = exported(&[window_segment(1_784_000_000, 1_784_000_600)], &[]);
        let started_at = value["segments"][0]["started_at"]
            .as_str()
            .expect("started_at string");

        let parsed = DateTime::parse_from_rfc3339(started_at)
            .unwrap_or_else(|error| panic!("{started_at} is not RFC 3339: {error}"));
        assert_eq!(
            parsed.timestamp(),
            1_784_000_000,
            "the exported instant must be the stored one, offset included"
        );
    }

    #[test]
    fn an_exported_segment_carries_its_duration_in_seconds() {
        let value = exported(&[window_segment(1_784_000_000, 1_784_000_600)], &[]);

        assert_eq!(
            value["segments"][0]["duration_seconds"], 600,
            "a consumer must not have to parse two timestamps to sum a day"
        );
    }

    #[test]
    fn an_exported_segment_exposes_exactly_the_documented_fields() {
        let value = exported(
            &[TimelineSegment {
                started_at: 1_784_000_000,
                ended_at: 1_784_000_600,
                snapshot: ActivitySnapshot::idle(),
            }],
            &[],
        );
        let segment = value["segments"][0].as_object().expect("segment object");

        let mut keys: Vec<&str> = segment.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "app_class",
                "duration_seconds",
                "ended_at",
                "kind",
                "monitor",
                "started_at",
                "title",
                "workspace",
            ],
            "the exported shape is a contract: an absent value stays a null key, and no \
             internal column may leak into it"
        );
        assert!(
            segment["app_class"].is_null() && segment["workspace"].is_null(),
            "absent values must be null rather than missing: {segment:?}"
        );
    }

    #[test]
    fn a_powered_down_stretch_leaves_the_machine_as_its_own_kind() {
        let value = exported(
            &[TimelineSegment {
                started_at: 1_784_000_000,
                ended_at: 1_784_003_600,
                snapshot: ActivitySnapshot::suspended(),
            }],
            &[],
        );

        assert_eq!(
            value["segments"][0]["kind"], "suspended",
            "a consumer of the export has to be able to tell a machine that was off from one \
             nobody was using"
        );
    }

    #[test]
    fn a_day_with_nothing_stored_exports_an_empty_list_rather_than_failing() {
        let value = exported(&[], &[]);

        assert_eq!(value["date"], "2026-07-20");
        assert_eq!(
            value["segments"].as_array().expect("segments array").len(),
            0
        );
    }

    #[test]
    fn the_top_level_export_carries_exactly_date_media_and_segments() {
        let json = render_day_export(date(), &[], &[]).expect("export");
        let value: Value = serde_json::from_str(&json).expect("valid JSON");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();

        assert_eq!(
            keys,
            vec!["date", "media", "segments"],
            "no storage, lane, version or reconciliation field may appear at the top level: \
             {json}"
        );
    }

    #[test]
    fn a_day_with_no_media_exports_an_empty_array_rather_than_omitting_the_key() {
        let value = exported(&[window_segment(1_784_000_000, 1_784_000_600)], &[]);

        assert!(
            value.get("media").is_some(),
            "the key must be present even on a day with nothing to put in it: {value}"
        );
        assert_eq!(
            value["media"].as_array().expect("media array").len(),
            0,
            "a consumer must not have to tell an empty day from a daytrace older than media"
        );
    }

    #[test]
    fn a_media_entry_lists_the_day_and_carries_its_own_fields() {
        let value = exported(&[], &[media_segment(1_784_000_000, 1_784_001_440)]);

        assert_eq!(value["media"].as_array().expect("media array").len(), 1);
        assert_eq!(value["media"][0]["player"], "spotify");
        assert_eq!(value["media"][0]["title"], "Track title");
        assert_eq!(value["media"][0]["artist"], "Artist");
        assert_eq!(value["media"][0]["album"], "Album");
        assert_eq!(
            value["media"][0]["item_url"],
            "https://open.spotify.com/track/1"
        );
        assert_eq!(value["media"][0]["duration_seconds"], 1_440);
    }

    #[test]
    fn a_media_entry_exposes_exactly_the_documented_fields() {
        let value = exported(
            &[],
            &[MediaSegment {
                started_at: 1_784_000_000,
                ended_at: 1_784_000_600,
                snapshot: MediaSnapshot {
                    player: None,
                    title: None,
                    artist: None,
                    album: None,
                    item_url: None,
                },
            }],
        );
        let entry = value["media"][0].as_object().expect("media object");

        let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "album",
                "artist",
                "duration_seconds",
                "ended_at",
                "item_url",
                "player",
                "started_at",
                "title",
            ],
            "the lane and the stored kind must never leave the machine: {entry:?}"
        );
        assert!(
            entry["player"].is_null() && entry["artist"].is_null(),
            "an absent value stays a null key rather than a missing one: {entry:?}"
        );
    }
}
