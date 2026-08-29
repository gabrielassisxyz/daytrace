use crate::activity::{ActivityKind, MediaSegment, TimelineSegment};
use crate::narrative::{self, BackgroundMedia, Block, TitlePart};
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
    sort_totals_longest_first(&mut totals);
    totals
}

/// Longest first, and equal totals by name so the same day reads the same way twice. The name
/// comparison ignores case: application classes arrive in whichever case the compositor
/// reports, and raw byte order would sort every capitalised one above every lowercase one.
///
/// Shared between `application_totals` (raw segments) and `application_totals_from_blocks`
/// (the narrative), so the two orderings cannot drift apart from each other independently.
fn sort_totals_longest_first(totals: &mut [ApplicationTotal]) {
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
}

/// `Time per application`, totalled from the blocks rather than the raw segments.
///
/// Swallowing moves seconds between applications, so a total taken from the rows would
/// contradict the timeline printed above it: `block.label` is `application_label` under
/// another name, kept in step with it because grouping is built on that same function.
fn application_totals_from_blocks(blocks: &[Block]) -> Vec<ApplicationTotal> {
    let mut seconds_by_label: BTreeMap<&str, i64> = BTreeMap::new();
    for block in blocks {
        *seconds_by_label.entry(block.label.as_str()).or_default() +=
            block.ended_at.saturating_sub(block.started_at).max(0);
    }

    let mut totals: Vec<ApplicationTotal> = seconds_by_label
        .into_iter()
        .map(|(label, seconds)| ApplicationTotal {
            label: label.to_string(),
            seconds,
        })
        .collect();
    sort_totals_longest_first(&mut totals);
    totals
}

/// Render one day: the desktop timeline and its per-application totals, unchanged from before
/// media existed, followed by a media section when the day held any.
///
/// The two sources stay apart rather than sharing one list or one total, because media
/// overlaps with the desktop and with itself: a track played behind a window is real time
/// twice over, and a total that summed both would claim more than the day held. Reconciling
/// them into one narrative is the aggregation layer's job, not this one's.
///
/// `today` renders `render_narrative_day` instead as of the aggregation layer; this stays for
/// `today --raw` to print byte for byte, which is a later bead in that layer, and for the tests
/// that check the aggregated view against it. `#[allow(dead_code)]` because that flag does not
/// exist yet, so nothing in the production binary reaches this until it does.
#[allow(dead_code)]
pub fn render_day(
    date: NaiveDate,
    segments: &[TimelineSegment],
    media: &[MediaSegment],
) -> Result<String, String> {
    // A row needs the end of the reported day to tell a boundary clip from a segment that
    // genuinely stopped at midnight, and only the day being rendered knows where that is.
    let (_, day_end) = day_bounds(date)?;

    if segments.is_empty() && media.is_empty() {
        return Ok(format!("No activity events recorded for {date}.\n"));
    }

    let mut output = format!("Timeline for {date}\n");
    for segment in segments {
        output.push_str(&format_segment(segment, day_end)?);
        output.push('\n');
    }

    if !segments.is_empty() {
        output.push_str("\nTime per application\n");
        for total in application_totals(segments) {
            let duration = format_duration(total.seconds);
            output.push_str(&format!("{duration:>6}  {}\n", total.label));
        }
    }

    if !media.is_empty() {
        if !segments.is_empty() {
            output.push('\n');
        }
        output.push_str("Media playing\n");
        for entry in media {
            output.push_str(&format_media_segment(entry, day_end)?);
            output.push('\n');
        }
        output.push('\n');
        let total = format_duration(media_playing_seconds(media));
        output.push_str(&format!("{total:>6}  Total\n"));
    }

    Ok(output)
}

/// The width of `{start}-{end}`, both of which always format to `HH:MM`: two five-character
/// clocks and the dash between them. A sub-line's leading blank replaces exactly this many
/// columns, so a duration below a block sits under the block's own without depending on either
/// clock reading.
const CLOCK_RANGE_WIDTH: usize = 11;

/// Render one day as the narrative: blocks with their title sub-lines and background media,
/// `Time per application` totalled from those blocks, and the `Media playing` section unchanged.
///
/// The media section shares `format_media_segment` and `media_playing_seconds` with
/// [`render_day`] rather than a copy, which is what keeps decision 6 (the section stays
/// byte-identical) true by construction instead of by a golden test alone.
pub fn render_narrative_day(
    date: NaiveDate,
    segments: &[TimelineSegment],
    media: &[MediaSegment],
) -> Result<String, String> {
    let (_, day_end) = day_bounds(date)?;

    if segments.is_empty() && media.is_empty() {
        return Ok(format!("No activity events recorded for {date}.\n"));
    }

    let narrative = narrative::build_day(segments, media);

    let mut output = format!("Timeline for {date}\n");
    for block in &narrative.blocks {
        output.push_str(&format_block(block, day_end)?);
        output.push('\n');
    }

    if !segments.is_empty() {
        output.push_str("\nTime per application\n");
        for total in application_totals_from_blocks(&narrative.blocks) {
            let duration = format_duration(total.seconds);
            output.push_str(&format!("{duration:>6}  {}\n", total.label));
        }
    }

    if !media.is_empty() {
        if !segments.is_empty() {
            output.push('\n');
        }
        output.push_str("Media playing\n");
        for entry in media {
            output.push_str(&format_media_segment(entry, day_end)?);
            output.push('\n');
        }
        output.push('\n');
        let total = format_duration(media_playing_seconds(media));
        output.push_str(&format!("{total:>6}  Total\n"));
    }

    Ok(output)
}

/// One block: its line, then its title sub-lines when it has more than one, exactly as
/// `format_segment` already distinguishes a window's title from every other kind's fixed label.
fn format_block(block: &Block, day_end: i64) -> Result<String, String> {
    let started = format_clock(block.started_at)?;
    let ended = format_end_clock(block.ended_at, day_end)?;
    let duration = format_duration(block.ended_at.saturating_sub(block.started_at));
    let parts = block.title_parts();
    let label = block_line_label(block, &parts);
    let mut rendered = format!("{started}-{ended}  {duration:<6}  {label}");

    if block.kind == ActivityKind::Window && parts.len() > 1 {
        for part in &parts {
            rendered.push('\n');
            rendered.push_str(&format_title_part(part));
        }
    }

    Ok(rendered)
}

/// The block line's label: the application or absence name, its title when a window block has
/// exactly one (the same shape a raw window row already prints), and the background suffix.
fn block_line_label(block: &Block, parts: &[TitlePart]) -> String {
    let mut label = block.label.clone();
    if block.kind == ActivityKind::Window
        && let [TitlePart::Title { title, .. }] = parts
    {
        label.push_str(" - ");
        label.push_str(title);
    }
    if let Some(background) = &block.background {
        label.push_str(&format_background_suffix(background));
    }
    label
}

/// `, <player> playing in the background`, with `and N more` appended when other players also
/// cleared the floor, naming only the one that overlapped the block longest.
fn format_background_suffix(background: &BackgroundMedia) -> String {
    let mut suffix = format!(", {} playing in the background", background.player);
    if background.other_player_count > 0 {
        suffix.push_str(&format!(" and {} more", background.other_player_count));
    }
    suffix
}

/// One title beneath a block: the clock range blanked out, its duration under the block's own,
/// and the title indented two columns past where the block label starts.
fn format_title_part(part: &TitlePart) -> String {
    let blank_range = " ".repeat(CLOCK_RANGE_WIDTH);
    let duration = format_duration(part.duration_seconds());
    let text = title_part_text(part);
    format!("{blank_range}  {duration:<6}    {text}")
}

fn title_part_text(part: &TitlePart) -> String {
    match part {
        TitlePart::Title { title, .. } => title.clone(),
        TitlePart::Remainder { title_count, .. } => format!("other ({title_count} titles)"),
    }
}

/// One row of the media section, at the timeline's own column widths so the two sections read
/// as one report.
fn format_media_segment(segment: &MediaSegment, day_end: i64) -> Result<String, String> {
    let started = format_clock(segment.started_at)?;
    let ended = format_end_clock(segment.ended_at, day_end)?;
    let duration = format_duration(segment.ended_at.saturating_sub(segment.started_at));
    let label = media_label(segment);
    Ok(format!("{started}-{ended}  {duration:<6}  {label}"))
}

/// `<player> - <title> - <artist>`, with the artist and its separator dropped when there is no
/// artist, and the title itself falling back first to the address and then to a fixed label,
/// mirroring how a window with no class already reads as `unknown app`.
fn media_label(segment: &MediaSegment) -> String {
    let player = segment
        .snapshot
        .player
        .as_deref()
        .unwrap_or("unknown player");
    let title = segment
        .snapshot
        .title
        .as_deref()
        .or(segment.snapshot.item_url.as_deref())
        .unwrap_or("unknown media");

    match segment.snapshot.artist.as_deref() {
        Some(artist) => format!("{player} - {title} - {artist}"),
        None => format!("{player} - {title}"),
    }
}

/// The time media held the day, counting a stretch during which more than one player was
/// reporting once rather than once per player.
///
/// Summing each segment's own length would let two players playing at once double the total,
/// which is exactly the double count the media section exists apart from the desktop timeline
/// to avoid; nothing makes that hazard exclusive to the boundary between the two sources.
fn media_playing_seconds(media: &[MediaSegment]) -> i64 {
    let mut intervals: Vec<(i64, i64)> = media
        .iter()
        .map(|segment| (segment.started_at, segment.ended_at.max(segment.started_at)))
        .collect();
    intervals.sort_unstable_by_key(|&(start, _)| start);

    let mut total = 0i64;
    let mut open: Option<(i64, i64)> = None;
    for (start, end) in intervals {
        open = Some(match open {
            Some((open_start, open_end)) if start <= open_end => (open_start, open_end.max(end)),
            Some((open_start, open_end)) => {
                total += open_end - open_start;
                (start, end)
            }
            None => (start, end),
        });
    }
    if let Some((start, end)) = open {
        total += end - start;
    }
    total
}

/// What to call the thing that held the time, for both a timeline row and a total.
///
/// One source for both, because a row and a total that disagree about the name of an
/// application read as two different applications in the same report. `pub(crate)` rather than
/// private: `narrative::group_into_blocks` keys a block by this same name, and a second copy is
/// how a row and a total came to name the same application differently before this bead.
pub(crate) fn application_label(segment: &TimelineSegment) -> &str {
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
        ApplicationTotal, application_totals, application_totals_from_blocks, day_bounds,
        local_date, media_playing_seconds, render_day, render_narrative_day, retention_cutoff,
    };
    use crate::activity::{ActivitySnapshot, MediaSegment, MediaSnapshot, TimelineSegment};
    use crate::narrative::build_day;
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

    /// A window segment with its own title, for the narrative tests below: unlike `segment`,
    /// which fixes the title to make every desktop row read the same, a block's title parts
    /// need segments that genuinely differ.
    fn window_titled(started_at: i64, ended_at: i64, app: &str, title: &str) -> TimelineSegment {
        TimelineSegment {
            started_at,
            ended_at,
            snapshot: ActivitySnapshot::window(
                Some(app.to_string()),
                Some(title.to_string()),
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
        let rendered = render_day(
            date(2026, 7, 20),
            &[segment(start, start + 600, "ghostty")],
            &[],
        )
        .expect("render");

        assert!(
            rendered.starts_with("Timeline for 2026-07-20\n"),
            "a report for a past day must not be labelled with another date: {rendered}"
        );
        assert!(rendered.contains("ghostty"), "{rendered}");
    }

    #[test]
    fn an_empty_day_says_which_day_was_empty() {
        let rendered = render_day(date(2026, 7, 20), &[], &[]).expect("render");

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

        let rendered = render_day(date(2026, 7, 20), &short_visits, &[]).expect("render");
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

        let rendered = render_day(date(2026, 7, 20), &segments, &[]).expect("render");

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
            render_day(date(2026, 7, 20), &[segment(start, end, "ghostty")], &[]).expect("render");

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

        let rendered = render_day(
            date(2026, 7, 20),
            &[segment(end - 600, end, "ghostty")],
            &[],
        )
        .expect("render");

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

        let rendered = render_day(
            date(2026, 7, 20),
            &[segment(start, start + 600, "ghostty")],
            &[],
        )
        .expect("render");

        assert!(
            rendered.contains("00:00-00:10"),
            "the first instant of the day is midnight and reads as it: {rendered}"
        );
    }

    #[test]
    fn a_visit_shorter_than_a_minute_reports_the_seconds_it_lasted() {
        let rendered =
            render_day(date(2026, 7, 20), &[segment(0, 12, "ghostty")], &[]).expect("render");
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
            &[],
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
        let rendered = render_day(
            date(2026, 7, 20),
            &[segment(600, 600, "last-before-crash")],
            &[],
        )
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
            &[],
        )
        .expect("render");

        let chronology = rendered.find("ghostty - a window").expect("timeline row");
        let totals = rendered.find("Time per application").expect("totals block");
        assert!(
            chronology < totals,
            "the day is read in order first and summed second: {rendered}"
        );
    }

    fn media_segment(
        started_at: i64,
        ended_at: i64,
        player: Option<&str>,
        title: Option<&str>,
        artist: Option<&str>,
        item_url: Option<&str>,
    ) -> MediaSegment {
        MediaSegment {
            started_at,
            ended_at,
            snapshot: MediaSnapshot {
                player: player.map(str::to_string),
                title: title.map(str::to_string),
                artist: artist.map(str::to_string),
                album: None,
                item_url: item_url.map(str::to_string),
            },
        }
    }

    // Golden fixtures over a fixed day, offsets kept relative to `day_bounds` so the expected
    // clock readings do not depend on the timezone the test happens to run under.

    fn desktop_only_fixture() -> (NaiveDate, i64, Vec<TimelineSegment>) {
        let day = date(2026, 7, 20);
        let (start, _) = day_bounds(day).expect("bounds");
        let segments = vec![
            TimelineSegment {
                started_at: start,
                ended_at: start + 1_440,
                snapshot: ActivitySnapshot::window(
                    Some("ghostty".to_string()),
                    Some("tmux".to_string()),
                    Some("3".to_string()),
                    Some(1),
                ),
            },
            TimelineSegment {
                started_at: start + 1_440,
                ended_at: start + 2_460,
                snapshot: ActivitySnapshot::window(
                    Some("firefox".to_string()),
                    Some("Inbox - Brave".to_string()),
                    None,
                    None,
                ),
            },
            idle_segment(start + 2_460, start + 3_420),
        ];
        (day, start, segments)
    }

    fn media_only_fixture(start: i64) -> Vec<MediaSegment> {
        vec![media_segment(
            start + 1_440,
            start + 2_880,
            Some("spotify"),
            Some("Track title"),
            Some("Artist"),
            None,
        )]
    }

    /// The desktop block alone, exactly as the renderer produced it before this bead: a
    /// backslash-continued string literal eats the leading whitespace of the following line,
    /// which would silently swallow the total block's own indentation, so the lines are joined
    /// explicitly instead.
    fn desktop_only_golden() -> String {
        [
            "Timeline for 2026-07-20",
            "00:00-00:24  24m     ghostty - tmux workspace 3, monitor 1",
            "00:24-00:41  17m     firefox - Inbox - Brave",
            "00:41-00:57  16m     AFK",
            "",
            "Time per application",
            "   24m  ghostty",
            "   17m  firefox",
            "   16m  AFK",
            "",
        ]
        .join("\n")
    }

    fn media_block_golden() -> String {
        [
            "Media playing",
            "00:24-00:48  24m     spotify - Track title - Artist",
            "",
            "   24m  Total",
            "",
        ]
        .join("\n")
    }

    /// A regression fixture captured from the renderer before this bead added the media
    /// section, so a day with no media proves it still renders the same bytes rather than
    /// merely a similar-looking string reconstructed after the change.
    #[test]
    fn a_day_with_no_media_renders_exactly_as_it_did_before_media_existed() {
        let (day, _, segments) = desktop_only_fixture();

        let rendered = render_day(day, &segments, &[]).expect("render");

        assert_eq!(rendered, desktop_only_golden());
    }

    #[test]
    fn a_media_only_day_golden() {
        let (day, start, _) = desktop_only_fixture();
        let media = media_only_fixture(start);

        let rendered = render_day(day, &[], &media).expect("render");

        assert_eq!(
            rendered,
            format!("Timeline for {day}\n{}", media_block_golden())
        );
    }

    #[test]
    fn a_mixed_day_golden_puts_media_after_the_desktop_totals() {
        let (day, start, segments) = desktop_only_fixture();
        let media = media_only_fixture(start);

        let rendered = render_day(day, &segments, &media).expect("render");

        assert_eq!(
            rendered,
            format!("{}\n{}", desktop_only_golden(), media_block_golden())
        );
    }

    #[test]
    fn an_empty_day_with_neither_source_is_unchanged() {
        let rendered = render_day(date(2026, 7, 20), &[], &[]).expect("render");

        assert_eq!(rendered, "No activity events recorded for 2026-07-20.\n");
    }

    // Narrative rendering: `render_narrative_day`, the path `today` calls as of this bead.

    /// A day built to actually contain the cases the golden test has to reach, not merely claim
    /// to: a block with a background (`firefox`, overlapped by `spotify` well past the floor), a
    /// block with more titles than the five-title cap (seven distinct pages, so a remainder line
    /// is unavoidable), and a block a foreign focus was swallowed into (`kitty`, with a
    /// three-second `rofi` window folded into it). A fourth block (`zed`) carries three media
    /// players overlapping it at once, so `and N more` is reachable, and a fifth (`Suspended`)
    /// closes the day with the longest single total.
    ///
    /// The floor is exercised by the `AFK` block, which `vlc` overlaps for thirty seconds and so
    /// never names. That segment is the golden's only rejection case, which is why it is called
    /// out here: a later reader trimming it as redundant would remove the coverage silently.
    fn narrative_mixed_day_fixture() -> (NaiveDate, i64, Vec<TimelineSegment>, Vec<MediaSegment>) {
        let day = date(2026, 7, 20);
        let (start, _) = day_bounds(day).expect("bounds");

        let segments = vec![
            // firefox: seven distinct titles, longest first once sorted, so titles "z".."e" are
            // kept and "d"/"c" (the two shortest) become the remainder.
            window_titled(start, start + 600, "firefox", "z"),
            window_titled(start + 600, start + 1_080, "firefox", "y"),
            window_titled(start + 1_080, start + 1_440, "firefox", "x"),
            window_titled(start + 1_440, start + 1_740, "firefox", "w"),
            window_titled(start + 1_740, start + 1_980, "firefox", "v"),
            window_titled(start + 1_980, start + 2_160, "firefox", "u"),
            window_titled(start + 2_160, start + 2_280, "firefox", "t"),
            idle_segment(start + 2_280, start + 2_880),
            window_titled(start + 2_880, start + 3_060, "zed", "notes"),
            window_titled(start + 3_060, start + 3_660, "kitty", "editing"),
            window_titled(start + 3_660, start + 3_663, "rofi", "quick check"),
            window_titled(start + 3_663, start + 4_263, "kitty", "editing"),
            TimelineSegment {
                started_at: start + 4_263,
                ended_at: start + 7_863,
                snapshot: ActivitySnapshot::suspended(),
            },
        ];

        let media = vec![
            // Clears the floor on the firefox block alone.
            media_segment(start + 100, start + 190, Some("spotify"), None, None, None),
            // Overlaps the AFK block for 30s, under the floor: listed in Media playing, named
            // on no block line.
            media_segment(start + 2_290, start + 2_320, Some("vlc"), None, None, None),
            // Three players over the zed block: brave overlaps longest, so "and 2 more".
            media_segment(
                start + 2_880,
                start + 3_050,
                Some("brave"),
                None,
                None,
                None,
            ),
            media_segment(start + 2_900, start + 2_990, Some("mpv"), None, None, None),
            media_segment(
                start + 2_950,
                start + 3_015,
                Some("mplayer"),
                None,
                None,
                None,
            ),
        ];

        (day, start, segments, media)
    }

    #[test]
    fn a_mixed_narrative_day_golden() {
        let (day, _, segments, media) = narrative_mixed_day_fixture();

        let rendered = render_narrative_day(day, &segments, &media).expect("render");

        assert_eq!(
            rendered,
            [
                "Timeline for 2026-07-20",
                "00:00-00:38  38m     firefox, spotify playing in the background",
                "             10m       z",
                "             8m        y",
                "             6m        x",
                "             5m        w",
                "             4m        v",
                "             5m        other (2 titles)",
                "00:38-00:48  10m     AFK",
                "00:48-00:51  3m      zed - notes, brave playing in the background and 2 more",
                "00:51-01:11  20m     kitty",
                "             20m       editing",
                "             3s        quick check",
                "01:11-02:11  1h      Suspended",
                "",
                "Time per application",
                "    1h  Suspended",
                "   38m  firefox",
                "   20m  kitty",
                "   10m  AFK",
                "    3m  zed",
                "",
                "Media playing",
                "00:01-00:03  2m      spotify - unknown media",
                "00:38-00:38  30s     vlc - unknown media",
                "00:48-00:50  3m      brave - unknown media",
                "00:48-00:49  2m      mpv - unknown media",
                "00:49-00:50  1m      mplayer - unknown media",
                "",
                "    5m  Total",
                "",
            ]
            .join("\n"),
            "actual rendering: {rendered}"
        );
    }

    /// A block with exactly one title reads exactly as a raw window row does, minus the
    /// workspace and monitor location the output shape drops: the narrative names an
    /// application and a title, not where on the desktop it sat.
    #[test]
    fn a_narrative_day_with_one_title_per_block_golden() {
        let (day, _, segments) = desktop_only_fixture();

        let rendered = render_narrative_day(day, &segments, &[]).expect("render");

        assert_eq!(
            rendered,
            [
                "Timeline for 2026-07-20",
                "00:00-00:24  24m     ghostty - tmux",
                "00:24-00:41  17m     firefox - Inbox - Brave",
                "00:41-00:57  16m     AFK",
                "",
                "Time per application",
                "   24m  ghostty",
                "   17m  firefox",
                "   16m  AFK",
                "",
            ]
            .join("\n"),
            "actual rendering: {rendered}"
        );
    }

    /// Six distinct titles is one past the five-title cap: the smallest case that actually
    /// forces a remainder line, rather than one so large the cap's edge is never approached.
    #[test]
    fn a_narrative_block_with_six_titles_prints_five_and_a_remainder() {
        let day = date(2026, 7, 20);
        let (start, _) = day_bounds(day).expect("bounds");
        let segments: Vec<TimelineSegment> = (0..6)
            .map(|index| {
                window_titled(
                    start + index * 60,
                    start + index * 60 + 60,
                    "firefox",
                    &format!("title {index}"),
                )
            })
            .collect();

        let rendered = render_narrative_day(day, &segments, &[]).expect("render");

        assert_eq!(
            rendered,
            [
                "Timeline for 2026-07-20",
                "00:00-00:06  6m      firefox",
                "             1m        title 0",
                "             1m        title 1",
                "             1m        title 2",
                "             1m        title 3",
                "             1m        title 4",
                "             1m        other (1 titles)",
                "",
                "Time per application",
                "    6m  firefox",
                "",
            ]
            .join("\n"),
            "actual rendering: {rendered}"
        );
    }

    #[test]
    fn an_empty_day_through_the_narrative_path_is_unchanged() {
        let rendered = render_narrative_day(date(2026, 7, 20), &[], &[]).expect("render");

        assert_eq!(rendered, "No activity events recorded for 2026-07-20.\n");
    }

    /// Decision 6: the `Media playing` section stays byte-identical under the narrative path,
    /// and a media-only day still skips the empty timeline and the empty totals heading.
    #[test]
    fn a_media_only_day_through_the_narrative_path_matches_the_media_section() {
        let (day, start, _) = desktop_only_fixture();
        let media = media_only_fixture(start);

        let rendered = render_narrative_day(day, &[], &media).expect("render");

        assert_eq!(
            rendered,
            format!("Timeline for {day}\n{}", media_block_golden())
        );
    }

    /// Decision 5, proved directly rather than only through the golden above: a total taken from
    /// the raw segments still names the swallowed application on its own, while the total taken
    /// from the blocks folds its seconds into the block that absorbed it and never names it at
    /// all. The two must disagree here, or the totals are not actually sourced from the blocks.
    #[test]
    fn time_per_application_is_sourced_from_blocks_not_raw_segments_when_swallowing_moves_seconds()
    {
        let segments = vec![
            segment(0, 600, "term"),
            segment(600, 603, "popup"),
            segment(603, 1_203, "term"),
        ];

        let raw_totals = application_totals(&segments);
        assert!(
            raw_totals.iter().any(|total| total.label == "popup"),
            "the raw segments still name the three seconds on their own: {raw_totals:?}"
        );

        let narrative = build_day(&segments, &[]);
        let block_totals = application_totals_from_blocks(&narrative.blocks);
        assert_eq!(
            block_totals,
            vec![total("term", 1_203)],
            "swallowing must fold the popup's seconds into term and leave no total under \
             popup's own name: {block_totals:?}"
        );
    }

    #[test]
    fn a_day_with_media_and_no_desktop_rows_skips_straight_to_the_media_section() {
        let (day, start, _) = desktop_only_fixture();
        let media = media_only_fixture(start);

        let rendered = render_day(day, &[], &media).expect("render");

        assert!(
            rendered.starts_with(&format!("Timeline for {day}\nMedia playing\n")),
            "a day with media and no desktop rows must not print an empty timeline or an empty \
             totals heading before the media section: {rendered}"
        );
        assert!(
            !rendered.contains("No activity events recorded"),
            "a day that held media happened, even with nothing on the desktop side: {rendered}"
        );
    }

    /// The media row's own boundary clipping has to read exactly as a desktop row's does: a
    /// media segment reaching the end of the reported day says `24:00` rather than `00:00`, the
    /// same distinction `format_end_clock` exists to make for the desktop timeline above it.
    #[test]
    fn a_media_row_reaching_the_end_of_the_day_says_so_rather_than_saying_midnight() {
        let day = date(2026, 7, 20);
        let (start, end) = day_bounds(day).expect("bounds");
        let media = vec![media_segment(
            start,
            end,
            Some("spotify"),
            Some("Full Day"),
            None,
            None,
        )];

        let rendered = render_day(day, &[], &media).expect("render");

        assert!(
            rendered.contains("00:00-24:00"),
            "a media row covering the whole day must not read as beginning and ending at the \
             same instant: {rendered}"
        );
        assert!(
            rendered.contains("24h"),
            "the span shown and the hours claimed have to agree: {rendered}"
        );
    }

    #[test]
    fn a_media_entry_omits_the_artist_and_its_separator_when_there_is_none() {
        let (day, start, _) = desktop_only_fixture();
        let media = vec![media_segment(
            start,
            start + 60,
            Some("spotify"),
            Some("Track title"),
            None,
            None,
        )];

        let rendered = render_day(day, &[], &media).expect("render");

        assert!(
            rendered.contains("spotify - Track title\n"),
            "no artist must drop the separator along with it, not leave a trailing dash: \
             {rendered}"
        );
        assert!(!rendered.contains(" - Track title - "), "{rendered}");
    }

    #[test]
    fn a_media_entry_with_no_title_falls_back_to_the_address() {
        let (day, start, _) = desktop_only_fixture();
        let media = vec![media_segment(
            start,
            start + 60,
            Some("spotify"),
            None,
            None,
            Some("https://example.test/stream"),
        )];

        let rendered = render_day(day, &[], &media).expect("render");

        assert!(
            rendered.contains("spotify - https://example.test/stream\n"),
            "no title must fall back to the address rather than to a placeholder: {rendered}"
        );
    }

    #[test]
    fn a_media_entry_with_neither_title_nor_address_reads_as_unknown_media() {
        let (day, start, _) = desktop_only_fixture();
        let media = vec![media_segment(
            start,
            start + 60,
            Some("spotify"),
            None,
            None,
            None,
        )];

        let rendered = render_day(day, &[], &media).expect("render");

        assert!(
            rendered.contains("spotify - unknown media\n"),
            "with nothing to name the track, the row still names the player: {rendered}"
        );
    }

    #[test]
    fn media_playing_seconds_counts_an_overlap_once_rather_than_per_player() {
        let media = vec![
            media_segment(0, 1_200, Some("spotify"), None, None, None),
            media_segment(600, 1_800, Some("brave"), None, None, None),
        ];

        assert_eq!(
            media_playing_seconds(&media),
            1_800,
            "two players overlapping from 600 to 1200 must not double that stretch: the wall \
             clock covered is 0 to 1800, eighteen hundred seconds, not the sum of both durations"
        );
    }

    #[test]
    fn media_playing_seconds_sums_disjoint_stretches() {
        let media = vec![
            media_segment(0, 600, Some("spotify"), None, None, None),
            media_segment(1_000, 1_300, Some("brave"), None, None, None),
        ];

        assert_eq!(media_playing_seconds(&media), 900);
    }

    /// A day where media and desktop overlap for most of it must still keep every total the
    /// report prints at or under the length of the day, whichever source produced it, and
    /// whichever players overlapped each other. Extended to the narrative path: the same
    /// generated day, rendered through `render_narrative_day`, must keep its own
    /// `Time per application` (sourced from blocks rather than raw segments) under the same
    /// ceiling, and media attached to a block must not have raised it. Breaking that on purpose,
    /// by letting `application_totals_from_blocks` add a block's `background` overlap into its
    /// seconds, turns this loop red well before 200 iterations.
    #[test]
    fn no_total_the_report_prints_exceeds_the_length_of_the_day() {
        let day = date(2026, 7, 20);
        let (day_start, day_end) = day_bounds(day).expect("bounds");
        let day_length = day_end - day_start;
        let mut state: u64 = 0x2026_0720_da91_face;

        for _ in 0..200 {
            let desktop = generated_desktop_segments(&mut state, day_start, day_end);
            let media = generated_media_segments(&mut state, day_start, day_end);

            let rendered = render_day(day, &desktop, &media).expect("render");

            let desktop_total: i64 = application_totals(&desktop)
                .iter()
                .map(|total| total.seconds)
                .sum();
            assert!(
                desktop_total <= day_length,
                "the desktop totals summed to {desktop_total}s, more than the day's \
                 {day_length}s: {rendered}"
            );

            let media_total = media_playing_seconds(&media);
            assert!(
                media_total <= day_length,
                "the media section's own total was {media_total}s, more than the day's \
                 {day_length}s: {rendered}"
            );

            let narrative_rendered = render_narrative_day(day, &desktop, &media).expect("render");
            let narrative = build_day(&desktop, &media);
            let block_total: i64 = application_totals_from_blocks(&narrative.blocks)
                .iter()
                .map(|total| total.seconds)
                .sum();
            assert!(
                block_total <= day_length,
                "the block totals summed to {block_total}s, more than the day's {day_length}s: \
                 {narrative_rendered}"
            );
            assert_eq!(
                block_total, desktop_total,
                "grouping and swallowing must not change how many desktop seconds the day \
                 totals to, only which application some of them are credited to: \
                 {narrative_rendered}"
            );
        }
    }

    /// A small, dependency-free linear congruential generator: the property test above needs
    /// many varied inputs, not a cryptographically sound one, and pulling in a fuzzing crate
    /// for one test would ask every future audit of this crate's dependencies to account for it.
    fn lcg_next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        *state
    }

    /// Non-overlapping desktop segments walking the whole day, the way capture actually
    /// produces them: one lane, one row open at a time.
    fn generated_desktop_segments(
        state: &mut u64,
        day_start: i64,
        day_end: i64,
    ) -> Vec<TimelineSegment> {
        let apps = ["ghostty", "firefox", "zed"];
        let mut segments = Vec::new();
        let mut cursor = day_start;
        while cursor < day_end {
            let duration = 1 + (lcg_next(state) % 3_600) as i64;
            let end = (cursor + duration).min(day_end);
            let app = apps[(lcg_next(state) % apps.len() as u64) as usize];
            segments.push(segment(cursor, end, app));
            cursor = end;
        }
        segments
    }

    /// Media segments placed freely within the day, deliberately allowed to overlap each other
    /// and the desktop: a second player genuinely can start before the first one stops.
    fn generated_media_segments(
        state: &mut u64,
        day_start: i64,
        day_end: i64,
    ) -> Vec<MediaSegment> {
        let day_length = (day_end - day_start).max(1);
        // Every start falls inside a narrow window near the day's own start, rather than
        // scattered uniformly across it: uniformly scattered starts rarely overlap enough for
        // the naive sum of their durations to reach the day's length, so a property test built
        // that way would never exercise the case the ceiling exists to guard. Clustering many
        // segments this close together makes that naive sum reach well past the day even
        // though the true wall-clock coverage of the cluster cannot.
        let cluster_window = (day_length / 8).max(1) as u64;
        (0..40)
            .map(|index| {
                let start = day_start + (lcg_next(state) % cluster_window) as i64;
                let duration = 1 + (lcg_next(state) % 14_400) as i64;
                let end = (start + duration).min(day_end);
                let player = format!("player-{}", index % 5);
                media_segment(start, end, Some(&player), Some("Track"), None, None)
            })
            .collect()
    }
}
