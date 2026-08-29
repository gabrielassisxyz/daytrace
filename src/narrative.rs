//! Groups a day's desktop segments into narrative blocks: consecutive segments that share one
//! application label. Read-time and stateless, it consumes the desktop slice
//! `Store::day_activity` already returns and produces an owned value. Nothing calls this module
//! yet, so deleting it returns the tool to its current behaviour.

use crate::activity::{ActivityKind, TimelineSegment};

/// A run of consecutive desktop segments that share one application label.
#[derive(Debug, Eq, PartialEq)]
pub struct Block {
    pub label: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub segments: Vec<TimelineSegment>,
}

/// One day's desktop segments, grouped into blocks.
#[derive(Debug, Eq, PartialEq)]
pub struct Narrative {
    pub blocks: Vec<Block>,
}

/// Group the desktop slice of a stored day into blocks.
///
/// A segment extends the last block when it shares its label and picks up exactly where the
/// last one left off. A gap between two segments of the same label, meaning the daemon was not
/// running, starts a new block instead of extending the last one: a gap is an absence of
/// evidence rather than a continuation of what came before it, so the block boundaries sit at
/// the stored instants rather than spanning it.
///
/// The caller is expected to pass what the store returns: segments ordered by start and never
/// overlapping, which the desktop lane guarantees through its one-open-segment-per-lane index.
/// An unordered or overlapping vector is not rejected, it is grouped as given, so a caller that
/// merges or filters before calling has to preserve both properties itself.
///
/// No renderer calls this yet (that lands in a later bead), hence the `#[allow(dead_code)]`.
#[allow(dead_code)]
pub fn build_narrative(segments: &[TimelineSegment]) -> Narrative {
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
                started_at: segment.started_at,
                ended_at: segment.ended_at,
                segments: vec![segment.clone()],
            });
        }
    }

    Narrative { blocks }
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
        let mut segments = Vec::new();
        let mut cursor = day_start;
        while cursor < day_end {
            // One run in six opens a gap instead of a segment, so the generator exercises the
            // boundary the "gap breaks a block" rule exists for, not only unbroken runs.
            if lcg_next(state).is_multiple_of(6) {
                let gap = 1 + (lcg_next(state) % 120) as i64;
                cursor = (cursor + gap).min(day_end);
                continue;
            }
            let duration = 1 + (lcg_next(state) % 3_600) as i64;
            let end = (cursor + duration).min(day_end);
            let label = labels[(lcg_next(state) % labels.len() as u64) as usize];
            let segment = match label {
                "AFK" => idle_segment(cursor, end),
                // A suspended stretch is a first-class label the criteria name beside AFK, and it
                // is the one that produces the longest real blocks, so the generator has to emit it.
                "Suspended" => suspended_segment(cursor, end),
                _ => window_segment(cursor, end, label, "a title"),
            };
            segments.push(segment);
            cursor = end;
        }
        segments
    }
}
