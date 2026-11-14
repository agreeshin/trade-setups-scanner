//! Constant-space cursors survive turns; latency slices admit work, never abort IO.
use super::*;

#[derive(Default)]
pub(super) struct ColdSchedule {
    ready: usize,
    waiting: usize,
    turn: u8,
}
impl ColdSchedule {
    pub fn select(&mut self, plans: &[SymbolPlan], count: usize) -> Vec<usize> {
        let (ready, waiting): (Vec<_>, Vec<_>) = (0..plans.len()).partition(|&index| {
            plans[index].specs.iter().any(|spec| !spec.pending && !spec.waiting)
        });
        let mut selected = Vec::with_capacity(count.min(plans.len()));
        for _ in 0..count.min(plans.len()) {
            // Three priority pages, then a reserved waiting page, even when a
            // turn admits only one page. Neither cursor resets on a retry.
            let priority = self.turn != 3;
            self.turn = (self.turn + 1) % 4;
            let (first, second, first_cursor, second_cursor) = if priority {
                (&ready, &waiting, &mut self.ready, &mut self.waiting)
            } else {
                (&waiting, &ready, &mut self.waiting, &mut self.ready)
            };
            let next = take_next(first, first_cursor, &selected)
                .or_else(|| take_next(second, second_cursor, &selected));
            if let Some(index) = next { selected.push(index); }
        }
        selected
    }
}
fn take_next(indices: &[usize], cursor: &mut usize, selected: &[usize]) -> Option<usize> {
    for _ in 0..indices.len() {
        let index = indices[*cursor % indices.len()];
        *cursor = (*cursor + 1) % indices.len();
        if !selected.contains(&index) { return Some(index); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive::state::tests::fixture;
    fn plans() -> Vec<SymbolPlan> {
        let f = fixture();
        (0..822).map(|index| {
            let mut spec = f.spec.clone();
            spec.waiting = index < 810;
            SymbolPlan { epoch: index.to_string(), instrument: f.instrument.clone(),
                catalog_id: index, specs: vec![spec], slots: BTreeMap::new() }
        }).collect()
    }
    #[test]
    fn restart_prioritizes_durable_ready_and_reserves_waiting_progress_across_single_page_turns() {
        let plans = plans();
        let mut schedule = ColdSchedule::default();
        let mut visited = BTreeSet::new();
        for turn in 0..3240 {
            let selected = schedule.select(&plans, 1);
            let index = selected[0];
            assert_eq!(index >= 810, turn % 4 != 3);
            visited.insert(index);
        }
        assert_eq!(visited.len(), 822, "the last waiting instrument must get a turn");
    }
    #[test]
    fn bounded_batches_are_distinct_and_survive_empty_or_shrinking_cohorts() {
        let mut schedule = ColdSchedule::default();
        let mut plans = plans();
        for _ in 0..20 {
            let selected = schedule.select(&plans, 4);
            assert_eq!(selected.len(), 4);
            assert_eq!(selected.iter().collect::<BTreeSet<_>>().len(), 4);
        }
        plans.truncate(2);
        assert_eq!(schedule.select(&plans, 4).len(), 2);
        plans.clear();
        assert!(schedule.select(&plans, 4).is_empty());
    }
}
