use std::cell::Cell;

use glib::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ResultRow {
        pub index: Cell<u64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ResultRow {
        const NAME: &'static str = "DgResultRow";
        type Type = super::ResultRow;
    }

    impl ObjectImpl for ResultRow {}
}

glib::wrapper! {
    pub struct ResultRow(ObjectSubclass<imp::ResultRow>);
}

impl ResultRow {
    pub(crate) fn new(index: u64) -> Self {
        let row: Self = glib::Object::new();
        row.imp().index.set(index);
        row
    }

    /// The row's index in the result, which a `GtkListItem` position is not.
    pub fn index(&self) -> u64 {
        self.imp().index.get()
    }
}
