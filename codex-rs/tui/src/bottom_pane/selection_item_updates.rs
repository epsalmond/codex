use super::ListSelectionView;
use super::SelectionItem;

impl ListSelectionView {
    pub(super) fn replace_items_preserving_state(&mut self, items: Vec<SelectionItem>) -> bool {
        if !self.tabs.is_empty() {
            return false;
        }

        let selected_key = self
            .selected_actual_idx()
            .and_then(|actual_idx| self.items.get(actual_idx))
            .and_then(|item| item.selection_key.clone());
        let scroll_anchor_key = self
            .filtered_indices
            .get(self.state.scroll_top)
            .and_then(|actual_idx| self.items.get(*actual_idx))
            .and_then(|item| item.selection_key.clone());
        let selected_idx = selected_key.as_ref().and_then(|key| {
            items
                .iter()
                .position(|item| item.selection_key.as_ref() == Some(key))
        });
        let previous_scroll_top = self.state.scroll_top;

        self.items = items;
        self.filtered_indices.clear();
        self.state.selected_idx = None;
        self.state.scroll_top = previous_scroll_top;
        self.initial_selected_idx = selected_idx;
        self.apply_filter();

        if let Some(anchor_key) = scroll_anchor_key
            && let Some(anchor_idx) = self.filtered_indices.iter().position(|actual_idx| {
                self.items
                    .get(*actual_idx)
                    .is_some_and(|item| item.selection_key.as_ref() == Some(&anchor_key))
            })
        {
            self.state.scroll_top = anchor_idx;
            self.state
                .ensure_visible(self.visible_len(), self.visible_rows(self.visible_len()));
        }
        true
    }
}
