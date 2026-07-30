use crate::activity::{ActivityKind, TimelineSegment};
use chrono::{Days, Local, MappedLocalTime, NaiveDate, TimeZone};
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

/// The first instant a window of `retention_days` keeps, given the current instant `now`.
///
/// The boundary is a local midnight rather than `now` minus a multiple of 86400 seconds,
/// because every command that reads the store is addressed by local day. A window measured in
/// raw seconds would fall part-way through the oldest day it keeps, so that day would report
/// only the hours after the cutoff while still being labelled a whole day, and a clock change
/// would move the boundary by an hour on top of that.
pub fn retention_cutoff(now: i64, retention_days: u32) -> Result<i64, String> {
    let oldest_kept = local_date(now)?
        .checked_sub_days(Days::new(retention_days.into()))
        .ok_or_else(|| format!("a window of {retention_days} days reaches before the calendar"))?;
    local_day_start(oldest_kept)
}

fn local_day_start(date: NaiveDate) -> Result<i64, String> {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| format!("failed to build local midnight for {date}"))?;

    match Local.from_local_datetime(&midnight) {
        MappedLocalTime::Single(at) => Ok(at.timestamp()),
        // A clock moved back gives midnight two candidate instants, and the day opens at the
        // earliest of them that actually falls on this date. Both halves of that matter, because
        // the two zones it covers are not the same shape. Where the clock falls back at one in
        // the morning, midnight genuinely happens twice, both candidates fall on this date, and
        // the day must open at the first pass. Where it falls back *at* midnight, as Sao_Paulo
        // did until 2019, the repeated hour is the one before midnight and the second candidate
        // reads locally as 23:00 on the day before: taking it put the boundary an hour inside the
        // day it was meant to open, so the 17th of February 2018 reported 24 hours of a 25 hour
        // day and handed its last hour to the 18th.
        MappedLocalTime::Ambiguous(one, other) => earliest_instant_on(date, [one, other]),
        // A clock moved forward can delete midnight outright. The day still happened, and it
        // began when the clock jumped. Refusing to name its start refused the report for two
        // days at once: the requested one, and the one before it, whose end is this start.
        MappedLocalTime::None => first_hour_that_exists(date),
    }
}

/// The earliest of the candidate instants whose local reading falls on `date`.
///
/// Both candidates are the same wall clock time under two different offsets, so which of them is
/// really on `date` is a question about the zone rather than about the clock, and only the local
/// reading answers it. Falling back to the earliest instant if neither qualifies keeps a zone this
/// does not anticipate reporting a day rather than refusing one.
fn earliest_instant_on(
    date: NaiveDate,
    candidates: [chrono::DateTime<Local>; 2],
) -> Result<i64, String> {
    let mut instants = candidates.map(|at| at.timestamp());
    instants.sort_unstable();

    Ok(instants
        .into_iter()
        .find(|at| local_date(*at).is_ok_and(|day| day == date))
        .unwrap_or(instants[0]))
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
    // A row needs the end of the reported day to tell a boundary clip from a segment that
    // genuinely stopped at midnight, and only the day being rendered knows where that is.
    let (_, day_end) = day_bounds(date)?;

    if segments.is_empty() {
        return Ok(format!("No activity events recorded for {date}.\n"));
    }

    let mut output = format!("Timeline for {date}\n");
    for segment in segments {
        output.push_str(&format_segment(segment, day_end)?);
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

fn format_segment(segment: &TimelineSegment, day_end: i64) -> Result<String, String> {
    let started = format_clock(segment.started_at)?;
    let ended = format_end_clock(segment.ended_at, day_end)?;
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

/// The end of a segment, where reaching the end of the reported day is said rather than shown.
///
/// A segment is clipped to the day it is reported under, and the clip target is the following
/// day's first instant, which a clock formatter reads as `00:00`. So a segment covering a whole
/// day claimed `00:00-00:00` while showing 24 hours, and one running past midnight gave no way to
/// tell which midnight it meant. `24:00` is deliberately not a wall clock reading: it names the
/// boundary, instead of an instant that belongs to the next day.
fn format_end_clock(ended_at: i64, day_end: i64) -> Result<String, String> {
    if ended_at >= day_end {
        return Ok("24:00".to_string());
    }
    format_clock(ended_at)
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
    let seconds = seconds.max(0);
    // Rounding to the nearest minute reports everything under half a minute as no time at all,
    // and one-second polling produces plenty of those: a stretch of rapid window switching became
    // a column of identical zeroes crowding out the blocks that held the day. Seconds are shown
    // only below a minute, where minutes have nothing left to say.
    if seconds < 60 {
        return format!("{seconds}s");
    }

    let minutes = (seconds + 30) / 60;
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
    use super::{
        ApplicationTotal, application_totals, day_bounds, local_date, render_day, retention_cutoff,
    };
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
    fn a_retention_window_opens_at_a_local_midnight_that_many_days_back() {
        let (start_of_today, _) = day_bounds(date(2026, 7, 29)).expect("bounds");
        let afternoon = start_of_today + 15 * 3600;

        let cutoff = retention_cutoff(afternoon, 90).expect("cutoff");

        assert_eq!(
            local_date(cutoff).expect("cutoff date"),
            date(2026, 4, 30),
            "ninety days before 2026-07-29 is 2026-04-30"
        );
        assert_eq!(
            cutoff,
            day_bounds(date(2026, 4, 30)).expect("bounds").0,
            "the window must open exactly where that day opens, not part-way through it"
        );
    }

    #[test]
    fn the_time_of_day_does_not_move_the_retention_boundary() {
        let (start_of_today, end_of_today) = day_bounds(date(2026, 7, 29)).expect("bounds");

        let at_midnight = retention_cutoff(start_of_today, 30).expect("cutoff");
        let last_second = retention_cutoff(end_of_today - 1, 30).expect("cutoff");

        assert_eq!(
            at_midnight, last_second,
            "pruning twice in one day must not remove a further day the second time"
        );
    }

    #[test]
    fn the_shortest_window_still_keeps_the_day_before_today() {
        let (start_of_today, _) = day_bounds(date(2026, 7, 29)).expect("bounds");

        let cutoff = retention_cutoff(start_of_today, 1).expect("cutoff");

        assert_eq!(
            local_date(cutoff).expect("cutoff date"),
            date(2026, 7, 28),
            "a window of one day keeps yesterday and today, so the earliest kept instant is \
             yesterday's first"
        );
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
    fn a_segment_reaching_the_end_of_the_day_says_so_rather_than_saying_midnight() {
        let (start, end) = day_bounds(date(2026, 7, 20)).expect("bounds");

        let rendered =
            render_day(date(2026, 7, 20), &[segment(start, end, "ghostty")]).expect("render");

        assert!(
            rendered.contains("00:00-24:00"),
            "a segment covering the whole day must not read as beginning and ending at the same \
             instant: {rendered}"
        );
        assert!(
            rendered.contains("24h"),
            "the day it claims and the hours it shows have to agree: {rendered}"
        );
    }

    #[test]
    fn a_segment_clipped_at_midnight_says_which_midnight() {
        let (_, end) = day_bounds(date(2026, 7, 20)).expect("bounds");

        let rendered =
            render_day(date(2026, 7, 20), &[segment(end - 600, end, "ghostty")]).expect("render");

        assert!(
            rendered.contains("-24:00"),
            "a segment running into the following day ends at the end of this one: {rendered}"
        );
    }

    /// The counterpart: only the end of the day is ambiguous. A segment that runs in from the
    /// previous day starts at the day's first instant, and `00:00` is what that instant is.
    #[test]
    fn the_start_of_the_day_is_still_the_start_of_the_day() {
        let (start, _) = day_bounds(date(2026, 7, 20)).expect("bounds");

        let rendered = render_day(date(2026, 7, 20), &[segment(start, start + 600, "ghostty")])
            .expect("render");

        assert!(
            rendered.contains("00:00-00:10"),
            "the first instant of the day is midnight and reads as it: {rendered}"
        );
    }

    #[test]
    fn a_visit_shorter_than_a_minute_reports_the_seconds_it_lasted() {
        let rendered = render_day(date(2026, 7, 20), &[segment(0, 12, "ghostty")]).expect("render");
        let row = rendered
            .lines()
            .find(|line| line.contains("ghostty - "))
            .expect("the visit is reported");

        assert!(
            row.contains("12s"),
            "twelve seconds in a window is twelve seconds, not nothing: {row}"
        );
        assert!(
            !row.contains("0m"),
            "rounding a short visit to zero minutes hides it among the blocks that carry real \
             time: {row}"
        );
    }

    /// A segment can genuinely last no time, and more than one thing produces that.
    ///
    /// A focus change with no input during an idle wait closes the displaced window at the very
    /// instant it opened. Startup recovery does the same to a segment the daemon only ever
    /// observed once, and that one is the last application focused before the daemon died, which
    /// is exactly what someone reconstructing a crash is looking for. Nothing in a stored row
    /// says which of the two it was, so a report that hid them would be hiding the second to
    /// tidy away the first. `0s` says what happened without pretending it was rounded.
    #[test]
    fn a_segment_that_lasted_no_time_says_so_rather_than_disappearing() {
        let rendered = render_day(
            date(2026, 7, 20),
            &[
                segment(0, 600, "ghostty"),
                segment(600, 600, "displaced"),
                segment(600, 1200, "firefox"),
            ],
        )
        .expect("render");

        // The row itself, not the whole report: `10m` contains `0m`, so a search over the
        // rendered day would find the neighbouring blocks and pass for the wrong reason.
        let row = rendered
            .lines()
            .find(|line| line.contains("displaced"))
            .expect("a segment that lasted no time is still something that was recorded");

        assert!(
            row.contains("0s"),
            "no time at all reads as no seconds, not as no minutes: {row}"
        );
        assert!(
            !row.contains("0m"),
            "`0m` reads as a duration lost to rounding, which is a different thing: {row}"
        );
    }

    /// The report and the export must agree on whether a day happened at all.
    ///
    /// Filtering the rows made a day whose every segment lasted no time claim that nothing was
    /// recorded, while `daytrace export` listed those same segments. Both halves now describe
    /// the same stored rows.
    #[test]
    fn a_day_holding_only_zero_length_segments_does_not_claim_to_be_empty() {
        let rendered = render_day(date(2026, 7, 20), &[segment(600, 600, "last-before-crash")])
            .expect("render");

        assert!(
            rendered.contains("last-before-crash"),
            "a recorded segment must not be reported as nothing having been recorded: {rendered}"
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
