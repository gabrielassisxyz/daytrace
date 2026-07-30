//! Recognizing a stretch the machine spent suspended.
//!
//! What this guarantees: no movement of the wall clock, of any size or direction, can produce a
//! segment. The duration of a stretch is the kernel's own count of suspended time and the wall
//! clock only says where on the timeline to put it. That matters because the wall clock jumps as
//! a matter of routine, since every boot starts it from a hardware clock that has drifted and the
//! correction arrives as a step. An earlier version of this compared wall time against elapsed
//! time, which turned each of those corrections into an absence nobody had. A fabricated
//! segment is worse than a missing one: it is stated as fact by the report and the export, and
//! nothing downstream can tell it from a real gap.
//!
//! What it cannot do. It cannot separate suspend from hibernate, since the kernel counts both
//! the same way, and both are honestly "the machine was not running". It cannot see a stretch the
//! daemon was not running for, whether the machine was off or the daemon merely stopped: the
//! clocks restart at boot and a fresh process has no earlier reading, so that stays an ordinary
//! hole in the day. It cannot say when within one poll interval the machine went down or came
//! back, so each endpoint carries up to a poll of error, and because the endpoints come from the
//! wall clock they also carry whatever error that clock holds at the moment of the resume: a
//! stretch of the right length can be placed at the wrong time, and on the wrong calendar day.
//!
//! One thing it does not fully guarantee, since the guarantee above is about the wall clock and
//! this is not. The two since-boot clocks are read one after the other, so a delay between those
//! two calls inflates the next poll's reading by the same amount, and a delay longer than
//! `MIN_POWERED_DOWN_SECONDS` becomes a stretch nobody spent. The floor is the bound, the window
//! is microseconds wide, and the ceiling is whatever the machine has really suspended since boot.
//! The reasoning is with that constant.
//!
//! What would make the choice wrong: a kernel whose boot-time clock did not count suspended
//! time. Then the two readings would agree and nothing would ever be recorded: a missing
//! segment, which is the failure this is willing to have.

use crate::timeline::unix_now;
use std::time::Duration;

/// One reading of the clocks a poll needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockReading {
    /// Seconds since the unix epoch.
    ///
    /// Used only to place a stretch on the timeline. It decides nothing, because it is the one
    /// clock on the machine that jumps: it is initialized from a hardware clock that has
    /// drifted, corrected in steps by whatever keeps it in sync, and settable by hand.
    pub wall: i64,
    /// Time since boot, not counting time the machine spent suspended.
    pub monotonic: Duration,
    /// Time since boot, counting time the machine spent suspended.
    pub boottime: Duration,
}

impl ClockReading {
    /// How long the machine has spent suspended since it booted.
    ///
    /// The two clocks are the same clock apart from this, so their difference is the whole of
    /// it, maintained by the kernel and unreachable from user space. Neither clock is affected
    /// by a correction to the wall clock, which is what makes this quantity, and not a wall
    /// clock jump, the thing a suspend can be recognized from.
    fn suspended_since_boot(&self) -> Duration {
        self.boottime.saturating_sub(self.monotonic)
    }
}

/// The clock boundary a powered-down stretch is recognized through.
///
/// A trait because a suspend cannot be staged inside a test, and a scripted set of readings is
/// the only way to run the code that interprets one.
pub trait SessionClock {
    /// Every clock a poll needs, read together, so the readings describe one instant.
    fn read(&self) -> ClockReading;
}

/// The clocks of the machine the daemon runs on.
#[derive(Debug)]
pub struct SystemSessionClock;

impl SessionClock for SystemSessionClock {
    fn read(&self) -> ClockReading {
        // Boot time first and monotonic second, deliberately, though not for the reason it looks
        // like. The two calls are not simultaneous, so whatever delay falls between them is
        // counted into the second clock alone and understates this reading's difference by that
        // much. That does not remove the error, it defers it: the next poll's delta is inflated
        // by however much the previous reading was understated, so either order can inflate a
        // delta by the same amount. What the order does buy is a ceiling. Reading boot time first
        // means the shortfall lands in the clock that is subtracted, so the difference saturates
        // at zero and the most a delay can ever hand the next poll is the suspend the machine has
        // actually accumulated since booting. Monotonic first has no such ceiling, and would
        // manufacture a stretch out of a machine that had never suspended at all.
        let boottime = since_boot(libc::CLOCK_BOOTTIME);
        let monotonic = since_boot(libc::CLOCK_MONOTONIC);

        ClockReading {
            wall: unix_now(),
            monotonic,
            boottime,
        }
    }
}

/// Read one of the kernel's since-boot clocks.
///
/// A failure is treated as unreachable rather than reported, which is what the standard
/// library's own monotonic clock does with the same call. Both clock ids are always present on
/// Linux, and the alternative matters: a read that silently returned zero would make the next
/// successful read look like every second of suspend since boot had just happened.
fn since_boot(clock_id: libc::clockid_t) -> Duration {
    let mut reading = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `reading` is a live, fully initialized `timespec`, and the clock ids passed here
    // are the two the kernel always provides.
    let outcome = unsafe { libc::clock_gettime(clock_id, &mut reading) };
    assert!(outcome == 0, "the kernel refused to read clock {clock_id}");

    Duration::new(
        reading.tv_sec.max(0) as u64,
        reading.tv_nsec.clamp(0, 999_999_999) as u32,
    )
}

/// A stretch during which the machine was not running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PoweredDownGap {
    pub started_at: i64,
    pub ended_at: i64,
}

/// What one poll of the clocks says: when it happened, and whether it is the first poll after
/// the machine came back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionObservation {
    pub observed_at: i64,
    pub powered_down_gap: Option<PoweredDownGap>,
}

/// The shortest suspend that earns a segment of its own.
///
/// Mostly a policy: a suspend shorter than this is a real one that is not worth breaking a day
/// into three rows for, and the seconds it discards stay with whatever segment was open, which is
/// where they already were.
///
/// It is also the only bound on the one way this can still be wrong. The two clock reads of a
/// single poll are not simultaneous, so a delay between them understates that reading and inflates
/// the next poll's delta by the same amount. A delay of at least this floor therefore shows up as
/// a stretch nobody spent. The window is roughly a microsecond wide out of a poll interval of
/// seconds, and the one realistic multi-second freeze that could land inside it is a real suspend,
/// which falls the safe way: boot time is read before it and elapsed time after, so the stretch is
/// counted correctly and only the thaw is over-credited. A process frozen by a signal or a cgroup
/// can land anywhere, capped by the suspend accumulated since boot. Bracketing the reads would
/// close it; the odds did not earn the machinery.
const MIN_POWERED_DOWN_SECONDS: u64 = 5;

/// Watches successive polls for time the machine spent suspended between them.
#[derive(Debug, Default)]
pub struct PowerGapWatch {
    previous: Option<ClockReading>,
}

impl PowerGapWatch {
    /// Read the clocks once and report a powered-down stretch when one has just ended.
    ///
    /// A suspend cannot be observed while it happens, because the process is frozen with the
    /// rest of the machine. It can only be recognized afterwards, which is why this reports a
    /// stretch that is already over rather than a state the machine is in.
    pub fn observe(&mut self, clock: &dyn SessionClock) -> SessionObservation {
        let reading = clock.read();
        let gap = self
            .previous
            .and_then(|previous| powered_down_gap(previous, reading));
        self.previous = Some(reading);

        SessionObservation {
            observed_at: reading.wall,
            powered_down_gap: gap,
        }
    }
}

/// The stretch the machine spent suspended between two readings, if it is long enough to record.
///
/// The duration comes from the kernel's own accounting and the wall clock only says where to put
/// it: the stretch ends at the poll that noticed it, which is within one poll of the resume, and
/// starts that many seconds earlier. Measuring the duration from the wall clock instead is what
/// made an ordinary clock correction indistinguishable from an absence.
fn powered_down_gap(previous: ClockReading, current: ClockReading) -> Option<PoweredDownGap> {
    let suspended_for = current
        .suspended_since_boot()
        .saturating_sub(previous.suspended_since_boot());
    if suspended_for < Duration::from_secs(MIN_POWERED_DOWN_SECONDS) {
        return None;
    }

    let suspended_seconds = suspended_for.as_secs() as i64;
    Some(PoweredDownGap {
        started_at: current.wall.saturating_sub(suspended_seconds),
        ended_at: current.wall,
    })
}

#[cfg(test)]
mod tests {
    use super::{ClockReading, PowerGapWatch, PoweredDownGap, SessionClock, SystemSessionClock};
    use std::cell::RefCell;
    use std::time::Duration;

    #[test]
    fn the_machine_reads_two_since_boot_clocks_and_not_the_wall_clock_twice() {
        // The scripted clock cannot reach the clock ids, and the three that matter are one
        // constant apart in the source. This covers the mistake that would fabricate data:
        // reading the wall clock as boot time puts an epoch's worth of seconds into the
        // difference and reports a suspend lasting decades.
        //
        // It does not cover reading the same clock twice, and cannot: the difference is then zero,
        // which is exactly what an honest reading on a machine that has not suspended looks like.
        // That mistake costs a segment rather than inventing one, and the assertion below catches
        // the swapped pair as soon as the machine has suspended at all.
        //
        // The bound is a decade rather than a century so that it stays below the unix epoch,
        // which is the number a misread would produce. A century would admit it.
        const A_DECADE: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);
        let reading = SystemSessionClock.read();

        assert!(
            reading.monotonic > Duration::ZERO && reading.boottime < A_DECADE,
            "both readings have to be an uptime rather than a date: {reading:?}"
        );
        // Not `boottime >= monotonic`, which is true of the clocks and false of two reads of
        // them: boot time is read first on purpose, so the microseconds between the two calls
        // land in the monotonic reading and leave it very slightly ahead on a machine that has
        // never suspended. That is the whole point of the ordering, and it is why the difference
        // is taken with a saturating subtraction. The slack below is generous enough for a
        // descheduled pair of reads and still far too small to hide a swapped pair on a machine
        // that has suspended at all.
        assert!(
            reading.monotonic <= reading.boottime + Duration::from_secs(1),
            "the monotonic clock cannot run ahead of boot time by more than the gap between the \
             two reads: {reading:?}"
        );
        assert!(
            reading.wall > 1_700_000_000,
            "the wall reading has to be a unix instant: {reading:?}"
        );
    }

    /// A clock boundary that replays a fixed script of readings.
    ///
    /// Readings are listed in the order they happen. None of the three clocks can be moved on the
    /// machine running the tests, since a suspend cannot be staged and stepping the wall clock
    /// needs the machine to itself, so a script is the only way to hand any of it to the code that
    /// reads it.
    struct ScriptedSessionClock {
        remaining: RefCell<Vec<ClockReading>>,
    }

    impl ScriptedSessionClock {
        fn new(readings: Vec<ClockReading>) -> Self {
            let mut remaining = readings;
            remaining.reverse();
            Self {
                remaining: RefCell::new(remaining),
            }
        }
    }

    impl SessionClock for ScriptedSessionClock {
        fn read(&self) -> ClockReading {
            self.remaining
                .borrow_mut()
                .pop()
                .expect("script ran out of clock readings")
        }
    }

    /// One reading, in the units a reader of the test thinks in.
    fn reading(wall: i64, monotonic_seconds: u64, boottime_seconds: u64) -> ClockReading {
        ClockReading {
            wall,
            monotonic: Duration::from_secs(monotonic_seconds),
            boottime: Duration::from_secs(boottime_seconds),
        }
    }

    fn gaps(readings: Vec<ClockReading>) -> Vec<Option<PoweredDownGap>> {
        let clock = ScriptedSessionClock::new(readings);
        let mut watch = PowerGapWatch::default();
        let mut observed = Vec::new();
        while !clock.remaining.borrow().is_empty() {
            observed.push(watch.observe(&clock).powered_down_gap);
        }
        observed
    }

    #[test]
    fn a_suspend_is_read_from_the_time_the_kernel_says_the_machine_was_off() {
        // Ten seconds of running either side of an hour of suspend: boot time counts the hour
        // and the monotonic clock does not.
        let observed = gaps(vec![reading(1_000, 500, 500), reading(4_610, 510, 4_110)]);

        assert_eq!(
            observed,
            vec![
                None,
                Some(PoweredDownGap {
                    started_at: 1_010,
                    ended_at: 4_610,
                })
            ],
            "the stretch lasts what the kernel counted and ends where the poll found it"
        );
    }

    #[test]
    fn a_wall_clock_corrected_forward_while_the_machine_ran_is_never_an_absence() {
        // Both since-boot clocks advance by the one second that actually elapsed, so whatever
        // the wall clock did is not evidence of anything. Each of these is a correction that
        // happens in ordinary use, and the largest is a hardware clock with a dead battery.
        for (name, wall_step) in [
            ("a hardware clock read at boot, drifted", 623),
            ("a hardware clock read in the wrong zone", 3 * 60 * 60),
            ("a clock set by hand", 24 * 60 * 60),
            (
                "a hardware clock with no battery left",
                16 * 365 * 24 * 60 * 60,
            ),
        ] {
            let observed = gaps(vec![
                reading(1_000, 500, 500),
                reading(1_000 + wall_step, 501, 501),
            ]);

            assert_eq!(
                observed,
                vec![None, None],
                "{name} moved the wall clock by {wall_step}s while the machine was running, and \
                 a segment invented for it would be indistinguishable from a real absence"
            );
        }
    }

    #[test]
    fn a_wall_clock_corrected_backward_is_not_an_absence_on_the_poll_after_it_either() {
        // The correction that matters is the second one. A clock put back an hour and then
        // moved forward again ends up where it started, and a detector watching the wall clock
        // sees the return trip as an hour nobody was there for.
        let observed = gaps(vec![
            reading(5_000, 500, 500),
            reading(1_400, 501, 501),
            reading(5_002, 502, 502),
        ]);

        assert_eq!(
            observed,
            vec![None, None, None],
            "neither half of a correction and its undoing is an absence: {observed:?}"
        );
    }

    #[test]
    fn a_daemon_that_was_merely_stalled_reports_no_absence() {
        // Both since-boot clocks moved by an hour, so the machine was up the whole time and
        // something else kept this process off the CPU: a suspended machine advances boot time
        // alone. A day may not gain an absence because the daemon was starved.
        let observed = gaps(vec![reading(1_000, 500, 500), reading(4_600, 4_100, 4_100)]);

        assert_eq!(observed, vec![None, None]);
    }

    #[test]
    fn a_suspend_lasts_what_the_kernel_counted_and_not_what_the_wall_clock_did() {
        // A resume re-reads the hardware clock, so the wall clock usually moves by more than the
        // suspend: here it gains the hour of suspend plus an hour of correction.
        let observed = gaps(vec![reading(1_000, 500, 500), reading(8_200, 501, 4_101)]);

        assert_eq!(
            observed,
            vec![
                None,
                Some(PoweredDownGap {
                    started_at: 4_600,
                    ended_at: 8_200,
                })
            ],
            "the stretch must be the 3600s the kernel counted, not the 7200s the wall clock moved"
        );
    }

    #[test]
    fn the_first_poll_has_nothing_to_compare_against() {
        assert_eq!(
            gaps(vec![reading(5_000, 0, 900)]),
            vec![None],
            "a daemon that has just started holds no evidence about what the machine did before"
        );
    }

    #[test]
    fn an_ordinary_poll_reports_no_gap() {
        assert_eq!(
            gaps(vec![
                reading(1_000, 0, 0),
                reading(1_001, 1, 1),
                reading(1_002, 2, 2)
            ]),
            vec![None; 3]
        );
    }

    #[test]
    fn a_suspend_too_short_to_be_worth_a_row_is_left_with_the_open_segment() {
        assert_eq!(
            gaps(vec![reading(1_000, 500, 500), reading(1_004, 501, 504)]),
            vec![None, None]
        );
    }

    #[test]
    fn a_second_suspend_is_measured_from_the_poll_that_saw_the_first_one_end() {
        let observed = gaps(vec![
            reading(1_000, 500, 500),
            reading(4_610, 510, 4_110),
            reading(8_220, 520, 7_720),
        ]);

        assert_eq!(
            observed[2],
            Some(PoweredDownGap {
                started_at: 4_620,
                ended_at: 8_220,
            }),
            "each stretch is the suspend since the previous poll, not the total since boot"
        );
    }
}
