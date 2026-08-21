use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::rc::Rc;

use gio::prelude::*;
use gio::subclass::prelude::*;

use crate::ffi::{CellKind, Query, RowWindow};
use crate::model::pager::Pager;
use crate::model::row::ResultRow;
use crate::model::status::{Column, QueryStatus};

const PAGE_SIZE: u64 = 512;
const MAX_RESIDENT_PAGES: usize = 4;
const ROW_CACHE_LIMIT: usize = 256;

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct Live {
        // Declared before `query`, so every DatagrepRows is freed before it.
        pub pager: Pager<RowWindow>,
        pub query: Query,
    }

    pub struct StatusRefresh {
        pub stale_rows: Vec<Range<u64>>,
        pub columns_widened: bool,
    }

    #[derive(Default)]
    pub struct RowCache {
        by_index: HashMap<u64, ResultRow>,
        order: VecDeque<u64>,
    }

    impl RowCache {
        fn get(&mut self, index: u64) -> ResultRow {
            if let Some(row) = self.by_index.get(&index) {
                return row.clone();
            }
            let row = ResultRow::new(index);
            self.by_index.insert(index, row.clone());
            self.order.push_back(index);
            while self.order.len() > ROW_CACHE_LIMIT {
                if let Some(evicted) = self.order.pop_front() {
                    self.by_index.remove(&evicted);
                }
            }
            row
        }

        fn forget(&mut self, rows: Range<u64>) {
            self.by_index.retain(|index, _| !rows.contains(index));
            self.order.retain(|index| !rows.contains(index));
        }

        fn clear(&mut self) {
            self.by_index.clear();
            self.order.clear();
        }
    }

    #[derive(Default)]
    pub struct ResultModel {
        pub live: RefCell<Option<Live>>,
        pub exposed: Cell<u64>,
        pub loaded: Cell<u64>,
        pub columns: RefCell<Vec<Column>>,
        pub status: RefCell<QueryStatus>,
        pub rows: RefCell<RowCache>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ResultModel {
        const NAME: &'static str = "DgResultModel";
        type Type = super::ResultModel;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for ResultModel {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("status-changed").build(),
                    Signal::builder("columns-changed").build(),
                ]
            })
        }
    }

    impl ListModelImpl for ResultModel {
        fn item_type(&self) -> glib::Type {
            ResultRow::static_type()
        }

        fn n_items(&self) -> u32 {
            self.exposed.get() as u32
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            let index = u64::from(position);
            if index >= self.exposed.get() {
                return None;
            }
            let row = self.rows.borrow_mut().get(index);
            Some(row.upcast())
        }
    }

    impl ResultModel {
        pub(super) fn window(&self, row: u64) -> Option<Rc<RowWindow>> {
            let mut live = self.live.borrow_mut();
            let Live { pager, query } = live.as_mut()?;
            pager.get(row, |offset, len| query.rows(offset, len).ok())
        }

        // GTK re-enters through `item` while `items_changed` runs: hold no
        // borrow across an emission.
        pub(super) fn on_progress_tick(&self) {
            if self.live.borrow().is_none() {
                return;
            }
            let exposed = self.exposed.get();
            let refresh = self.refresh_status();
            let obj = self.obj();

            if refresh.columns_widened {
                obj.emit_by_name::<()>("columns-changed", &[]);
                self.rows.borrow_mut().clear();
                if exposed > 0 {
                    obj.items_changed(0, exposed as u32, exposed as u32);
                }
            } else {
                for range in refresh.stale_rows {
                    let start = range.start.min(exposed);
                    let end = range.end.min(exposed);
                    if start >= end {
                        continue;
                    }
                    self.rows.borrow_mut().forget(start..end);
                    let len = (end - start) as u32;
                    obj.items_changed(start as u32, len, len);
                }
            }

            self.expose_loaded_rows();
            obj.emit_by_name::<()>("status-changed", &[]);
        }

        fn refresh_status(&self) -> StatusRefresh {
            let next = match self.live.borrow().as_ref().map(|l| l.query.status_json()) {
                Some(Ok(json)) => QueryStatus::parse(&json),
                Some(Err(e)) => QueryStatus::failed(e.0),
                None => {
                    return StatusRefresh {
                        stale_rows: Vec::new(),
                        columns_widened: false,
                    }
                }
            };

            let mut columns_widened = false;
            {
                let mut columns = self.columns.borrow_mut();
                let known = columns.len();
                if next.columns.len() > known {
                    columns.extend_from_slice(&next.columns[known..]);
                    columns_widened = true;
                }
            }

            let stale_rows = match self.live.borrow_mut().as_mut() {
                Some(live) => {
                    // An older, narrower window would read a new column as
                    // another row's cell.
                    if columns_widened {
                        live.pager.invalidate_all();
                    }
                    live.pager.invalidate_partial(next.rows_loaded)
                }
                None => Vec::new(),
            };

            self.loaded.set(next.rows_loaded);
            *self.status.borrow_mut() = next;
            StatusRefresh {
                stale_rows,
                columns_widened,
            }
        }

        fn expose_loaded_rows(&self) {
            let target = self.loaded.get().min(u64::from(u32::MAX));
            let exposed = self.exposed.get();
            if target <= exposed {
                return;
            }
            self.exposed.set(target);
            self.obj()
                .items_changed(exposed as u32, 0, (target - exposed) as u32);
        }

        pub(super) fn teardown(&self) {
            let removed = self.exposed.get() as u32;
            let had_columns = !self.columns.borrow().is_empty();
            // Out of the RefCell before it drops: freeing a query runs engine code.
            drop(self.live.borrow_mut().take());
            self.exposed.set(0);
            self.loaded.set(0);
            self.columns.borrow_mut().clear();
            *self.status.borrow_mut() = QueryStatus::default();
            self.rows.borrow_mut().clear();
            if removed > 0 {
                self.obj().items_changed(0, removed, 0);
            }
            if had_columns {
                self.obj().emit_by_name::<()>("columns-changed", &[]);
            }
        }
    }
}

glib::wrapper! {
    pub struct ResultModel(ObjectSubclass<imp::ResultModel>) @implements gio::ListModel;
}

impl Default for ResultModel {
    fn default() -> Self {
        Self::new()
    }
}

impl ResultModel {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_query(&self, mut query: Query) {
        let imp = self.imp();
        imp.teardown();

        // bounded(1) coalesces: a full channel means a tick is already queued.
        let (tx, rx) = async_channel::bounded::<()>(1);
        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            while rx.recv().await.is_ok() {
                match weak.upgrade() {
                    Some(model) => model.imp().on_progress_tick(),
                    None => break,
                }
            }
        });
        query.on_progress(move || {
            let _ = tx.try_send(());
        });

        *imp.live.borrow_mut() = Some(imp::Live {
            pager: Pager::new(PAGE_SIZE, MAX_RESIDENT_PAGES),
            query,
        });
        imp.on_progress_tick();
    }

    pub fn reset(&self) {
        self.imp().teardown();
        self.emit_by_name::<()>("status-changed", &[]);
    }

    pub fn with_cell<R>(&self, row: u64, col: u32, f: impl FnOnce(CellKind, &str) -> R) -> R {
        let imp = self.imp();
        if row >= imp.exposed.get() || col >= imp.columns.borrow().len() as u32 {
            return f(CellKind::Pending, "");
        }
        match imp.window(row) {
            Some(window) => match window.kind(row, col) {
                Some(kind) => f(kind, window.cell(row, col).unwrap_or("")),
                None => f(CellKind::Pending, ""),
            },
            None => f(CellKind::Pending, ""),
        }
    }

    pub fn column_count(&self) -> u32 {
        self.imp().columns.borrow().len() as u32
    }

    pub fn column(&self, col: u32) -> Option<Column> {
        self.imp().columns.borrow().get(col as usize).cloned()
    }

    pub fn with_status<R>(&self, f: impl FnOnce(&QueryStatus) -> R) -> R {
        f(&self.imp().status.borrow())
    }

    pub fn cancel(&self) -> Option<String> {
        let live = self.imp().live.borrow();
        live.as_ref()?.query.cancel()
    }

    pub fn cell_detail_json(&self, row: u64, col: u32) -> Option<String> {
        self.imp().window(row)?.cell_detail_json(row, col)
    }

    pub fn envelope_json(&self, row: u64) -> Option<String> {
        self.imp().window(row)?.envelope_json(row)
    }

    pub fn resident_rows(&self) -> u64 {
        match self.imp().live.borrow().as_ref() {
            Some(live) => live.pager.resident_rows(),
            None => 0,
        }
    }

    pub fn resident_pages(&self) -> usize {
        match self.imp().live.borrow().as_ref() {
            Some(live) => live.pager.resident_pages(),
            None => 0,
        }
    }

    pub fn connect_status_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_void("status-changed", f)
    }

    pub fn connect_columns_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_void("columns-changed", f)
    }

    fn connect_void<F: Fn(&Self) + 'static>(&self, signal: &str, f: F) -> glib::SignalHandlerId {
        self.connect_local(signal, false, move |values| {
            let model = values[0]
                .get::<Self>()
                .expect("the signal carries the model");
            f(&model);
            None
        })
    }
}
