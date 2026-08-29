//! Groups a day's desktop segments into narrative blocks: consecutive segments that share one
//! application label, then absorbs a short foreign focus back into the block around it and
//! attaches whichever media held the background. Read-time and stateless, it consumes the two
//! slices `Store::day_activity` already returns and produces an owned value that
//! `timeline::render_narrative_day` walks to render `daytrace today`.

use crate::activity::{ActivityKind, MediaSegment, TimelineSegment};

/// A run of consecutive desktop segments that share one application label.
#[derive(Debug, Eq, PartialEq)]
pub struct Block {
    pub label: String,
    pub kind: ActivityKind,
    pub started_at: i64,
    pub ended_at: i64,
    pub segments: Vec<TimelineSegment>,
    /// Set by `attach_background_media`, never by grouping or swallowing. `None` until that
    /// pass runs, and still `None` afterward for a block no player cleared the floor on.
    pub background: Option<BackgroundMedia>,
}

/// A block's background media: the player that overlapped it longest, and how many other
/// players also cleared `BACKGROUND_MEDIA_FLOOR_SECONDS`.
///
/// One value per block, not a list: decision 1 keeps the desktop lane the sole holder of a
/// block's time and media a single secondary fact riding along, the same shape the browser
/// layer's own competing-source problem will need a table for later, not this one.
#[derive(Debug, Eq, PartialEq)]
pub struct BackgroundMedia {
    pub player: String,
    pub other_player_count: usize,
}

/// A foreign focus shorter than this is swallowed into the block around it, provided the
/// segments on both sides are windows sharing one label different from its own. Measured
/// against the live store rather than guessed: see the bead this constant belongs to.
const SWALLOW_THRESHOLD_SECONDS: i64 = 5;

/// One day's desktop segments, grouped into blocks.
#[derive(Debug, Eq, PartialEq)]
pub struct Narrative {
    pub blocks: Vec<Block>,
}

/// Group the desktop slice of a stored day into blocks, then swallow short foreign focus.
///
/// A segment extends the last block when it shares its label and picks up exactly where the
/// last one left off. A gap between two segments of the same label, meaning the daemon was not
/// running, starts a new block instead of extending the last one: a gap is an absence of
/// evidence rather than a continuation of what came before it, so the block boundaries sit at
/// the stored instants rather than spanning it. Once grouped, a window block under the
/// swallowing threshold sitting between two touching window blocks of one shared, different
/// label is folded into them; see `swallow_short_foreign_blocks`.
///
/// The caller is expected to pass what the store returns: segments ordered by start and never
/// overlapping, which the desktop lane guarantees through its one-open-segment-per-lane index.
/// An unordered or overlapping vector is not rejected, it is grouped as given, so a caller that
/// merges or filters before calling has to preserve both properties itself.
///
/// Media never plays a part here: attaching it is `attach_background_media`, run separately once
/// these boundaries have settled. `build_day` is the one call that runs both in order.
pub fn build_narrative(segments: &[TimelineSegment]) -> Narrative {
    let blocks = group_into_blocks(segments);
    let blocks = swallow_short_foreign_blocks(blocks);
    Narrative { blocks }
}

/// Build one day's narrative from the two slices `Store::day_activity` returns: group and
/// swallow the desktop lane into blocks, then attach whichever media cleared the floor on each
/// one.
///
/// The one call the render path needs, so it does not have to know that media attachment has to
/// run after grouping and swallowing have settled the block boundaries, or scatter that
/// ordering across the module that renders rather than the one that builds.
pub fn build_day(segments: &[TimelineSegment], media: &[MediaSegment]) -> Narrative {
    let mut narrative = build_narrative(segments);
    attach_background_media(&mut narrative.blocks, media);
    narrative
}

/// Group consecutive desktop segments sharing one application label into blocks, with a gap
/// between two segments breaking a block regardless of label.
fn group_into_blocks(segments: &[TimelineSegment]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();

    for segment in segments {
        let label = crate::timeline::application_label(segment);
        let touches_previous = blocks
            .last()
            .is_some_and(|block| block.label == label && block.ended_at == segment.started_at);

        if touches_previous {
            let block = blocks.last_mut().expect("checked above");
            block.ended_at = segment.ended_at;
            block.segments.push(segment.clone());
        } else {
            blocks.push(Block {
                label: label.to_string(),
                kind: segment.snapshot.kind.clone(),
                started_at: segment.started_at,
                ended_at: segment.ended_at,
                segments: vec![segment.clone()],
                background: None,
            });
        }
    }

    blocks
}

/// Absorb a foreign window block shorter than the threshold into the block around it, when the
/// blocks on both sides are windows sharing one label different from its own and touch it in
/// time on both sides. Touching is required, not merely same label: a gap is the daemon not
/// running, and merging across it would claim seconds nothing covered, breaking the invariant
/// that a narrative covers exactly the seconds its input covered.
///
/// Repeats until a full pass changes nothing, so an alternating run of two labels collapses to
/// one block rather than leaving every other short block behind.
fn swallow_short_foreign_blocks(mut blocks: Vec<Block>) -> Vec<Block> {
    loop {
        let mut changed = false;
        let mut index = 1;
        while index + 1 < blocks.len() {
            if is_swallowable(&blocks, index) {
                let right = blocks.remove(index + 1);
                let short = blocks.remove(index);
                let left = blocks
                    .get_mut(index - 1)
                    .expect("checked by is_swallowable");
                left.ended_at = right.ended_at;
                left.segments.extend(short.segments);
                left.segments.extend(right.segments);
                changed = true;
            } else {
                index += 1;
            }
        }
        if !changed {
            break;
        }
    }
    blocks
}

/// Whether `blocks[index]` is a short foreign focus that its two neighbours should absorb.
fn is_swallowable(blocks: &[Block], index: usize) -> bool {
    let left = &blocks[index - 1];
    let short = &blocks[index];
    let right = &blocks[index + 1];

    short.kind == ActivityKind::Window
        && left.kind == ActivityKind::Window
        && right.kind == ActivityKind::Window
        && left.label == right.label
        && short.label != left.label
        && left.ended_at == short.started_at
        && short.ended_at == right.started_at
        && short.ended_at - short.started_at < SWALLOW_THRESHOLD_SECONDS
}

/// A block prints at most this many distinct titles as their own line; anything past the cap
/// rolls into one remainder. Measured against the live store rather than guessed: see the bead
/// this constant belongs to.
const TITLE_PART_CAP: usize = 5;

/// What a title part calls a segment whose title was never recorded.
///
/// The same string `format_segment` in `src/timeline.rs` already prints for that segment,
/// rather than a new one from the "unknown app" family: the raw report keeps rendering these
/// rows once the aggregated timeline exists beside it, and one fact under two names across two
/// views of the same day is the confusion both views exist to avoid.
const MISSING_TITLE: &str = "untitled";

/// One line under a block: a distinct title with its own duration, or the remainder standing in
/// for every title past `TITLE_PART_CAP`.
#[derive(Debug, Eq, PartialEq)]
pub enum TitlePart {
    Title {
        title: String,
        duration_seconds: i64,
    },
    Remainder {
        duration_seconds: i64,
        title_count: usize,
    },
}

impl TitlePart {
    pub fn duration_seconds(&self) -> i64 {
        match self {
            TitlePart::Title {
                duration_seconds, ..
            } => *duration_seconds,
            TitlePart::Remainder {
                duration_seconds, ..
            } => *duration_seconds,
        }
    }
}

/// Below this overlap with a block, in seconds, a media segment does not explain the block: a
/// track heard for four seconds while the block was about something else is noise, not context.
/// Measured against the media layer's own tests rather than observed overlap, since the store
/// this layer was designed against predates the media schema and holds no media rows at all.
const BACKGROUND_MEDIA_FLOOR_SECONDS: i64 = 60;

/// What a background fact calls a media segment whose player was never recorded, matching the
/// fallback `media_label` in `src/timeline.rs` already renders for the same case in the `Media
/// playing` section, so the two views of a day agree on the name.
const UNKNOWN_PLAYER: &str = "unknown player";

/// Attach background media to every block: the player whose total overlap with the block is
/// longest, provided it clears `BACKGROUND_MEDIA_FLOOR_SECONDS`, and how many other players also
/// cleared it. A block no player reaches the floor on keeps `background: None`.
///
/// Must run after grouping and swallowing have settled the block boundaries: overlap is measured
/// against each block's final `started_at`/`ended_at`, and swallowing changes both by merging
/// blocks together, so a background computed before it ran would be measured against a boundary
/// the block no longer has.
///
/// Media never contributes time: this only ever writes `block.background`. `started_at`,
/// `ended_at` and `segments` are read, never assigned, so no block's duration, no title part and
/// no per-application total sourced from either can change here, whatever the media contains.
pub fn attach_background_media(blocks: &mut [Block], media: &[MediaSegment]) {
    for block in blocks.iter_mut() {
        block.background = background_media_for(block, media);
    }
}

/// A media segment's overlap with a block never depends on which one started or ended first, or
/// on either extending past the other: it is the length of their shared instants alone, clamped
/// to zero when they do not share any, so a segment entirely outside the block cannot go negative
/// and read as a floor-clearing overlap by accident.
fn overlap_seconds(block: &Block, media: &MediaSegment) -> i64 {
    let start = block.started_at.max(media.started_at);
    let end = block.ended_at.min(media.ended_at);
    (end - start).max(0)
}

/// The one background fact a block carries, or `None` when no player's total overlap with the
/// block clears the floor.
///
/// A player's overlap is summed across every one of its media segments that overlaps the block,
/// not taken from its single longest segment: two consecutive tracks from the same player are
/// one continuous stretch of that player playing behind the block, the same reasoning `.3`
/// already applies to a title repeated across an interruption.
fn background_media_for(block: &Block, media: &[MediaSegment]) -> Option<BackgroundMedia> {
    let mut overlap_by_player: Vec<(String, i64)> = Vec::new();
    for segment in media {
        let overlap = overlap_seconds(block, segment);
        if overlap <= 0 {
            continue;
        }
        let player = segment.snapshot.player.as_deref().unwrap_or(UNKNOWN_PLAYER);
        match overlap_by_player
            .iter_mut()
            .find(|(existing, _)| existing == player)
        {
            Some((_, total)) => *total += overlap,
            None => overlap_by_player.push((player.to_string(), overlap)),
        }
    }

    overlap_by_player.retain(|(_, total)| *total >= BACKGROUND_MEDIA_FLOOR_SECONDS);
    if overlap_by_player.is_empty() {
        return None;
    }

    // Longest overlap first, tie broken by player name so the same day reads the same way
    // twice, the same rule `.3` already applies when two titles tie on duration.
    overlap_by_player.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let (player, _) = overlap_by_player.remove(0);
    Some(BackgroundMedia {
        player,
        other_player_count: overlap_by_player.len(),
    })
}

impl Block {
    /// This block's distinct titles, longest first, at most `TITLE_PART_CAP` of them, with a
    /// duration tie broken by title text so the same day reads the same way twice.
    ///
    /// A title repeated inside the block, even with a different title between its two
    /// occurrences, becomes one part whose duration is their sum: the pairing is by title text,
    /// not by position. Every segment in the block contributes to exactly one part, so the parts
    /// sum to the block's own duration exactly, remainder included. Nothing here can produce
    /// two parts covering the same segment, or a part covering none.
    pub fn title_parts(&self) -> Vec<TitlePart> {
        let mut durations: Vec<(String, i64)> = Vec::new();
        for segment in &self.segments {
            let title = segment.snapshot.title.as_deref().unwrap_or(MISSING_TITLE);
            let duration = segment.ended_at - segment.started_at;
            match durations.iter_mut().find(|(existing, _)| existing == title) {
                Some((_, total)) => *total += duration,
                None => durations.push((title.to_string(), duration)),
            }
        }

        durations.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        if durations.len() <= TITLE_PART_CAP {
            return durations
                .into_iter()
                .map(|(title, duration_seconds)| TitlePart::Title {
                    title,
                    duration_seconds,
                })
                .collect();
        }

        let remainder_titles = durations.split_off(TITLE_PART_CAP);
        let mut parts: Vec<TitlePart> = durations
            .into_iter()
            .map(|(title, duration_seconds)| TitlePart::Title {
                title,
                duration_seconds,
            })
            .collect();
        parts.push(TitlePart::Remainder {
            duration_seconds: remainder_titles.iter().map(|(_, duration)| duration).sum(),
            title_count: remainder_titles.len(),
        });
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{ActivitySnapshot, MediaSnapshot};

    fn window_segment(started_at: i64, ended_at: i64, app: &str, title: &str) -> TimelineSegment {
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

    fn idle_segment(started_at: i64, ended_at: i64) -> TimelineSegment {
        TimelineSegment {
            started_at,
            ended_at,
            snapshot: ActivitySnapshot::idle(),
        }
    }

    fn suspended_segment(started_at: i64, ended_at: i64) -> TimelineSegment {
        TimelineSegment {
            started_at,
            ended_at,
            snapshot: ActivitySnapshot::suspended(),
        }
    }

    #[test]
    fn eleven_segments_with_different_titles_become_one_block() {
        let segments: Vec<TimelineSegment> = (0..11)
            .map(|index| {
                window_segment(
                    index * 10,
                    index * 10 + 10,
                    "firefox",
                    &format!("title {index}"),
                )
            })
            .collect();

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 1);
        let block = &narrative.blocks[0];
        assert_eq!(block.label, "firefox");
        assert_eq!(block.started_at, 0);
        assert_eq!(block.ended_at, 110);
        assert_eq!(block.segments.len(), 11);
    }

    #[test]
    fn a_different_application_label_starts_a_new_block() {
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            window_segment(10, 20, "zed", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 2);
        assert_eq!(narrative.blocks[0].label, "firefox");
        assert_eq!(narrative.blocks[1].label, "zed");
        assert_eq!(narrative.blocks[0].ended_at, narrative.blocks[1].started_at);
    }

    #[test]
    fn an_idle_segment_between_two_firefox_segments_produces_three_blocks() {
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            idle_segment(10, 20),
            window_segment(20, 30, "firefox", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 3);
        assert_eq!(narrative.blocks[0].label, "firefox");
        assert_eq!(narrative.blocks[1].label, "AFK");
        assert_eq!(narrative.blocks[2].label, "firefox");
    }

    #[test]
    fn a_gap_between_two_segments_of_the_same_label_produces_two_blocks() {
        // The daemon was not running between 10 and 25: the same label on both sides of the
        // gap must not read as one uninterrupted stretch of it.
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            window_segment(25, 30, "firefox", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 2);
        assert_eq!(narrative.blocks[0].started_at, 0);
        assert_eq!(narrative.blocks[0].ended_at, 10);
        assert_eq!(narrative.blocks[1].started_at, 25);
        assert_eq!(narrative.blocks[1].ended_at, 30);
    }

    #[test]
    fn an_empty_input_produces_no_blocks() {
        let narrative = build_narrative(&[]);

        assert!(narrative.blocks.is_empty());
    }

    #[test]
    fn a_short_foreign_focus_between_matching_windows_is_swallowed() {
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            window_segment(10, 13, "zed", "quick check"),
            window_segment(13, 23, "firefox", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 1);
        let block = &narrative.blocks[0];
        assert_eq!(block.label, "firefox");
        assert_eq!(block.started_at, 0);
        assert_eq!(block.ended_at, 23);
        assert_eq!(block.segments.len(), 3);
    }

    #[test]
    fn a_segment_of_exactly_five_seconds_is_not_swallowed() {
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            window_segment(10, 15, "zed", "exactly five"),
            window_segment(15, 25, "firefox", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 3);
        assert_eq!(narrative.blocks[1].label, "zed");
        assert_eq!(
            narrative.blocks[1].ended_at - narrative.blocks[1].started_at,
            5
        );
    }

    #[test]
    fn a_short_focus_between_mismatched_neighbours_is_not_swallowed() {
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            window_segment(10, 13, "zed", "short"),
            window_segment(13, 23, "kitty", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 3);
        assert_eq!(narrative.blocks[0].label, "firefox");
        assert_eq!(narrative.blocks[1].label, "zed");
        assert_eq!(narrative.blocks[2].label, "kitty");
    }

    #[test]
    fn an_afk_segment_is_never_swallowed_regardless_of_duration() {
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            idle_segment(10, 11),
            window_segment(11, 20, "firefox", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 3);
        assert_eq!(narrative.blocks[1].label, "AFK");
        assert_eq!(narrative.blocks[1].started_at, 10);
        assert_eq!(narrative.blocks[1].ended_at, 11);
    }

    #[test]
    fn a_suspended_segment_is_never_swallowed_regardless_of_duration() {
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            suspended_segment(10, 11),
            window_segment(11, 20, "firefox", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 3);
        assert_eq!(narrative.blocks[1].label, "Suspended");
    }

    #[test]
    fn a_short_focus_is_not_swallowed_when_the_following_neighbour_is_afk() {
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            window_segment(10, 13, "zed", "short"),
            idle_segment(13, 20),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 3);
        assert_eq!(narrative.blocks[0].label, "firefox");
        assert_eq!(narrative.blocks[1].label, "zed");
        assert_eq!(narrative.blocks[2].label, "AFK");
    }

    #[test]
    fn a_short_focus_is_not_swallowed_when_the_preceding_neighbour_is_suspended() {
        let segments = vec![
            suspended_segment(0, 10),
            window_segment(10, 13, "zed", "short"),
            window_segment(13, 23, "firefox", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 3);
        assert_eq!(narrative.blocks[1].label, "zed");
    }

    #[test]
    fn a_window_named_afk_does_not_pass_the_afk_neighbour_check_by_label_text_alone() {
        // A window whose own app class happens to be spelled "AFK" must not be mistaken for the
        // idle block that precedes it: the check that keeps AFK out of a merge has to compare
        // what kind of segment this is, not merely the text its label renders as.
        let segments = vec![
            idle_segment(0, 10),
            window_segment(10, 13, "zed", "short"),
            window_segment(13, 23, "AFK", "a window named like the idle label"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 3);
        assert_eq!(narrative.blocks[1].label, "zed");
    }

    #[test]
    fn a_short_focus_separated_from_a_neighbour_by_a_gap_is_not_swallowed() {
        // The daemon was not running between the short segment and the block following it:
        // merging across that gap would credit seconds nothing covered.
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            window_segment(10, 13, "zed", "short"),
            window_segment(20, 30, "firefox", "b"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 3);
        assert_eq!(narrative.blocks[1].label, "zed");
    }

    #[test]
    fn an_alternating_run_of_short_foreign_focus_collapses_to_one_block() {
        let segments = vec![
            window_segment(0, 10, "firefox", "a"),
            window_segment(10, 13, "term", "short 1"),
            window_segment(13, 23, "firefox", "b"),
            window_segment(23, 25, "term", "short 2"),
            window_segment(25, 35, "firefox", "c"),
        ];

        let narrative = build_narrative(&segments);

        assert_eq!(narrative.blocks.len(), 1);
        let block = &narrative.blocks[0];
        assert_eq!(block.label, "firefox");
        assert_eq!(block.started_at, 0);
        assert_eq!(block.ended_at, 35);
        assert_eq!(block.segments.len(), 5);
    }

    fn window_segment_no_title(started_at: i64, ended_at: i64, app: &str) -> TimelineSegment {
        TimelineSegment {
            started_at,
            ended_at,
            snapshot: ActivitySnapshot::window(Some(app.to_string()), None, None, None),
        }
    }

    #[test]
    fn a_block_with_eleven_distinct_titles_truncates_to_five_plus_a_remainder() {
        let segments: Vec<TimelineSegment> = (0..11)
            .map(|index| {
                window_segment(
                    index * 10,
                    index * 10 + 10,
                    "firefox",
                    &format!("title {index}"),
                )
            })
            .collect();
        let block = &build_narrative(&segments).blocks[0];

        let parts = block.title_parts();

        assert_eq!(parts.len(), 6);
        let Some(TitlePart::Remainder {
            duration_seconds,
            title_count,
        }) = parts.last()
        else {
            panic!("expected the last part to be the remainder: {parts:?}");
        };
        assert_eq!(*title_count, 6);
        let block_seconds = block.ended_at - block.started_at;
        let parts_seconds: i64 = parts.iter().map(TitlePart::duration_seconds).sum();
        assert_eq!(parts_seconds, block_seconds);
        assert!(*duration_seconds > 0);
    }

    #[test]
    fn a_block_with_five_distinct_titles_carries_no_remainder() {
        let segments: Vec<TimelineSegment> = (0..5)
            .map(|index| {
                window_segment(
                    index * 10,
                    index * 10 + 10,
                    "firefox",
                    &format!("title {index}"),
                )
            })
            .collect();
        let block = &build_narrative(&segments).blocks[0];

        let parts = block.title_parts();

        assert_eq!(parts.len(), 5);
        assert!(
            !parts
                .iter()
                .any(|part| matches!(part, TitlePart::Remainder { .. })),
            "{parts:?}"
        );
    }

    #[test]
    fn a_repeated_title_sums_across_an_interruption_by_another_title() {
        let segments = vec![
            window_segment(0, 10, "firefox", "inbox"),
            window_segment(10, 25, "firefox", "docs"),
            window_segment(25, 40, "firefox", "inbox"),
        ];
        let block = &build_narrative(&segments).blocks[0];

        let parts = block.title_parts();

        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0],
            TitlePart::Title {
                title: "inbox".to_string(),
                duration_seconds: 25,
            }
        );
        assert_eq!(
            parts[1],
            TitlePart::Title {
                title: "docs".to_string(),
                duration_seconds: 15,
            }
        );
    }

    #[test]
    fn a_duration_tie_between_titles_breaks_by_title_text() {
        let segments = vec![
            window_segment(0, 10, "firefox", "zulu"),
            window_segment(10, 20, "firefox", "alpha"),
        ];
        let block = &build_narrative(&segments).blocks[0];

        let parts = block.title_parts();

        assert_eq!(
            parts,
            vec![
                TitlePart::Title {
                    title: "alpha".to_string(),
                    duration_seconds: 10,
                },
                TitlePart::Title {
                    title: "zulu".to_string(),
                    duration_seconds: 10,
                },
            ]
        );
    }

    #[test]
    fn a_block_with_one_distinct_title_carries_one_part() {
        let segments = vec![
            window_segment(0, 10, "firefox", "inbox"),
            window_segment(10, 20, "firefox", "inbox"),
        ];
        let block = &build_narrative(&segments).blocks[0];

        let parts = block.title_parts();

        assert_eq!(
            parts,
            vec![TitlePart::Title {
                title: "inbox".to_string(),
                duration_seconds: 20,
            }]
        );
    }

    #[test]
    fn a_segment_with_no_title_becomes_an_untitled_part_rather_than_being_dropped() {
        let segments = vec![window_segment_no_title(0, 10, "firefox")];
        let block = &build_narrative(&segments).blocks[0];

        let parts = block.title_parts();

        assert_eq!(
            parts,
            vec![TitlePart::Title {
                title: "untitled".to_string(),
                duration_seconds: 10,
            }]
        );
    }

    fn media_segment(started_at: i64, ended_at: i64, player: Option<&str>) -> MediaSegment {
        MediaSegment {
            started_at,
            ended_at,
            snapshot: MediaSnapshot {
                player: player.map(str::to_string),
                title: None,
                artist: None,
                album: None,
                item_url: None,
            },
        }
    }

    /// A single block spanning the whole day, so every test below only has to vary the media
    /// segment: overlap with a block that never itself changes is what each of these checks.
    fn one_day_block() -> Vec<TimelineSegment> {
        vec![window_segment(0, 3_600, "firefox", "a")]
    }

    #[test]
    fn an_overlap_of_fifty_nine_seconds_does_not_attach() {
        let mut narrative = build_narrative(&one_day_block());
        let media = vec![media_segment(0, 59, Some("spotify"))];

        attach_background_media(&mut narrative.blocks, &media);

        assert_eq!(narrative.blocks[0].background, None);
    }

    #[test]
    fn an_overlap_of_sixty_seconds_attaches() {
        let mut narrative = build_narrative(&one_day_block());
        let media = vec![media_segment(0, 60, Some("spotify"))];

        attach_background_media(&mut narrative.blocks, &media);

        assert_eq!(
            narrative.blocks[0].background,
            Some(BackgroundMedia {
                player: "spotify".to_string(),
                other_player_count: 0,
            })
        );
    }

    #[test]
    fn the_longest_overlap_among_three_qualifying_players_is_named() {
        let mut narrative = build_narrative(&one_day_block());
        let media = vec![
            media_segment(0, 60, Some("spotify")),
            media_segment(0, 90, Some("brave")),
            media_segment(0, 75, Some("mpv")),
        ];

        attach_background_media(&mut narrative.blocks, &media);

        assert_eq!(
            narrative.blocks[0].background,
            Some(BackgroundMedia {
                player: "brave".to_string(),
                other_player_count: 2,
            })
        );
    }

    #[test]
    fn a_media_segment_spanning_three_blocks_attaches_only_to_the_ones_it_clears_the_floor_on() {
        let segments = vec![
            window_segment(0, 100, "firefox", "a"),
            window_segment(100, 130, "zed", "b"),
            window_segment(130, 300, "kitty", "c"),
        ];
        let mut narrative = build_narrative(&segments);
        // 0-100 in the first block (100s, clears), 100-130 in the second (30s, does not), and
        // 130-190 in the third (60s, clears exactly).
        let media = vec![media_segment(0, 190, Some("spotify"))];

        attach_background_media(&mut narrative.blocks, &media);

        assert_eq!(
            narrative.blocks[0].background,
            Some(BackgroundMedia {
                player: "spotify".to_string(),
                other_player_count: 0,
            })
        );
        assert_eq!(narrative.blocks[1].background, None);
        assert_eq!(
            narrative.blocks[2].background,
            Some(BackgroundMedia {
                player: "spotify".to_string(),
                other_player_count: 0,
            })
        );
    }

    #[test]
    fn an_afk_block_takes_background_media_like_an_application_block() {
        let mut narrative = build_narrative(&[idle_segment(0, 3_600)]);
        let media = vec![media_segment(0, 60, Some("spotify"))];

        attach_background_media(&mut narrative.blocks, &media);

        assert_eq!(narrative.blocks[0].label, "AFK");
        assert_eq!(narrative.blocks[0].started_at, 0);
        assert_eq!(narrative.blocks[0].ended_at, 3_600);
        assert_eq!(
            narrative.blocks[0].background,
            Some(BackgroundMedia {
                player: "spotify".to_string(),
                other_player_count: 0,
            })
        );
    }

    #[test]
    fn a_suspended_block_takes_background_media_with_no_special_case() {
        let mut narrative = build_narrative(&[suspended_segment(0, 3_600)]);
        let media = vec![media_segment(0, 60, Some("spotify"))];

        attach_background_media(&mut narrative.blocks, &media);

        assert_eq!(narrative.blocks[0].label, "Suspended");
        assert_eq!(narrative.blocks[0].started_at, 0);
        assert_eq!(narrative.blocks[0].ended_at, 3_600);
        assert_eq!(
            narrative.blocks[0].background,
            Some(BackgroundMedia {
                player: "spotify".to_string(),
                other_player_count: 0,
            })
        );
    }

    #[test]
    fn a_media_segment_with_no_player_name_attaches_under_the_media_sections_fallback() {
        let mut narrative = build_narrative(&one_day_block());
        let media = vec![media_segment(0, 60, None)];

        attach_background_media(&mut narrative.blocks, &media);

        assert_eq!(
            narrative.blocks[0].background,
            Some(BackgroundMedia {
                player: "unknown player".to_string(),
                other_player_count: 0,
            })
        );
    }

    #[test]
    fn attaching_media_changes_no_blocks_duration_no_parts_duration_and_no_segments() {
        let segments = one_day_block();
        let mut narrative = build_narrative(&segments);
        let before_started_at = narrative.blocks[0].started_at;
        let before_ended_at = narrative.blocks[0].ended_at;
        let before_parts = narrative.blocks[0].title_parts();
        let before_segment_count = narrative.blocks[0].segments.len();
        // Deliberately outside the block on both sides, so a bug that let media widen a block
        // rather than merely describe it would move started_at or ended_at here.
        let media = vec![media_segment(-1_000, 10_000, Some("spotify"))];

        attach_background_media(&mut narrative.blocks, &media);

        assert_eq!(narrative.blocks[0].started_at, before_started_at);
        assert_eq!(narrative.blocks[0].ended_at, before_ended_at);
        assert_eq!(narrative.blocks[0].title_parts(), before_parts);
        assert_eq!(narrative.blocks[0].segments.len(), before_segment_count);
        assert!(narrative.blocks[0].background.is_some());
    }

    /// The invariant this whole layer rests on: whatever segments are grouped into, the result
    /// must be ordered and non-overlapping, must cover exactly the seconds the input covered,
    /// and must never reach outside the day it was built for. Each of the three has to be
    /// broken on purpose in the production code and seen to turn this test red before the
    /// bead is accepted; that check is not part of the automated suite, since a self-breaking
    /// guard is not something a passing run can demonstrate.
    #[test]
    fn blocks_are_ordered_non_overlapping_and_cover_exactly_the_day() {
        let day_start: i64 = 1_785_000_000;
        let day_end: i64 = day_start + 86_400;
        let mut state: u64 = 0x6e61_7272_6174_6976;

        for _ in 0..200 {
            let segments = generated_desktop_segments(&mut state, day_start, day_end);

            let narrative = build_narrative(&segments);

            for pair in narrative.blocks.windows(2) {
                assert!(
                    pair[0].ended_at <= pair[1].started_at,
                    "blocks out of order or overlapping: {pair:?}"
                );
            }

            let block_seconds: i64 = narrative
                .blocks
                .iter()
                .map(|block| block.ended_at - block.started_at)
                .sum();
            let segment_seconds: i64 = segments
                .iter()
                .map(|segment| segment.ended_at - segment.started_at)
                .sum();
            assert_eq!(
                block_seconds, segment_seconds,
                "blocks covered {block_seconds}s, input segments covered {segment_seconds}s"
            );

            for block in &narrative.blocks {
                assert!(
                    block.started_at >= day_start && block.ended_at <= day_end,
                    "block [{}, {}) reaches outside the day [{day_start}, {day_end})",
                    block.started_at,
                    block.ended_at
                );
            }
        }
    }

    /// The invariant `.3` adds on top of `.1`/`.2`: however a block's segments are split into
    /// title parts, the parts must sum to exactly the block's own duration. Breaking that has
    /// to turn this test red before the bead is accepted; dropping the remainder is the way the
    /// bead itself names, and this generator produces blocks past the five-title cap for a
    /// dropped remainder to actually be missed rather than merely unexercised.
    #[test]
    fn title_parts_sum_to_the_block_they_belong_to() {
        let day_start: i64 = 1_785_000_000;
        let day_end: i64 = day_start + 86_400;
        let mut state: u64 = 0x6e61_7272_6174_6976;
        let mut saw_a_remainder = false;

        for _ in 0..200 {
            let segments = generated_desktop_segments(&mut state, day_start, day_end);
            let narrative = build_narrative(&segments);

            for block in &narrative.blocks {
                let parts = block.title_parts();
                let parts_seconds: i64 = parts.iter().map(TitlePart::duration_seconds).sum();
                let block_seconds = block.ended_at - block.started_at;
                assert_eq!(
                    parts_seconds, block_seconds,
                    "block [{}, {}) parts covered {parts_seconds}s: {parts:?}",
                    block.started_at, block.ended_at
                );
                assert!(parts.len() <= TITLE_PART_CAP + 1, "{parts:?}");
                if parts
                    .iter()
                    .any(|part| matches!(part, TitlePart::Remainder { .. }))
                {
                    saw_a_remainder = true;
                }
            }
        }

        assert!(
            saw_a_remainder,
            "200 generated days never produced a block past the five-title cap; \
             the remainder branch went unexercised"
        );
    }

    /// The invariant `.4` adds on top of `.1`/`.2`/`.3`: attaching media moves no block's
    /// boundary or segments and changes no title part, whatever the media contains. The review
    /// of the parent bead found its own containment check close to vacuous, since grouping only
    /// ever copies timestamps from input segments; `attach_background_media` is the first place
    /// in this module that computes an instant rather than copying one, so this generator has to
    /// produce media that overlaps block boundaries, spans several blocks, and starts before and
    /// ends after the day for that computation to actually be exercised, not merely present.
    /// Break it on purpose by letting a block's `ended_at` widen to a media segment's own bound
    /// and see it turn red before accepting the bead.
    #[test]
    fn attaching_media_changes_no_block_and_no_title_part() {
        let day_start: i64 = 1_785_000_000;
        let day_end: i64 = day_start + 86_400;
        let mut desktop_state: u64 = 0x6e61_7272_6174_6976;
        let mut media_state: u64 = 0x6d65_6469_615f_7374;
        let mut saw_out_of_bounds_media = false;
        let mut saw_a_multi_block_attachment = false;
        let mut saw_an_attachment = false;

        for _ in 0..200 {
            let segments = generated_desktop_segments(&mut desktop_state, day_start, day_end);
            let media = generated_media_segments(&mut media_state, day_start, day_end);
            let mut narrative = build_narrative(&segments);

            let before: Vec<(i64, i64, usize)> = narrative
                .blocks
                .iter()
                .map(|block| (block.started_at, block.ended_at, block.segments.len()))
                .collect();
            let before_parts: Vec<Vec<TitlePart>> =
                narrative.blocks.iter().map(Block::title_parts).collect();

            attach_background_media(&mut narrative.blocks, &media);

            let after: Vec<(i64, i64, usize)> = narrative
                .blocks
                .iter()
                .map(|block| (block.started_at, block.ended_at, block.segments.len()))
                .collect();
            assert_eq!(
                before, after,
                "attaching media moved a block's boundary or its segments"
            );

            let after_parts: Vec<Vec<TitlePart>> =
                narrative.blocks.iter().map(Block::title_parts).collect();
            assert_eq!(
                before_parts, after_parts,
                "attaching media changed a block's title parts"
            );

            for pair in narrative.blocks.windows(2) {
                assert!(
                    pair[0].ended_at <= pair[1].started_at,
                    "blocks out of order or overlapping after attaching media: {pair:?}"
                );
            }
            for block in &narrative.blocks {
                assert!(
                    block.started_at >= day_start && block.ended_at <= day_end,
                    "block [{}, {}) reaches outside the day after attaching media",
                    block.started_at,
                    block.ended_at
                );
            }

            if media
                .iter()
                .any(|segment| segment.started_at < day_start || segment.ended_at > day_end)
            {
                saw_out_of_bounds_media = true;
            }
            for segment in &media {
                let clearing_blocks = narrative
                    .blocks
                    .iter()
                    .filter(|block| {
                        overlap_seconds(block, segment) >= BACKGROUND_MEDIA_FLOOR_SECONDS
                    })
                    .count();
                if clearing_blocks >= 2 {
                    saw_a_multi_block_attachment = true;
                }
            }
            if narrative
                .blocks
                .iter()
                .any(|block| block.background.is_some())
            {
                saw_an_attachment = true;
            }
        }

        assert!(
            saw_out_of_bounds_media,
            "200 generated days never produced media outside the day; the out-of-bounds path \
             went unexercised"
        );
        assert!(
            saw_a_multi_block_attachment,
            "200 generated days never produced one media segment clearing the floor on two \
             blocks at once; the multi-block path went unexercised"
        );
        assert!(
            saw_an_attachment,
            "200 generated days never produced a single background attachment; \
             attach_background_media went unexercised"
        );
    }

    /// A small, dependency-free linear congruential generator, the same one `src/timeline.rs`
    /// uses for its own property test: many varied inputs are needed, not a cryptographically
    /// sound source, and pulling in a fuzzing crate for one test would ask every future audit
    /// of this crate's dependencies to account for it.
    fn lcg_next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        *state
    }

    /// Non-overlapping desktop segments walking the whole day, the way capture actually
    /// produces them: one lane, one row open at a time, occasionally leaving a gap where the
    /// daemon was not running.
    fn generated_desktop_segments(
        state: &mut u64,
        day_start: i64,
        day_end: i64,
    ) -> Vec<TimelineSegment> {
        let labels = ["firefox", "zed", "AFK", "Suspended"];
        // A small pool rather than a title unique per segment: real focus revisits the same
        // handful of pages and buffers, and the title-parts property test needs blocks that
        // repeat a title (to exercise the summed-duration path) as well as blocks that exceed
        // the five-title cap (to exercise the remainder), neither of which a pool of one or a
        // pool of hundreds would produce with any regularity.
        let titles = [
            "title 0", "title 1", "title 2", "title 3", "title 4", "title 5", "title 6", "title 7",
        ];
        let mut segments = Vec::new();
        let mut cursor = day_start;
        let mut previous_label: Option<&str> = None;
        while cursor < day_end {
            // One run in six opens a gap instead of a segment, so the generator exercises the
            // boundary the "gap breaks a block" rule exists for, not only unbroken runs. A gap
            // also ends any run of the same label: the daemon being down is not a continuation
            // of what came before it.
            if lcg_next(state).is_multiple_of(6) {
                let gap = 1 + (lcg_next(state) % 120) as i64;
                cursor = (cursor + gap).min(day_end);
                previous_label = None;
                continue;
            }
            // Duration, label, stickiness and title all come from this one draw rather than
            // separate calls: an LCG's low bits cycle far faster than its high bits, so
            // `% labels.len()` alone (a power of two) walked the four labels in a near-fixed
            // rotation and the swallow branch below never once lined up with a matching pair of
            // neighbours. Every field below is read from its own bit range, all distinct and
            // clear of the lowest dozen bits `% 3_600` and `% 8` consume, so none of them can
            // fall into that same lock-step.
            let raw = lcg_next(state);
            // One run in eight lands near the swallow threshold instead of the full range, so
            // the generator exercises swallowing itself, not only the grouping it sits on top of.
            let duration = if (raw >> 58) & 0b111 == 0 {
                1 + (raw % 8) as i64
            } else {
                1 + (raw % 3_600) as i64
            };
            let end = (cursor + duration).min(day_end);
            // About half the time, focus stays on whatever it was on last instead of drawing
            // fresh: a label picked independently every segment averages a run of about one
            // segment, too short for a block to ever accumulate more than a handful of titles.
            let stays_on_previous_label = (raw >> 48) & 0b1 == 1;
            let label = match (stays_on_previous_label, previous_label) {
                (true, Some(label)) => label,
                _ => labels[(raw >> 62) as usize],
            };
            previous_label = Some(label);
            let title = titles[((raw >> 50) & 0b111) as usize];
            let segment = match label {
                "AFK" => idle_segment(cursor, end),
                // A suspended stretch is a first-class label the criteria name beside AFK, and it
                // is the one that produces the longest real blocks, so the generator has to emit it.
                "Suspended" => suspended_segment(cursor, end),
                _ => window_segment(cursor, end, label, title),
            };
            segments.push(segment);
            cursor = end;
        }
        segments
    }

    /// Media segments walking a window wider than the day on both sides, the way a stored media
    /// row actually can: its own timestamps are never clipped to the day a report asks for, so
    /// the first and last rows of a real day routinely start before it or end after it. Duration
    /// is drawn from a wide range so a segment often spans several blocks, and occasionally from
    /// a narrow one so it lands on either side of the sixty-second floor rather than always
    /// clearing it by a wide margin.
    fn generated_media_segments(
        state: &mut u64,
        day_start: i64,
        day_end: i64,
    ) -> Vec<MediaSegment> {
        let players = ["spotify", "brave", "mpv"];
        let mut segments = Vec::new();
        let mut cursor = day_start - 3_600;
        let stop_at = day_end + 3_600;
        while cursor < stop_at {
            let raw = lcg_next(state);
            // One run in eight lands near the sixty-second floor instead of the wide range, so
            // the generator exercises both sides of it rather than only clearing it by minutes.
            let duration = if (raw >> 58) & 0b111 == 0 {
                1 + (raw % 120) as i64
            } else {
                1 + (raw % 5_000) as i64
            };
            let end = cursor + duration;
            // Read from the top bits rather than `% players.len()`: an LCG's low bits cycle far
            // faster than its high bits under a power-of-two modulus, the defect that made the
            // desktop generator's own label draw walk a near-fixed rotation before it was moved
            // here too. One slot in four stands for a media row with no player name at all.
            let player = players.get((raw >> 62) as usize);
            segments.push(MediaSegment {
                started_at: cursor,
                ended_at: end,
                snapshot: MediaSnapshot {
                    player: player.map(|name| name.to_string()),
                    title: None,
                    artist: None,
                    album: None,
                    item_url: None,
                },
            });
            // A gap between tracks about a quarter of the time, so segments are not always
            // touching: a real player stops between tracks too.
            let gap = if (raw >> 45) & 0b11 == 0 {
                1 + ((raw >> 20) % 600) as i64
            } else {
                0
            };
            cursor = end + gap;
        }
        segments
    }
}
