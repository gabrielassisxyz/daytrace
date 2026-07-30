use crate::activity::{ActivityKind, TimelineSegment};
use chrono::{Local, MappedLocalTime, NaiveDate, TimeZone};
use std::cmp::Reverse;
use std::collections::BTreeMap;

/// The local calendar day a unix timestamp falls in.
pub fn local_date(now: i64) -> Result<NaiveDate, String> {
    Ok(Local
        .timestamp_opt(now, 0)
        .single()
        .ok_or_else(|| "timestamp is outside supported range".to_string())?
        .date_naive())
}

/// The half-open unix range `[start, end)` covering `date` in local time.
///
/// The end comes from the following day's start rather than from a fixed number of seconds,
/// because a clock change makes a day 23 or 25 hours long and the ranges of two neighbouring
/// days still have to meet exactly. Anything else drops a segment into a gap between two
/// days, or reports it under both.
pub fn day_bounds(date: NaiveDate) -> Result<(i64, i64), String> {
    let next = date
        .succ_opt()
        .ok_or_else(|| format!("{date} has no following day in the supported range"))?;
    Ok((local_day_start(date)?, local_day_start(next)?))
}

fn local_day_start(date: NaiveDate) -> Result<i64, String> {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| format!("failed to build local midnight for {date}"))?;

    match Local.from_local_datetime(&midnight) {
        MappedLocalTime::Single(at) => Ok(at.timestamp()),
        // A clock moved back repeats the hour, so midnight happens twice and the day has to
        // open at the first of the two. The two instants are compared rather than asking for
        // the earliest of the pair, because the earliest of two *local* readings is the one
        // with the smaller offset, which is the later instant: in a zone that falls back at
        // one in the morning, that credited the first hour of the day to the day before.
        MappedLocalTime::Ambiguous(one, other) => Ok(one.timestamp().min(other.timestamp())),
        // A clock moved forward can delete midnight outright. The day still happened, and it
        // began when the clock jumped. Refusing to name its start refused the report for two
        // days at once: the requested one, and the one before it, whose end is this start.
        MappedLocalTime::None => first_hour_that_exists(date),
    }
}

fn first_hour_that_exists(date: NaiveDate) -> Result<i64, String> {
    (1..=3)
        .filter_map(|hour| date.and_hms_opt(hour, 0, 0))
        .find_map(|local| Local.from_local_datetime(&local).earliest())
        .map(|at| at.timestamp())
        .ok_or_else(|| format!("no local start of day exists for {date}"))
}

/// Time one application held, summed over a day.
#[derive(Debug, Eq, PartialEq)]
pub struct ApplicationTotal {
    pub label: String,
    pub seconds: i64,
}

/// Total each application over `segments`, longest first.
///
/// Seconds are summed and only then formatted. Adding up what each row displays would
/// compound its rounding, so a minute spent in three short visits would report as nothing
/// at all, which is the opposite of what a total is for.
pub fn application_totals(segments: &[TimelineSegment]) -> Vec<ApplicationTotal> {
    let mut seconds_by_label: BTreeMap<&str, i64> = BTreeMap::new();
    for segment in segments {
        *seconds_by_label
            .entry(application_label(segment))
            .or_default() += segment.ended_at.saturating_sub(segment.started_at).max(0);
    }

    let mut totals: Vec<ApplicationTotal> = seconds_by_label
        .into_iter()
        .map(|(label, seconds)| ApplicationTotal {
            label: label.to_string(),
            seconds,
        })
        .collect();
    // Longest first, and equal totals by name so the same day reads the same way twice.
    // The name comparison ignores case: application classes arrive in whichever case the
    // compositor reports, and raw byte order would sort every capitalised one above every
    // lowercase one.
    totals.sort_by(|left, right| {
        Reverse(left.seconds)
            .cmp(&Reverse(right.seconds))
            .then_with(|| {
                left.label
                    .to_lowercase()
                    .cmp(&right.label.to_lowercase())
                    .then_with(|| left.label.cmp(&right.label))
            })
    });
    totals
}

pub fn render_day(date: NaiveDate, segments: &[TimelineSegment]) -> Result<String, String> {
    if segments.is_empty() {
        return Ok(format!("No activity events recorded for {date}.\n"));
    }

    let mut output = format!("Timeline for {date}\n");
    for segment in segments {
        output.push_str(&format_segment(segment)?);
        output.push('\n');
    }

    output.push_str("\nTime per application\n");
    for total in application_totals(segments) {
        let duration = format_duration(total.seconds);
        output.push_str(&format!("{duration:>6}  {}\n", total.label));
    }
    Ok(output)
}

/// What to call the thing that held the time, for both a timeline row and a total.
///
/// One source for both, because a row and a total that disagree about the name of an
/// application read as two different applications in the same report.
fn application_label(segment: &TimelineSegment) -> &str {
    match segment.snapshot.kind {
        ActivityKind::Idle => "AFK",
        // Named apart from absence on purpose: a report that called a suspended night "AFK"
        // would say somebody was away from a running machine for eight hours.
        ActivityKind::Suspended => "Suspended",
        ActivityKind::Unknown => "Unknown",
        ActivityKind::Window => segment
            .snapshot
            .app_class
            .as_deref()
            .unwrap_or("unknown app"),
    }
}

pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn format_segment(segment: &TimelineSegment) -> Result<String, String> {
    let started = format_clock(segment.started_at)?;
    let ended = format_clock(segment.ended_at)?;
    let duration = format_duration(segment.ended_at.saturating_sub(segment.started_at));
    let label = match segment.snapshot.kind {
        ActivityKind::Window => {
            let title = segment.snapshot.title.as_deref().unwrap_or("untitled");
            format!("{} - {title}", application_label(segment))
        }
        _ => application_label(segment).to_string(),
    };
    let location = match (&segment.snapshot.workspace, segment.snapshot.monitor) {
        (Some(workspace), Some(monitor)) => format!(" workspace {workspace}, monitor {monitor}"),
        (Some(workspace), None) => format!(" workspace {workspace}"),
        (None, Some(monitor)) => format!(" monitor {monitor}"),
        (None, None) => String::new(),
    };

    Ok(format!(
        "{started}-{ended}  {duration:<6}  {label}{location}"
    ))
}

fn format_clock(timestamp: i64) -> Result<String, String> {
    Ok(Local
        .timestamp_opt(timestamp, 0)
        .single()
        .ok_or_else(|| "timeline timestamp is outside supported range".to_string())?
        .format("%H:%M")
        .to_string())
}

fn format_duration(seconds: i64) -> String {
    let minutes = (seconds.max(0) + 30) / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }

    let hours = minutes / 60;
    let remaining = minutes % 60;
    if remaining == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{remaining:02}m")
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplicationTotal, application_totals, day_bounds, local_date, render_day};
    use crate::activity::{ActivitySnapshot, TimelineSegment};
    use chrono::NaiveDate;

    fn idle_segment(started_at: i64, ended_at: i64) -> TimelineSegment {
        TimelineSegment {
            started_at,
            ended_at,
            snapshot: ActivitySnapshot::idle(),
        }
    }

    fn total(label: &str, seconds: i64) -> ApplicationTotal {
        ApplicationTotal {
            label: label.to_string(),
            seconds,
        }
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    fn segment(started_at: i64, ended_at: i64, app: &str) -> TimelineSegment {
        TimelineSegment {
            started_at,
            ended_at,
            snapshot: ActivitySnapshot::window(
                Some(app.to_string()),
                Some("a window".to_string()),
                None,
                None,
            ),
        }
    }

    #[test]
    fn a_requested_day_spans_exactly_that_local_day() {
        let (start, end) = day_bounds(date(2026, 7, 20)).expect("bounds");

        assert_eq!(
            local_date(start).expect("start date"),
            date(2026, 7, 20),
            "the range must open on the requested day"
        );
        assert_eq!(
            local_date(end).expect("end date"),
            date(2026, 7, 21),
            "the range is half-open, so it ends at the next day's first instant"
        );
        assert_eq!(
            local_date(end - 1).expect("last second"),
            date(2026, 7, 20),
            "the last second of the day belongs to the requested day"
        );
    }

    #[test]
    fn consecutive_days_neither_overlap_nor_leave_a_gap() {
        let (_, first_end) = day_bounds(date(2026, 7, 20)).expect("first day");
        let (second_start, _) = day_bounds(date(2026, 7, 21)).expect("second day");

        assert_eq!(
            first_end, second_start,
            "a segment must not fall between two days, nor be reported by both"
        );
    }

    #[test]
    fn the_header_names_the_requested_day_and_not_today() {
        let (start, _) = day_bounds(date(2026, 7, 20)).expect("bounds");
        let rendered = render_day(date(2026, 7, 20), &[segment(start, start + 600, "ghostty")])
            .expect("render");

        assert!(
            rendered.starts_with("Timeline for 2026-07-20\n"),
            "a report for a past day must not be labelled with another date: {rendered}"
        );
        assert!(rendered.contains("ghostty"), "{rendered}");
    }

    #[test]
    fn an_empty_day_says_which_day_was_empty() {
        let rendered = render_day(date(2026, 7, 20), &[]).expect("render");

        assert_eq!(rendered, "No activity events recorded for 2026-07-20.\n");
    }

    #[test]
    fn an_application_is_totalled_across_the_whole_day_not_per_visit() {
        let totals = application_totals(&[
            segment(0, 600, "ghostty"),
            segment(600, 900, "firefox"),
            segment(900, 1200, "ghostty"),
        ]);

        assert_eq!(
            totals,
            vec![total("ghostty", 900), total("firefox", 300)],
            "returning to an application must add to its total, and the longest comes first"
        );
    }

    #[test]
    fn totals_add_seconds_rather_than_the_minutes_each_row_shows() {
        let short_visits: Vec<TimelineSegment> = (0..3)
            .map(|index| segment(index * 100, index * 100 + 20, "ghostty"))
            .collect();

        let rendered = render_day(date(2026, 7, 20), &short_visits).expect("render");
        let totals = application_totals(&short_visits);

        assert_eq!(
            totals,
            vec![total("ghostty", 60)],
            "a minute spent in three short visits is still a minute"
        );
        assert!(
            rendered.contains("Time per application") && rendered.contains("1m  ghostty"),
            "the total must not inherit the rounding of the rows it sums: {rendered}"
        );
    }

    #[test]
    fn absence_is_totalled_apart_from_any_application() {
        let totals = application_totals(&[
            segment(0, 300, "ghostty"),
            idle_segment(300, 1200),
            segment(1200, 1500, "ghostty"),
        ]);

        assert_eq!(totals, vec![total("AFK", 900), total("ghostty", 600)]);
    }

    #[test]
    fn a_powered_down_stretch_is_reported_apart_from_ordinary_absence() {
        let segments = [
            segment(0, 300, "ghostty"),
            idle_segment(300, 600),
            TimelineSegment {
                started_at: 600,
                ended_at: 4_200,
                snapshot: ActivitySnapshot::suspended(),
            },
        ];

        let rendered = render_day(date(2026, 7, 20), &segments).expect("render");

        assert_eq!(
            application_totals(&segments),
            vec![
                total("Suspended", 3_600),
                total("AFK", 300),
                total("ghostty", 300)
            ],
            "a machine that was off is not somebody sitting still, and the day may not merge them"
        );
        assert!(
            rendered.contains("Suspended"),
            "the timeline has to say which absence it was: {rendered}"
        );
    }

    #[test]
    fn applications_holding_equal_time_are_ordered_by_name() {
        // Mixed case on purpose: compositors report classes such as `Zed` and `firefox` side
        // by side, and ordering them by raw bytes puts every capitalised name above every
        // lowercase one, which reads as an ordering nobody chose.
        let totals = application_totals(&[
            segment(0, 600, "Zed"),
            segment(600, 1200, "alacritty"),
            segment(1200, 1800, "Brave"),
        ]);

        assert_eq!(
            totals,
            vec![
                total("alacritty", 600),
                total("Brave", 600),
                total("Zed", 600)
            ],
            "a tie must be ordered by name and never reshuffle between runs"
        );
    }

    #[test]
    fn the_timeline_keeps_its_chronology_above_the_totals() {
        let rendered = render_day(
            date(2026, 7, 20),
            &[segment(0, 600, "ghostty"), segment(600, 900, "firefox")],
        )
        .expect("render");

        let chronology = rendered.find("ghostty - a window").expect("timeline row");
        let totals = rendered.find("Time per application").expect("totals block");
        assert!(
            chronology < totals,
            "the day is read in order first and summed second: {rendered}"
        );
    }
}
