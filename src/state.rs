// sway-tab — Pure state logic, no async, no IPC.

use crate::trace;

/// Shared state between the focus-event loop and the signal handlers.
///
/// `history` is ordered from most-recent (index 0) to least-recent. Index
/// 0 is the current window, index 1 is the previously focused window,
/// etc. The state is "cycling" (see [`State::cycling`]) while the user is
/// actively cycling — `cycle_pos` is `Some(_)`. While cycling we ignore our
/// own preview focus events so they don't pollute the history.
#[derive(Default)]
pub struct State {
    pub history: Vec<i64>,
    /// Some(_) when the user is actively cycling.
    pub cycle_pos: Option<usize>,
    /// The con_id we most recently told sway to focus (our own preview).
    /// Used to distinguish our preview focus events from real user switches.
    pub last_preview: Option<i64>,
}

impl State {
    /// True while the user is actively cycling.
    fn cycling(&self) -> bool {
        self.cycle_pos.is_some()
    }

    /// Add con_id to front of history. If already present, move it.
    fn add(&mut self, con_id: i64) {
        trace!("history.add: con_id={con_id}");
        self.history.retain(|&id| id != con_id);
        self.history.insert(0, con_id);
        trace!("history.add: new len={}", self.history.len());
    }

    /// Remove a window from history (e.g. when it closes).
    fn remove_id(&mut self, con_id: i64) {
        trace!("history.remove: con_id={con_id}");
        self.history.retain(|&id| id != con_id);
        trace!("history.remove: new len={}", self.history.len());
    }

    /// Move the item at `pos` to the front (used on commit).
    fn promote(&mut self, pos: usize) {
        trace!("history.promote: pos={pos}");
        if pos < self.history.len() {
            let con_id = self.history.remove(pos);
            self.history.insert(0, con_id);
            trace!("history.promote: con_id={con_id} promoted to front");
        }
    }

    /// Seed the history with existing window con_ids (e.g. from sway get_tree).
    /// Added in order — the first id in the slice will be at the back (oldest),
    /// the last will be at the front (most recent). Typically called once at
    /// startup.
    pub fn seed(&mut self, con_ids: &[i64]) {
        trace!("state.seed: {} windows", con_ids.len());
        for &id in con_ids {
            self.add(id);
        }
    }

    /// Handle a window close event. Removes the window from history.
    /// If the closed window was the current cycle target, cancels the cycle.
    pub fn handle_close_event(&mut self, con_id: i64) {
        trace!("handle_close_event: con_id={con_id}");
        self.remove_id(con_id);

        // If we were cycling and the closed window was our preview target,
        // or the history is now too short, cancel the cycle.
        if self.cycling() {
            if self.last_preview == Some(con_id) || self.history.len() < 2 {
                trace!("handle_close_event: cancelling active cycle");
                self.cycle_pos = None;
                self.last_preview = None;
            } else if let Some(pos) = self.cycle_pos {
                // The cycle_pos may now be out of bounds — clamp it.
                if pos >= self.history.len() {
                    self.cycle_pos = Some(self.history.len() - 1);
                }
            }
        }
    }

    /// Advance cycle by one. Returns Some(target_con_id) to focus, or None if history too short.
    /// On first call: freezes history, sets cycle_pos=1.
    /// On subsequent: advances cycle_pos circularly.
    pub fn advance_cycle(&mut self) -> Option<i64> {
        trace!("advance_cycle: called");

        if self.history.len() < 2 {
            trace!("advance_cycle: history too short, skipping");
            return None;
        }

        let pos = match self.cycle_pos {
            None => {
                // First press: freeze history so preview focuses don't pollute it.
                trace!("advance_cycle: first press, freezing history, cycle_pos=1");
                1
            }
            Some(pos) => {
                let next_pos = (pos + 1) % self.history.len();
                trace!("advance_cycle: advancing cycle_pos {pos} -> {next_pos}");
                next_pos
            }
        };
        self.cycle_pos = Some(pos);

        let target = self.history[pos];
        trace!("advance_cycle: target con_id={target} for preview");
        self.last_preview = Some(target);

        Some(target)
    }

    /// Commit current cycle position — promote to front, unfreeze, clear last_preview.
    /// No-op if no active cycle.
    pub fn commit_cycle(&mut self) {
        trace!("commit_cycle: called");
        if let Some(pos) = self.cycle_pos.take() {
            trace!("commit_cycle: committing pos={pos}, unfreezing history");
            self.promote(pos);
            self.last_preview = None;
        } else {
            trace!("commit_cycle: no active cycle, nothing to commit");
        }
    }

    /// Handle an incoming focus event.
    pub fn handle_focus_event(&mut self, con_id: i64) {
        trace!("event: focus change on con_id={con_id}");
        if !self.cycling() {
            self.add(con_id);
            trace!("event: con_id={con_id} tracked");
        } else if self.last_preview == Some(con_id) {
            trace!("event: focus ignored (our own preview)");
        } else {
            // User moved focus to something we didn't select — lazy commit
            // the in-progress cycle, then track the new focus.
            trace!("event: external focus change to con_id={con_id}, auto-committing");
            self.commit_cycle();
            self.add(con_id);
            trace!("event: con_id={con_id} auto-committed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: creates a State and adds the given con_ids in order.
    /// The *first* element in `ids` ends up at the front (most recent).
    /// e.g. make_state(&[10, 20, 30]) → history = [10, 20, 30]
    fn make_state(ids: &[i64]) -> State {
        let mut state = State::default();
        // Add in reverse so that the first element of `ids` ends up at front.
        for &id in ids.iter().rev() {
            state.add(id);
        }
        state
    }

    // ── history primitive tests ──────────────────────────────────────

    #[test]
    fn history_add_front_and_dedup() {
        let mut s = State::default();
        s.add(1);
        s.add(2);
        s.add(3);
        // Order should be [3, 2, 1]
        assert_eq!(s.history, vec![3, 2, 1]);

        // Re-adding 1 should move it to front: [1, 3, 2]
        s.add(1);
        assert_eq!(s.history, vec![1, 3, 2]);
    }

    #[test]
    fn history_remove_existing() {
        let mut s = State::default();
        s.add(1);
        s.add(2);
        s.add(3);
        // [3, 2, 1]
        s.remove_id(2);
        assert_eq!(s.history, vec![3, 1]);
    }

    #[test]
    fn history_remove_nonexistent_is_noop() {
        let mut s = State::default();
        s.add(1);
        s.add(2);
        s.remove_id(99);
        assert_eq!(s.history, vec![2, 1]);
    }

    #[test]
    fn history_get_returns_correct_items() {
        let mut s = State::default();
        s.add(100);
        s.add(200);
        s.add(300);
        // [300, 200, 100]
        assert_eq!(s.history.get(0).copied(), Some(300));
        assert_eq!(s.history.get(1).copied(), Some(200));
        assert_eq!(s.history.get(2).copied(), Some(100));
        assert_eq!(s.history.get(3).copied(), None);
    }

    #[test]
    fn history_promote_moves_to_front() {
        let mut s = State::default();
        s.add(1);
        s.add(2);
        s.add(3);
        // [3, 2, 1]
        s.promote(2); // promote item at index 2 (which is 1)
        // Now: [1, 3, 2]
        assert_eq!(s.history, vec![1, 3, 2]);
    }

    #[test]
    fn history_promote_out_of_bounds_is_noop() {
        let mut s = State::default();
        s.add(1);
        s.add(2);
        // [2, 1]
        s.promote(5); // out of bounds — should do nothing
        assert_eq!(s.history, vec![2, 1]);
    }

    // ── State::advance_cycle tests ───────────────────────────────────

    #[test]
    fn advance_cycle_empty_history_returns_none() {
        let mut state = State::default();
        assert_eq!(state.advance_cycle(), None);
    }

    #[test]
    fn advance_cycle_single_item_returns_none() {
        let mut state = make_state(&[42]);
        assert_eq!(state.advance_cycle(), None);
    }

    #[test]
    fn advance_cycle_two_items_first_press() {
        let mut state = make_state(&[10, 20]);
        // history = [10, 20]
        let result = state.advance_cycle();
        assert_eq!(result, Some(20)); // pos 1 = second item
        assert_eq!(state.cycle_pos, Some(1));
        assert_eq!(state.last_preview, Some(20));
    }

    #[test]
    fn advance_cycle_three_items_circular() {
        let mut state = make_state(&[10, 20, 30]);
        // history = [10, 20, 30]

        // First press → pos 1 → 20
        assert_eq!(state.advance_cycle(), Some(20));
        assert_eq!(state.cycle_pos, Some(1));

        // Second press → pos 2 → 30
        assert_eq!(state.advance_cycle(), Some(30));
        assert_eq!(state.cycle_pos, Some(2));

        // Third press → pos 0 (wraps) → 10
        assert_eq!(state.advance_cycle(), Some(10));
        assert_eq!(state.cycle_pos, Some(0));

        // Fourth press → pos 1 again → 20
        assert_eq!(state.advance_cycle(), Some(20));
        assert_eq!(state.cycle_pos, Some(1));
    }

    #[test]
    fn advance_cycle_frozen_during_cycling() {
        let mut state = make_state(&[10, 20, 30]);
        assert_eq!(state.cycle_pos, None);

        state.advance_cycle();
        assert!(state.cycle_pos.is_some());

        state.advance_cycle();
        assert!(state.cycle_pos.is_some());
    }

    // ── State::commit_cycle tests ────────────────────────────────────

    #[test]
    fn commit_cycle_promotes_and_unfreezes() {
        let mut state = make_state(&[10, 20, 30]);
        // history = [10, 20, 30]

        // Advance twice: pos 1 → pos 2, targeting 30
        state.advance_cycle(); // pos 1, target=20
        state.advance_cycle(); // pos 2, target=30

        state.commit_cycle();

        // 30 should be promoted to front: [30, 10, 20]
        assert_eq!(state.history, vec![30, 10, 20]);
        assert_eq!(state.last_preview, None);
        assert_eq!(state.cycle_pos, None);
    }

    #[test]
    fn commit_cycle_no_active_cycle_is_noop() {
        let mut state = make_state(&[10, 20]);
        // No cycle active
        state.commit_cycle(); // should not panic
        assert_eq!(state.history, vec![10, 20]);
        assert_eq!(state.cycle_pos, None);
    }

    // ── State::handle_focus_event tests ──────────────────────────────

    #[test]
    fn handle_focus_not_frozen_tracks() {
        let mut state = make_state(&[10, 20]);
        state.handle_focus_event(30);
        // 30 should be at front now
        assert_eq!(state.history, vec![30, 10, 20]);
    }

    #[test]
    fn handle_focus_frozen_matching_preview_ignored() {
        let mut state = make_state(&[10, 20, 30]);
        // Start cycling
        state.advance_cycle(); // pos 1, target=20, cycling
        assert!(state.cycle_pos.is_some());

        // Sway sends focus event for our own preview (20)
        state.handle_focus_event(20);

        // History should be unchanged: [10, 20, 30]
        assert_eq!(state.history, vec![10, 20, 30]);
        assert!(state.cycle_pos.is_some()); // still cycling
    }

    #[test]
    fn handle_focus_frozen_external_auto_committed() {
        let mut state = make_state(&[10, 20, 30]);
        // Start cycling
        state.advance_cycle(); // pos 1, target=20, frozen=true

        // External focus to a window we did NOT preview
        state.handle_focus_event(99);

        // 20 (at cycle_pos=1) should have been promoted, then 99 added to front.
        // Before auto-commit promote: history = [10, 20, 30], promote pos 1 → [20, 10, 30]
        // Then add 99: [99, 20, 10, 30]
        assert_eq!(state.history, vec![99, 20, 10, 30]);

        assert_eq!(state.last_preview, None);
        assert_eq!(state.cycle_pos, None);
    }

    // ── Integration / multi-step scenarios ───────────────────────────

    #[test]
    fn full_alt_tab_cycle() {
        // Add windows A=1, B=2, C=3. History = [3, 2, 1] (C most recent)
        let mut state = make_state(&[3, 2, 1]);

        // Advance twice: first press → pos 1 (2), second press → pos 2 (1)
        assert_eq!(state.advance_cycle(), Some(2));
        assert_eq!(state.advance_cycle(), Some(1));

        // Commit — promotes pos 2 (value 1) to front
        state.commit_cycle();

        // History should be [1, 3, 2]
        assert_eq!(state.history, vec![1, 3, 2]);
        assert_eq!(state.cycle_pos, None);
    }

    #[test]
    fn lazy_commit_scenario() {
        // History: [A=10, B=20, C=30]
        let mut state = make_state(&[10, 20, 30]);

        // Advance once → previews B=20 at pos 1
        assert_eq!(state.advance_cycle(), Some(20));
        assert!(state.cycle_pos.is_some());

        // External focus event to D=40 (user clicked a different window)
        state.handle_focus_event(40);

        // B=20 promoted (from pos 1), then D=40 added to front
        // Promote pos 1: [10, 20, 30] → [20, 10, 30]
        // Add 40: [40, 20, 10, 30]
        assert_eq!(state.history, vec![40, 20, 10, 30]);
        assert_eq!(state.cycle_pos, None);
    }

    #[test]
    fn multiple_cycles_second_uses_updated_history() {
        // History: [A=10, B=20, C=30]
        let mut state = make_state(&[10, 20, 30]);

        // First cycle: advance once (pos 1 = B=20), commit
        state.advance_cycle();
        state.commit_cycle();
        // After commit: [20, 10, 30] (B promoted to front)

        assert_eq!(state.history, vec![20, 10, 30]);

        // Second cycle: should start from updated history
        assert_eq!(state.advance_cycle(), Some(10)); // pos 1 = A=10
        assert_eq!(state.cycle_pos, Some(1));

        // Advance again
        assert_eq!(state.advance_cycle(), Some(30)); // pos 2 = C=30

        // Commit C=30 to front
        state.commit_cycle();
        assert_eq!(state.history, vec![30, 20, 10]);
    }

    // ── State::seed tests ─────────────────────────────────────────────

    #[test]
    fn seed_populates_history() {
        let mut state = State::default();
        state.seed(&[1, 2, 3]);
        // seed adds in order, so 3 is most recent (last added = front)
        assert_eq!(state.history, vec![3, 2, 1]);
    }

    #[test]
    fn seed_deduplicates() {
        let mut state = State::default();
        state.seed(&[1, 2, 1, 3]);
        assert_eq!(state.history, vec![3, 1, 2]);
    }

    // ── State::handle_close_event tests ───────────────────────────────

    #[test]
    fn close_removes_from_history() {
        let mut state = make_state(&[10, 20, 30]);
        state.handle_close_event(20);
        assert_eq!(state.history, vec![10, 30]);
    }

    #[test]
    fn close_nonexistent_is_noop() {
        let mut state = make_state(&[10, 20]);
        state.handle_close_event(99);
        assert_eq!(state.history.len(), 2);
    }

    #[test]
    fn close_preview_target_cancels_cycle() {
        let mut state = make_state(&[10, 20, 30]);
        state.advance_cycle(); // pos 1, target=20, cycling
        assert!(state.cycle_pos.is_some());
        assert_eq!(state.last_preview, Some(20));

        // Close the previewed window
        state.handle_close_event(20);
        assert_eq!(state.cycle_pos, None);
        assert_eq!(state.last_preview, None);
        assert_eq!(state.history.len(), 2);
    }

    #[test]
    fn close_non_preview_during_cycle_clamps_pos() {
        let mut state = make_state(&[10, 20, 30]);
        // Advance twice: pos=2, target=30
        state.advance_cycle(); // pos 1
        state.advance_cycle(); // pos 2, target=30
        assert_eq!(state.cycle_pos, Some(2));

        // Close window 10 (at pos 0 in frozen history [10, 20, 30])
        // After removal: [20, 30], cycle_pos=2 is out of bounds → clamped to 1
        state.handle_close_event(10);
        assert!(state.cycle_pos.is_some());
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.cycle_pos, Some(1));
    }

    #[test]
    fn close_during_cycle_too_few_remaining_cancels() {
        let mut state = make_state(&[10, 20]);
        state.advance_cycle(); // pos 1, target=20, cycling
        assert!(state.cycle_pos.is_some());

        // Close one of the two — only 1 left, can't cycle
        state.handle_close_event(10);
        assert_eq!(state.cycle_pos, None);
    }
}
