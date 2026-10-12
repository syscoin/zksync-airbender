use std::ops::Range;

/// Release only the leading unified circuits that cannot contain I&T records.
///
/// At this revision collection emits at most one record per RAM word and packs
/// `cycles_per_circuit` records per unified circuit. Thus at most
/// `ceil(ram_words / cycles_per_circuit)` trailing circuits can contain I&T,
/// regardless of delegation accesses or their distribution among snapshots.
/// This is the old per-word geometry, not the newer per-window geometry.
pub(crate) struct EmptyInitsAndTeardownsStreamer {
    pub next_sequence_id: usize,
    cycles_per_circuit: usize,
    max_it_instances: usize,
}

impl EmptyInitsAndTeardownsStreamer {
    pub fn new(ram_words: usize, cycles_per_circuit: usize) -> Self {
        assert_ne!(cycles_per_circuit, 0);
        Self {
            next_sequence_id: 0,
            cycles_per_circuit,
            max_it_instances: ram_words.div_ceil(cycles_per_circuit),
        }
    }

    pub fn release(&mut self, cycles_so_far: usize) -> Range<usize> {
        let completed_circuits = cycles_so_far / self.cycles_per_circuit;
        let frontier = completed_circuits.saturating_sub(self.max_it_instances);
        let start = self.next_sequence_id;
        self.next_sequence_id = frontier.max(start);
        start..self.next_sequence_id
    }

    pub fn finish(self, empty_circuits: usize) -> Range<usize> {
        // The run-time frontier is an architectural bound, not a prediction of
        // final occupancy. Fail closed if future layout changes violate it.
        assert!(
            self.next_sequence_id <= empty_circuits,
            "streamed unified init/teardown prefix exceeds the final empty prefix"
        );
        self.next_sequence_id..empty_circuits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM_WORDS: usize = (1 << 30) / size_of::<u32>();
    const CYCLES_PER_CIRCUIT: usize = (1 << 23) - 1;

    #[test]
    fn pinned_per_word_geometry_has_thirty_three_trailing_instances() {
        let streamer = EmptyInitsAndTeardownsStreamer::new(RAM_WORDS, CYCLES_PER_CIRCUIT);
        assert_eq!(streamer.max_it_instances, 33);
        assert_eq!(RAM_WORDS.div_ceil(CYCLES_PER_CIRCUIT), 33);
        assert!(32 * CYCLES_PER_CIRCUIT < RAM_WORDS);
        assert!(RAM_WORDS <= 33 * CYCLES_PER_CIRCUIT);
    }

    #[test]
    fn partial_circuits_and_repeated_snapshots_do_not_release_early_or_duplicate() {
        let mut streamer = EmptyInitsAndTeardownsStreamer::new(25, 10);
        assert!(streamer.release(29).is_empty());
        assert!(streamer.release(39).is_empty());
        assert_eq!(streamer.release(40), 0..1);
        assert!(streamer.release(40).is_empty());
        assert_eq!(streamer.release(105), 1..7);
        assert!(streamer.release(104).is_empty());
        assert_eq!(streamer.finish((105 - 25) / 10), 7..8);
    }

    #[test]
    fn all_final_occupancies_preserve_the_original_exact_prefix() {
        // Exhaustive reduced geometry includes partial final circuits and RAM
        // sizes not divisible by the per-circuit capacity.
        for ram_words in 0..=50 {
            for per_circuit in 1..=12 {
                for total_cycles in 0..=150 {
                    for records in 0..=ram_words.min(total_cycles) {
                        let mut streamer =
                            EmptyInitsAndTeardownsStreamer::new(ram_words, per_circuit);
                        let mut ids = Vec::new();
                        for cycles in (0..total_cycles).step_by(7) {
                            ids.extend(streamer.release(cycles));
                        }
                        ids.extend(streamer.release(total_cycles));
                        let final_empty = (total_cycles - records) / per_circuit;
                        ids.extend(streamer.finish(final_empty));
                        assert_eq!(ids, (0..final_empty).collect::<Vec<_>>());
                    }
                }
            }
        }
    }

    #[test]
    fn delegation_bursts_cannot_exceed_the_ram_word_bound() {
        let total_cycles = 101 * CYCLES_PER_CIRCUIT + 57;
        let mut streamer = EmptyInitsAndTeardownsStreamer::new(RAM_WORDS, CYCLES_PER_CIRCUIT);
        let mut ids = Vec::new();
        // Large, irregular cycle advances model delegation-heavy snapshots;
        // occupancy is the architectural worst case: every RAM word touched.
        for circuits in [0, 1, 32, 33, 34, 47, 65, 100] {
            let cycles = circuits * CYCLES_PER_CIRCUIT;
            let released = streamer.release(cycles);
            let guaranteed_empty = cycles.saturating_sub(RAM_WORDS) / CYCLES_PER_CIRCUIT;
            assert!(released.end <= guaranteed_empty);
            ids.extend(released);
        }
        ids.extend(streamer.release(total_cycles));
        let final_empty = (total_cycles - RAM_WORDS) / CYCLES_PER_CIRCUIT;
        ids.extend(streamer.finish(final_empty));
        assert_eq!(ids, (0..final_empty).collect::<Vec<_>>());
    }

    #[test]
    fn finite_pool_model_progresses_beyond_forty_six_circuits() {
        const POOL: usize = 384;
        const PER_CIRCUIT: usize = 8;
        const OTHER_LIVE: usize = 16;
        let mut streamer = EmptyInitsAndTeardownsStreamer::new(RAM_WORDS, CYCLES_PER_CIRCUIT);
        let mut retained = 0;
        for completed in 1..=160 {
            // Same ordering as process_trace: release before allocating this
            // snapshot's tracing chunks. This model assumes earlier replay/GPU
            // work has completed; native queue/snapshot lag is not modeled.
            let released = streamer.release(completed * CYCLES_PER_CIRCUIT);
            retained -= released.len() * PER_CIRCUIT;
            assert!(retained + PER_CIRCUIT + OTHER_LIVE <= POOL);
            retained += PER_CIRCUIT;
        }
        assert_eq!(retained, 33 * PER_CIRCUIT);
        assert!(47 * PER_CIRCUIT + OTHER_LIVE > POOL);
    }

    #[test]
    fn split_mode_none_keeps_all_markers_for_finalization() {
        let mut streamer: Option<EmptyInitsAndTeardownsStreamer> = None;
        let released = streamer.as_mut().map(|s| s.release(1000));
        assert!(released.is_none());
        let remaining = streamer.map_or(0..17, |s| s.finish(17));
        assert_eq!(remaining, 0..17);
    }

    #[test]
    #[should_panic(
        expected = "streamed unified init/teardown prefix exceeds the final empty prefix"
    )]
    fn finalization_fails_closed_if_a_future_layout_violates_the_bound() {
        let mut streamer = EmptyInitsAndTeardownsStreamer::new(25, 10);
        assert_eq!(streamer.release(100), 0..7);
        streamer.finish(6);
    }
}
