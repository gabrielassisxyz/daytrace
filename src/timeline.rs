use crate::activity::{ActivityKind, TimelineSegment};
use chrono::{Local, NaiveDate, TimeZone};

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
    Ok(Local
        .from_local_datetime(&midnight)
        .earliest()
        .ok_or_else(|| format!("local midnight does not exist for {date}"))?
        .timestamp())
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
    Ok(output)
}

pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn format_segment(segment: &TimelineSegment) -> Result<String, String> {
    let started = format_clock(segment.started_at)?;
    let ended = format_clock(segment.ended_at)?;
    let duration = format_duration(segment.ended_at.saturating_sub(segment.started_at));
    let label = match segment.snapshot.kind {
        ActivityKind::Idle => "AFK".to_string(),
        ActivityKind::Window => {
            let app = segment
                .snapshot
                .app_class
                .as_deref()
                .unwrap_or("unknown app");
            let title = segment.snapshot.title.as_deref().unwrap_or("untitled");
            format!("{app} - {title}")
        }
        ActivityKind::Unknown => "Unknown".to_string(),
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
    use super::{day_bounds, local_date, render_day};
    use crate::activity::{ActivitySnapshot, TimelineSegment};
    use chrono::NaiveDate;

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
}
