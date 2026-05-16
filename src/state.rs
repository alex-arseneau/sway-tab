// sway-tab — Pure state logic, no async, no IPC.

use std::collections::VecDeque;

/// Persistent history of recently focused windows, populated by focus events.
/// Deduplicates and maintains recency order so Alt+Tab cycles through
/// windows in the order the user last visited them. Size is bounded
/// naturally by the number of open windows — closed windows are removed
/// via `remove()`.
#[derive(Default)]
pub struct WindowHistory {
    /// Ordered from most-recent (index 0) to least-recent.
    /// Index 0 = current window, index 1 = previously focused.
    history: VecDeque<i64>,
    /// True while the user is actively cycling — prevents preview
    /// focus events from polluting the history.
    frozen: bool,
}

impl WindowHistory {
    pub fn new() -> Self {
        Self {
            history: VecDeque::new(),
            frozen: false,
        }
    }

    /// Add con_id to front of history. If already present, move it.
    pub fn add(&mut self, con_id: i64) {
        tracing::trace!("history.add: con_id={con_id}");
        self.history.retain(|&id| id != con_id);
        self.history.push_front(con_id);
        tracing::trace!("history.add: new len={}", self.history.len());
    }

    /// Remove a window from history (e.g. when it closes).
    pub fn remove(&mut self, con_id: i64) {
        tracing::trace!("history.remove: con_id={con_id}");
        self.history.retain(|&id| id != con_id);
        tracing::trace!("history.remove: new len={}", self.history.len());
    }

    pub fn get(&self, pos: usize) -> Option<i64> {
        self.history.get(pos).copied()
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// Move the item at `pos` to the front (used on commit).
    pub fn promote(&mut self, pos: usize) {
        tracing::trace!("history.promote: pos={pos}");
        if let Some(con_id) = self.history.remove(pos) {
            self.history.push_front(con_id);
            tracing::trace!("history.promote: con_id={con_id} promoted to front");
        }
    }
}

/// What happened when a focus event was handled.
#[derive(Debug, PartialEq)]
pub enum FocusAction {
    /// Normal tracking — con_id was added to history
    Tracked,
    /// Our own preview focus — ignored
    Ignored,
    /// External focus triggered lazy auto-commit, new window added
    AutoCommitted,
}

/// Shared state between the focus-event loop and the signal handlers.
pub struct State {
    pub history: WindowHistory,
    /// Some(_) when the user is actively cycling.
    pub cycle_pos: Option<usize>,
    /// The con_id we most recently told sway to focus (our own preview).
    /// Used to distinguish our preview focus events from real user switches.
    pub last_preview: Option<i64>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            history: WindowHistory::new(),
            cycle_pos: None,
            last_preview: None,
        }
    }
}

impl State {
    /// Seed the history with existing window con_ids (e.g. from sway get_tree).
    /// Added in order — the first id in the slice will be at the back (oldest),
    /// the last will be at the front (most recent). Typically called once at
    /// startup.
    pub fn seed(&mut self, con_ids: &[i64]) {
        tracing::trace!("state.seed: {} windows", con_ids.len());
        for &id in con_ids {
            self.history.add(id);
        }
    }

    /// Handle a window close event. Removes the window from history.
    /// If the closed window was the current cycle target, cancels the cycle.
    pub fn handle_close_event(&mut self, con_id: i64) {
        tracing::trace!("handle_close_event: con_id={con_id}");
        self.history.remove(con_id);

        // If we were cycling and the closed window was our preview target,
        // or the history is now too short, cancel the cycle.
        if self.history.frozen {
            if self.last_preview == Some(con_id) || self.history.len() < 2 {
                tracing::trace!("handle_close_event: cancelling active cycle");
                self.cycle_pos = None;
                self.last_preview = None;
                self.history.frozen = false;
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
        tracing::trace!("advance_cycle: called");

        if self.history.len() < 2 {
            tracing::trace!("advance_cycle: history too short, skipping");
            return None;
        }

        if self.cycle_pos.is_none() {
            // First press: freeze history so preview focuses don't pollute it.
            tracing::trace!("advance_cycle: first press, freezing history, cycle_pos=1");
            self.history.frozen = true;
            self.cycle_pos = Some(1);
        } else {
            let pos = self.cycle_pos.unwrap();
            let next_pos = (pos + 1) % self.history.len();
            tracing::trace!("advance_cycle: advancing cycle_pos {pos} -> {next_pos}");
            self.cycle_pos = Some(next_pos);
        }

        let target = self.history.get(self.cycle_pos.unwrap()).unwrap();
        tracing::trace!("advance_cycle: target con_id={target} for preview");
        self.last_preview = Some(target);

        Some(target)
    }

    /// Commit current cycle position — promote to front, unfreeze, clear last_preview.
    /// No-op if no active cycle.
    pub fn commit_cycle(&mut self) {
        tracing::trace!("commit_cycle: called");
        if let Some(pos) = self.cycle_pos.take() {
            tracing::trace!("commit_cycle: committing pos={pos}, unfreezing history");
            self.history.promote(pos);
            self.history.frozen = false;
            self.last_preview = None;
        } else {
            tracing::trace!("commit_cycle: no active cycle, nothing to commit");
        }
    }

    /// Handle an incoming focus event. Returns what action was taken.
    pub fn handle_focus_event(&mut self, con_id: i64) -> FocusAction {
        tracing::trace!("event: focus change on con_id={con_id}");
        if !self.history.frozen {
            self.history.add(con_id);
            FocusAction::Tracked
        } else if self.last_preview == Some(con_id) {
            tracing::trace!("event: focus ignored (our own preview)");
            FocusAction::Ignored
        } else {
            // User moved focus to something we didn't select — lazy commit.
            tracing::trace!(
                "event: external focus change to con_id={con_id}, auto-committing"
            );
            if let Some(pos) = self.cycle_pos.take() {
                self.history.promote(pos);
                tracing::trace!("event: auto-commit promoted pos={pos}");
            }
            self.history.frozen = false;
            self.last_preview = None;
            self.history.add(con_id);
            FocusAction::AutoCommitted
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
            state.history.add(id);
        }
        state
    }

    // ── WindowHistory tests ──────────────────────────────────────────

    #[test]
    fn history_add_front_and_dedup() {
        let mut h = WindowHistory::new();
        h.add(1);
        h.add(2);
        h.add(3);
        // Order should be [3, 2, 1]
        assert_eq!(h.get(0), Some(3));
        assert_eq!(h.get(1), Some(2));
        assert_eq!(h.get(2), Some(1));

        // Re-adding 1 should move it to front: [1, 3, 2]
        h.add(1);
        assert_eq!(h.get(0), Some(1));
        assert_eq!(h.get(1), Some(3));
        assert_eq!(h.get(2), Some(2));
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn history_remove_existing() {
        let mut h = WindowHistory::new();
        h.add(1);
        h.add(2);
        h.add(3);
        // [3, 2, 1]
        h.remove(2);
        assert_eq!(h.len(), 2);
        assert_eq!(h.get(0), Some(3));
        assert_eq!(h.get(1), Some(1));
    }

    #[test]
    fn history_remove_nonexistent_is_noop() {
        let mut h = WindowHistory::new();
        h.add(1);
        h.add(2);
        h.remove(99);
        assert_eq!(h.len(), 2);
        assert_eq!(h.get(0), Some(2));
        assert_eq!(h.get(1), Some(1));
    }

    #[test]
    fn history_get_returns_correct_items() {
        let mut h = WindowHistory::new();
        h.add(100);
        h.add(200);
        h.add(300);
        // [300, 200, 100]
        assert_eq!(h.get(0), Some(300));
        assert_eq!(h.get(1), Some(200));
        assert_eq!(h.get(2), Some(100));
        assert_eq!(h.get(3), None);
    }

    #[test]
    fn history_promote_moves_to_front() {
        let mut h = WindowHistory::new();
        h.add(1);
        h.add(2);
        h.add(3);
        // [3, 2, 1]
        h.promote(2); // promote item at index 2 (which is 1)
        // Now: [1, 3, 2]
        assert_eq!(h.get(0), Some(1));
        assert_eq!(h.get(1), Some(3));
        assert_eq!(h.get(2), Some(2));
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn history_promote_out_of_bounds_is_noop() {
        let mut h = WindowHistory::new();
        h.add(1);
        h.add(2);
        // [2, 1]
        h.promote(5); // out of bounds — should do nothing
        assert_eq!(h.get(0), Some(2));
        assert_eq!(h.get(1), Some(1));
        assert_eq!(h.len(), 2);
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
        assert!(state.history.frozen);
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
        assert!(!state.history.frozen);

        state.advance_cycle();
        assert!(state.history.frozen);

        state.advance_cycle();
        assert!(state.history.frozen);
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
        assert_eq!(state.history.get(0), Some(30));
        assert_eq!(state.history.get(1), Some(10));
        assert_eq!(state.history.get(2), Some(20));
        assert!(!state.history.frozen);
        assert_eq!(state.last_preview, None);
        assert_eq!(state.cycle_pos, None);
    }

    #[test]
    fn commit_cycle_no_active_cycle_is_noop() {
        let mut state = make_state(&[10, 20]);
        // No cycle active
        state.commit_cycle(); // should not panic
        assert_eq!(state.history.get(0), Some(10));
        assert_eq!(state.history.get(1), Some(20));
        assert_eq!(state.cycle_pos, None);
        assert!(!state.history.frozen);
    }

    // ── State::handle_focus_event tests ──────────────────────────────

    #[test]
    fn handle_focus_not_frozen_returns_tracked() {
        let mut state = make_state(&[10, 20]);
        let action = state.handle_focus_event(30);
        assert_eq!(action, FocusAction::Tracked);
        // 30 should be at front now
        assert_eq!(state.history.get(0), Some(30));
        assert_eq!(state.history.get(1), Some(10));
        assert_eq!(state.history.get(2), Some(20));
    }

    #[test]
    fn handle_focus_frozen_matching_preview_returns_ignored() {
        let mut state = make_state(&[10, 20, 30]);
        // Start cycling
        state.advance_cycle(); // pos 1, target=20, frozen=true
        assert!(state.history.frozen);

        // Sway sends focus event for our own preview (20)
        let action = state.handle_focus_event(20);
        assert_eq!(action, FocusAction::Ignored);

        // History should be unchanged: [10, 20, 30]
        assert_eq!(state.history.get(0), Some(10));
        assert_eq!(state.history.get(1), Some(20));
        assert_eq!(state.history.get(2), Some(30));
        assert!(state.history.frozen); // still frozen
    }

    #[test]
    fn handle_focus_frozen_external_returns_auto_committed() {
        let mut state = make_state(&[10, 20, 30]);
        // Start cycling
        state.advance_cycle(); // pos 1, target=20, frozen=true

        // External focus to a window we did NOT preview
        let action = state.handle_focus_event(99);
        assert_eq!(action, FocusAction::AutoCommitted);

        // 20 (at cycle_pos=1) should have been promoted, then 99 added to front.
        // Before auto-commit promote: history = [10, 20, 30], promote pos 1 → [20, 10, 30]
        // Then add 99: [99, 20, 10, 30]
        assert_eq!(state.history.get(0), Some(99));
        assert_eq!(state.history.get(1), Some(20));
        assert_eq!(state.history.get(2), Some(10));
        assert_eq!(state.history.get(3), Some(30));

        assert!(!state.history.frozen);
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
        assert_eq!(state.history.get(0), Some(1));
        assert_eq!(state.history.get(1), Some(3));
        assert_eq!(state.history.get(2), Some(2));
        assert!(!state.history.frozen);
        assert_eq!(state.cycle_pos, None);

        // All original windows still present
        assert_eq!(state.history.len(), 3);
    }

    #[test]
    fn lazy_commit_scenario() {
        // History: [A=10, B=20, C=30]
        let mut state = make_state(&[10, 20, 30]);

        // Advance once → previews B=20 at pos 1
        assert_eq!(state.advance_cycle(), Some(20));
        assert!(state.history.frozen);

        // External focus event to D=40 (user clicked a different window)
        let action = state.handle_focus_event(40);
        assert_eq!(action, FocusAction::AutoCommitted);

        // B=20 promoted (from pos 1), then D=40 added to front
        // Promote pos 1: [10, 20, 30] → [20, 10, 30]
        // Add 40: [40, 20, 10, 30]
        assert_eq!(state.history.get(0), Some(40));
        assert_eq!(state.history.get(1), Some(20));
        assert_eq!(state.history.get(2), Some(10));
        assert_eq!(state.history.get(3), Some(30));
        assert!(!state.history.frozen);
    }

    #[test]
    fn multiple_cycles_second_uses_updated_history() {
        // History: [A=10, B=20, C=30]
        let mut state = make_state(&[10, 20, 30]);

        // First cycle: advance once (pos 1 = B=20), commit
        state.advance_cycle();
        state.commit_cycle();
        // After commit: [20, 10, 30] (B promoted to front)

        assert_eq!(state.history.get(0), Some(20));
        assert_eq!(state.history.get(1), Some(10));
        assert_eq!(state.history.get(2), Some(30));

        // Second cycle: should start from updated history
        assert_eq!(state.advance_cycle(), Some(10)); // pos 1 = A=10
        assert_eq!(state.cycle_pos, Some(1));
        assert!(state.history.frozen);

        // Advance again
        assert_eq!(state.advance_cycle(), Some(30)); // pos 2 = C=30

        // Commit C=30 to front
        state.commit_cycle();
        assert_eq!(state.history.get(0), Some(30));
        assert_eq!(state.history.get(1), Some(20));
        assert_eq!(state.history.get(2), Some(10));
    }

    // ── State::seed tests ─────────────────────────────────────────────

    #[test]
    fn seed_populates_history() {
        let mut state = State::default();
        state.seed(&[1, 2, 3]);
        // seed adds in order, so 3 is most recent (last added = front)
        assert_eq!(state.history.len(), 3);
        assert_eq!(state.history.get(0), Some(3));
        assert_eq!(state.history.get(1), Some(2));
        assert_eq!(state.history.get(2), Some(1));
    }

    #[test]
    fn seed_deduplicates() {
        let mut state = State::default();
        state.seed(&[1, 2, 1, 3]);
        assert_eq!(state.history.len(), 3);
        assert_eq!(state.history.get(0), Some(3));
        assert_eq!(state.history.get(1), Some(1));
        assert_eq!(state.history.get(2), Some(2));
    }

    // ── State::handle_close_event tests ───────────────────────────────

    #[test]
    fn close_removes_from_history() {
        let mut state = make_state(&[10, 20, 30]);
        state.handle_close_event(20);
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history.get(0), Some(10));
        assert_eq!(state.history.get(1), Some(30));
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
        state.advance_cycle(); // pos 1, target=20, frozen
        assert!(state.history.frozen);
        assert_eq!(state.last_preview, Some(20));

        // Close the previewed window
        state.handle_close_event(20);
        assert!(!state.history.frozen);
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
        assert!(state.history.frozen);
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.cycle_pos, Some(1));
    }

    #[test]
    fn close_during_cycle_too_few_remaining_cancels() {
        let mut state = make_state(&[10, 20]);
        state.advance_cycle(); // pos 1, target=20, frozen
        assert!(state.history.frozen);

        // Close one of the two — only 1 left, can't cycle
        state.handle_close_event(10);
        assert!(!state.history.frozen);
        assert_eq!(state.cycle_pos, None);
    }
}
