use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::rc::Rc;

pub trait WindowMeta {
    fn offset(&self) -> u64;
    fn count(&self) -> u64;

    fn contains(&self, row: u64) -> bool {
        row >= self.offset() && row < self.offset() + self.count()
    }
}

pub struct Pager<W> {
    pages: HashMap<u64, Rc<W>>,
    order: VecDeque<u64>,
    page_size: u64,
    max_pages: usize,
}

impl<W: WindowMeta> Pager<W> {
    pub fn new(page_size: u64, max_pages: usize) -> Self {
        Self {
            pages: HashMap::new(),
            order: VecDeque::new(),
            page_size: page_size.max(1),
            max_pages: max_pages.max(1),
        }
    }

    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    pub fn get(&mut self, row: u64, fetch: impl FnOnce(u64, u64) -> Option<W>) -> Option<Rc<W>> {
        let page = row / self.page_size;
        if let Some(window) = self.pages.get(&page) {
            let window = Rc::clone(window);
            self.touch(page);
            return window.contains(row).then_some(window);
        }
        let window = Rc::new(fetch(page * self.page_size, self.page_size)?);
        self.pages.insert(page, Rc::clone(&window));
        self.order.push_back(page);
        while self.order.len() > self.max_pages {
            if let Some(victim) = self.order.pop_front() {
                self.pages.remove(&victim); // datagrep_rows_free happens here
            }
        }
        window.contains(row).then_some(window)
    }

    pub fn invalidate_all(&mut self) {
        self.pages.clear();
        self.order.clear();
    }

    /// Returns the rows that were drawing as skeletons out of the dropped pages.
    /// A short page is stale only once `loaded` reaches past it: the last page of
    /// a finished result is short forever.
    pub fn invalidate_partial(&mut self, loaded: u64) -> Vec<Range<u64>> {
        let size = self.page_size;
        let mut stale: Vec<Range<u64>> = Vec::new();
        let dropped: Vec<u64> = self
            .pages
            .iter()
            .filter(|(&page, w)| page * size + w.count() < ((page + 1) * size).min(loaded))
            .map(|(&page, _)| page)
            .collect();
        for page in dropped {
            if let Some(window) = self.pages.remove(&page) {
                stale.push(page * size + window.count()..((page + 1) * size).min(loaded));
            }
            self.order.retain(|&p| p != page);
        }
        stale.sort_by_key(|r| r.start);
        stale
    }

    pub fn resident_pages(&self) -> usize {
        self.pages.len()
    }

    pub fn resident_rows(&self) -> u64 {
        self.pages.values().map(|w| w.count()).sum()
    }

    fn touch(&mut self, page: u64) {
        self.order.retain(|&p| p != page);
        self.order.push_back(page);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        offset: u64,
        count: u64,
    }

    impl WindowMeta for Fake {
        fn offset(&self) -> u64 {
            self.offset
        }
        fn count(&self) -> u64 {
            self.count
        }
    }

    struct Source {
        loaded: u64,
        fetches: RefCell<Vec<(u64, u64)>>,
    }

    impl Source {
        fn new(loaded: u64) -> Self {
            Self {
                loaded,
                fetches: RefCell::new(Vec::new()),
            }
        }

        fn window(&self, offset: u64, len: u64) -> Option<Fake> {
            self.fetches.borrow_mut().push((offset, len));
            Some(Fake {
                offset,
                count: self.loaded.saturating_sub(offset).min(len),
            })
        }
    }

    #[test]
    fn a_miss_fetches_exactly_one_page_and_a_hit_fetches_nothing() {
        let src = Source::new(10_000);
        let mut pager = Pager::new(512, 4);

        assert!(pager.get(700, |o, l| src.window(o, l)).is_some());
        assert_eq!(src.fetches.borrow().as_slice(), &[(512, 512)]);

        assert!(pager.get(1023, |o, l| src.window(o, l)).is_some());
        assert_eq!(src.fetches.borrow().len(), 1, "same page, no second fetch");
    }

    #[test]
    fn residency_is_capped_and_eviction_is_least_recently_used() {
        let src = Source::new(100_000);
        let mut pager = Pager::new(512, 4);

        for page in 0..4 {
            pager.get(page * 512, |o, l| src.window(o, l));
        }
        pager.get(0, |o, l| src.window(o, l));
        pager.get(4 * 512, |o, l| src.window(o, l));

        assert_eq!(pager.resident_pages(), 4);
        assert_eq!(pager.resident_rows(), 4 * 512);
        let before = src.fetches.borrow().len();
        pager.get(10, |o, l| src.window(o, l));
        assert_eq!(
            src.fetches.borrow().len(),
            before,
            "page 0 is still resident"
        );
    }

    #[test]
    fn a_far_row_never_materialises_anything_in_between() {
        let src = Source::new(2_000_000);
        let mut pager = Pager::new(512, 4);

        assert!(pager.get(999_999, |o, l| src.window(o, l)).is_some());
        assert_eq!(pager.resident_rows(), 512);
        assert_eq!(src.fetches.borrow().as_slice(), &[(999_936, 512)]);
    }

    #[test]
    fn a_row_past_the_stream_is_a_skeleton_not_a_wait() {
        let src = Source::new(100);
        let mut pager = Pager::new(512, 4);

        assert!(pager.get(300, |o, l| src.window(o, l)).is_none());
        assert!(pager.get(50, |o, l| src.window(o, l)).is_some());
    }

    #[test]
    fn only_the_uncovered_tail_of_a_short_page_is_reported_stale() {
        let src = Source::new(100);
        let mut pager = Pager::new(512, 4);
        pager.get(50, |o, l| src.window(o, l));

        assert_eq!(pager.invalidate_partial(5_000), vec![100..512]);
        assert_eq!(pager.resident_pages(), 0);
    }

    #[test]
    fn a_full_page_survives_invalidation() {
        let src = Source::new(4096);
        let mut pager = Pager::new(512, 4);
        pager.get(0, |o, l| src.window(o, l));

        assert!(pager.invalidate_partial(4096).is_empty());
        assert_eq!(pager.resident_pages(), 1);
        pager.invalidate_all();
        assert_eq!(pager.resident_pages(), 0);
    }

    #[test]
    fn the_short_last_page_of_a_finished_result_is_not_thrashed() {
        let src = Source::new(5_000);
        let mut pager = Pager::new(512, 4);
        pager.get(4_999, |o, l| src.window(o, l)); // page 9: 4608..5000, short

        assert!(pager.invalidate_partial(5_000).is_empty());
        assert_eq!(pager.resident_pages(), 1);
    }
}
