//! Which rows of a file pane are selected, and how clicks and keys change that.
//!
//! Rows are tracked by entry name, not by index, so the selection survives a
//! re-sort and a refresh of the same folder.

use std::collections::HashSet;

/// How a click changes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickKind {
    /// Plain click: select only this row.
    Single,
    /// Cmd/Ctrl-click: add or remove this row.
    Toggle,
    /// Shift-click: select every row between the anchor and this one.
    Range,
}

#[derive(Default)]
pub struct FileSelection {
    names: HashSet<String>,
    /// Where a range starts: the last row picked without Shift.
    anchor: Option<String>,
    /// The row the keyboard moves from: the last row picked at all.
    lead: Option<String>,
}

impl FileSelection {
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn lead(&self) -> Option<&str> {
        self.lead.as_deref()
    }

    /// The selected row when exactly one is selected. This is not always the
    /// lead: a Cmd/Ctrl-click that unselects a row leaves the lead on it.
    pub fn only(&self) -> Option<&str> {
        if self.names.len() != 1 {
            return None;
        }
        self.names.iter().next().map(String::as_str)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Apply a click on `name`. `visible` is the list as shown, top to bottom;
    /// a range covers only rows in it.
    pub fn click(&mut self, visible: &[&str], name: &str, kind: ClickKind) {
        match kind {
            ClickKind::Single => self.select_only(name),
            ClickKind::Toggle => {
                if !self.names.remove(name) {
                    self.names.insert(name.to_string());
                }
                self.anchor = Some(name.to_string());
                self.lead = Some(name.to_string());
            }
            ClickKind::Range => self.select_range_to(visible, name),
        }
    }

    pub fn select_only(&mut self, name: &str) {
        self.names.clear();
        self.names.insert(name.to_string());
        self.anchor = Some(name.to_string());
        self.lead = Some(name.to_string());
    }

    pub fn select_all(&mut self, visible: &[&str]) {
        self.names = visible.iter().map(|name| name.to_string()).collect();
        if self
            .anchor
            .as_deref()
            .is_none_or(|anchor| !self.names.contains(anchor))
        {
            self.anchor = visible.first().map(|name| name.to_string());
        }
        self.lead = visible.last().map(|name| name.to_string());
    }

    /// Replace the selection with the rows from the anchor to `name`. Without
    /// an anchor on screen, `name` becomes the anchor and the only row.
    fn select_range_to(&mut self, visible: &[&str], name: &str) {
        let position = |wanted: &str| visible.iter().position(|n| *n == wanted);
        let anchor_position = self.anchor.as_deref().and_then(position);
        let (Some(from), Some(to)) = (anchor_position, position(name)) else {
            self.select_only(name);
            return;
        };
        let (low, high) = if from <= to { (from, to) } else { (to, from) };
        self.names = visible[low..=high].iter().map(|n| n.to_string()).collect();
        self.lead = Some(name.to_string());
    }

    /// Move the lead row by `delta` rows and return its new position in
    /// `visible`. With no lead on screen, a move lands on the first or the
    /// last row.
    pub fn move_lead(&mut self, visible: &[&str], delta: i32, is_extending: bool) -> Option<usize> {
        let last = visible.len().checked_sub(1)?;
        let current = self
            .lead
            .as_deref()
            .and_then(|lead| visible.iter().position(|n| *n == lead));
        let target = match current {
            Some(position) => (position as i64 + delta as i64).clamp(0, last as i64) as usize,
            None if delta > 0 => 0,
            None => last,
        };
        self.move_lead_to(visible, target, is_extending)
    }

    /// Put the lead on the row at `position` (the last row when it is past the
    /// end) and return where it landed. With `is_extending` (Shift held) the
    /// selection grows or shrinks from the anchor; otherwise only that row is
    /// selected.
    pub fn move_lead_to(
        &mut self,
        visible: &[&str],
        position: usize,
        is_extending: bool,
    ) -> Option<usize> {
        let last = visible.len().checked_sub(1)?;
        let target = position.min(last);
        if is_extending {
            self.select_range_to(visible, visible[target]);
        } else {
            self.select_only(visible[target]);
        }
        Some(target)
    }

    /// Drop every selected name that `is_kept` rejects: rows that left the
    /// listing, or that a filter now hides. A hidden row must not be deleted
    /// or transferred along with the ones the user can see.
    pub fn retain(&mut self, is_kept: impl Fn(&str) -> bool) {
        self.names.retain(|name| is_kept(name));
        if self.anchor.as_deref().is_some_and(|name| !is_kept(name)) {
            self.anchor = None;
        }
        if self.lead.as_deref().is_some_and(|name| !is_kept(name)) {
            self.lead = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: [&str; 5] = ["a", "b", "c", "d", "e"];

    fn selected(selection: &FileSelection) -> Vec<&'static str> {
        ROWS.iter()
            .copied()
            .filter(|name| selection.contains(name))
            .collect()
    }

    #[test]
    fn plain_click_selects_one_row() {
        let mut s = FileSelection::default();
        s.click(&ROWS, "b", ClickKind::Single);
        s.click(&ROWS, "d", ClickKind::Single);
        assert_eq!(selected(&s), ["d"]);
        assert_eq!(s.lead(), Some("d"));
    }

    #[test]
    fn toggle_click_adds_and_removes_rows() {
        let mut s = FileSelection::default();
        s.click(&ROWS, "b", ClickKind::Toggle);
        s.click(&ROWS, "d", ClickKind::Toggle);
        assert_eq!(selected(&s), ["b", "d"]);
        s.click(&ROWS, "b", ClickKind::Toggle);
        assert_eq!(selected(&s), ["d"]);
        s.click(&ROWS, "d", ClickKind::Toggle);
        assert!(s.is_empty());
    }

    #[test]
    fn only_is_the_selected_row_not_the_last_clicked_one() {
        let mut s = FileSelection::default();
        assert_eq!(s.only(), None);
        s.click(&ROWS, "a", ClickKind::Single);
        s.click(&ROWS, "b", ClickKind::Toggle);
        assert_eq!(s.only(), None);
        s.click(&ROWS, "b", ClickKind::Toggle);
        assert_eq!(s.lead(), Some("b"));
        assert_eq!(s.only(), Some("a"));
    }

    #[test]
    fn shift_click_selects_the_range_in_either_direction() {
        let mut s = FileSelection::default();
        s.click(&ROWS, "d", ClickKind::Single);
        s.click(&ROWS, "b", ClickKind::Range);
        assert_eq!(selected(&s), ["b", "c", "d"]);
        // The anchor stays, so a second shift-click re-draws the range.
        s.click(&ROWS, "e", ClickKind::Range);
        assert_eq!(selected(&s), ["d", "e"]);
    }

    #[test]
    fn shift_click_without_an_anchor_selects_that_row() {
        let mut s = FileSelection::default();
        s.click(&ROWS, "c", ClickKind::Range);
        assert_eq!(selected(&s), ["c"]);
        s.click(&ROWS, "e", ClickKind::Range);
        assert_eq!(selected(&s), ["c", "d", "e"]);
    }

    #[test]
    fn range_starts_at_the_last_toggled_row() {
        let mut s = FileSelection::default();
        s.click(&ROWS, "a", ClickKind::Single);
        s.click(&ROWS, "c", ClickKind::Toggle);
        s.click(&ROWS, "e", ClickKind::Range);
        assert_eq!(selected(&s), ["c", "d", "e"]);
    }

    #[test]
    fn range_covers_only_visible_rows() {
        let visible = ["a", "c", "e"];
        let mut s = FileSelection::default();
        s.click(&visible, "a", ClickKind::Single);
        s.click(&visible, "e", ClickKind::Range);
        assert_eq!(selected(&s), ["a", "c", "e"]);
    }

    #[test]
    fn select_all_takes_every_visible_row() {
        let visible = ["b", "c"];
        let mut s = FileSelection::default();
        s.select_all(&visible);
        assert_eq!(selected(&s), ["b", "c"]);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn arrow_keys_move_a_single_selection() {
        let mut s = FileSelection::default();
        assert_eq!(s.move_lead(&ROWS, 1, false), Some(0));
        assert_eq!(s.move_lead(&ROWS, 1, false), Some(1));
        assert_eq!(selected(&s), ["b"]);
        assert_eq!(s.move_lead(&ROWS, -5, false), Some(0));
        s.clear();
        assert_eq!(s.move_lead(&ROWS, -1, false), Some(4));
        assert_eq!(s.move_lead(&[], 1, false), None);
    }

    #[test]
    fn shift_arrows_grow_and_shrink_from_the_anchor() {
        let mut s = FileSelection::default();
        s.click(&ROWS, "c", ClickKind::Single);
        s.move_lead(&ROWS, 1, true);
        s.move_lead(&ROWS, 1, true);
        assert_eq!(selected(&s), ["c", "d", "e"]);
        s.move_lead(&ROWS, -1, true);
        assert_eq!(selected(&s), ["c", "d"]);
        s.move_lead(&ROWS, -2, true);
        assert_eq!(selected(&s), ["b", "c"]);
    }

    #[test]
    fn home_and_end_jump_and_extend_from_the_anchor() {
        let mut s = FileSelection::default();
        assert_eq!(s.move_lead_to(&ROWS, usize::MAX, false), Some(4));
        assert_eq!(selected(&s), ["e"]);
        s.click(&ROWS, "c", ClickKind::Single);
        assert_eq!(s.move_lead_to(&ROWS, 0, true), Some(0));
        assert_eq!(selected(&s), ["a", "b", "c"]);
        assert_eq!(s.move_lead_to(&ROWS, usize::MAX, true), Some(4));
        assert_eq!(selected(&s), ["c", "d", "e"]);
        assert_eq!(s.move_lead_to(&[], 0, false), None);
    }

    #[test]
    fn retain_drops_rows_that_are_gone() {
        let mut s = FileSelection::default();
        s.click(&ROWS, "b", ClickKind::Single);
        s.click(&ROWS, "d", ClickKind::Range);
        s.retain(|name| name != "d");
        assert_eq!(selected(&s), ["b", "c"]);
        assert_eq!(s.lead(), None);
        // The anchor "b" is still there, so a range still starts from it.
        s.click(&ROWS, "a", ClickKind::Range);
        assert_eq!(selected(&s), ["a", "b"]);
    }
}
