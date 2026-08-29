//! Groups a day's desktop segments into narrative blocks: consecutive segments that share one
//! application label, then absorbs a short foreign focus back into the block around it. Read-time
//! and stateless, it consumes the desktop slice `Store::day_activity` already returns and
//! produces an owned value. Nothing calls this module yet, so deleting it returns the tool to
//! its current behaviour.

use crate::activity::{ActivityKind, TimelineSegment};

/// A run of consecutive desktop segments that share one application label.
#[derive(Debug, Eq, PartialEq)]
pub struct Block {
    pub label: String,
    pub kind: ActivityKind,
    pub started_at: i64,
    pub ended_at: i64,
    pub segments: Vec<TimelineSegment>,
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
/// No renderer calls this yet (that lands in a later bead), hence the `#[allow(dead_code)]`.
#[allow(dead_code)]
pub fn build_narrative(segments: &[TimelineSegment]) -> Narrative {
    let blocks = group_into_blocks(segments);
    let blocks = swallow_short_foreign_blocks(blocks);
    Narrative { blocks }
}

/// Group consecutive desktop segments sharing one application label into blocks, with a gap
/// between two segments breaking a block regardless of label.
fn group_into_blocks(segments: &[TimelineSegment]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();

    for segment in segments {
        let label = block_label(segment);
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

/// The name a block is keyed by.
///
/// Mirrors `application_label` in `src/timeline.rs`, kept as its own copy rather than an
/// import: that function is private to its module, and widening `timeline.rs`'s surface to
/// reach it falls outside this bead's blast radius.
fn block_label(segment: &TimelineSegment) -> &str {
    match segment.snapshot.kind {
        ActivityKind::Idle => "AFK",
        ActivityKind::Suspended => "Suspended",
        ActivityKind::Unknown => "Unknown",
        ActivityKind::Window => segment
            .snapshot
            .app_class
            .as_deref()
            .unwrap_or("unknown app"),
    }
}

/// A block prints at most this many distinct titles as their own line; anything past the cap
/// rolls into one remainder. Measured against the live store rather than guessed: see the bead
/// this constant belongs to.
const TITLE_PART_CAP: usize = 5;

/// What a title part calls a segment whose title was never recorded.
///
/// The same string `src/timeline.rs:280` already prints for that segment, rather than a new one
/// from the "unknown app" family: the raw report keeps rendering these rows once the aggregated
/// timeline exists beside it, and one fact under two names across two views of the same day is
/// the confusion both views exist to avoid.
const MISSING_TITLE: &str = "untitled";

/// One line under a block: a distinct title with its own duration, or the remainder standing in
/// for every title past `TITLE_PART_CAP`.
///
/// No renderer calls this yet (that lands in a later bead), hence the `#[allow(dead_code)]`
/// below on every item that only a renderer would reach.
#[derive(Debug, Eq, PartialEq)]
#[allow(dead_code)]
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
    #[allow(dead_code)]
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

impl Block {
    /// This block's distinct titles, longest first, at most `TITLE_PART_CAP` of them, with a
    /// duration tie broken by title text so the same day reads the same way twice.
    ///
    /// A title repeated inside the block, even with a different title between its two
    /// occurrences, becomes one part whose duration is their sum: the pairing is by title text,
    /// not by position. Every segment in the block contributes to exactly one part, so the parts
    /// sum to the block's own duration exactly, remainder included. Nothing here can produce
    /// two parts covering the same segment, or a part covering none.
    #[allow(dead_code)]
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
    use crate::activity::ActivitySnapshot;

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
}
