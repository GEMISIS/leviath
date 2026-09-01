//! The Defaults screen's provider priority: reading it out of the form, and
//! the reorder modal that edits it.
//!
//! The provider field is an ordered `provider_order`, not a single default, so
//! it opens a drag-to-reorder modal rather than the chooser. These methods keep
//! that apart from the rest of the wizard state, which is otherwise about
//! credentials and screens.

use super::{FieldValue, Wizard};

impl Wizard {
    /// Open the reorder modal for the Defaults field the cursor is on, seeded
    /// with its current order. A no-op for a field that is not an ordered list.
    pub(in crate::commands::setup) fn open_reorder(&mut self) {
        use crate::tui::widgets::reorder::{Reorder, ReorderItem};
        let field = self.cursor;
        let Some(order) = self.defaults.get(field).and_then(|f| f.value.order()) else {
            return;
        };
        let items: Vec<ReorderItem> = order
            .iter()
            .map(|value| ReorderItem {
                detail: self.provider_detail(value),
                value: value.clone(),
            })
            .collect();
        self.reorder_field = field;
        self.reorder = Some(Reorder::new(
            "Provider priority",
            Self::precedence_explanation(true)
                .into_iter()
                .take(2)
                .map(str::to_string)
                .collect(),
            items,
        ));
    }

    /// Take the reorder modal's answer, writing the new order back into the
    /// field it came from.
    pub(in crate::commands::setup) fn commit_reorder(&mut self, order: Vec<String>) {
        if let Some(field) = self.defaults.get_mut(self.reorder_field) {
            field.value = FieldValue::Order(order);
        }
        self.dirty = true;
        // The head of the order is the default provider, so the concurrency
        // default follows it exactly as the picker's does.
        if self.reorder_field == Self::PROVIDER_FIELD {
            self.apply_provider_concurrency_default();
        }
    }

    /// The default provider as it currently stands in the form, or the base
    /// config's value before the form exists.
    ///
    /// The provider field is the priority order now, so the default provider is
    /// its head - the one a bare model name prefers first.
    pub(super) fn current_default_provider(&self) -> String {
        self.defaults
            .first()
            .and_then(|f| f.value.order())
            .and_then(|order| order.first())
            .cloned()
            .unwrap_or_else(|| self.base.default_provider.clone())
    }

    /// The provider priority as it currently stands in the form.
    pub(super) fn current_provider_order(&self) -> Vec<String> {
        self.defaults
            .first()
            .and_then(|f| f.value.order())
            .map(<[String]>::to_vec)
            .unwrap_or_default()
    }
}
