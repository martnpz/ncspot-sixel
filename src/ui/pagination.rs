use crate::library::Library;
use crate::traits::ListItem;
use log::debug;
use std::sync::{Arc, RwLock};

pub struct ApiPage<I> {
    pub offset: u32,
    pub total: u32,
    pub items: Vec<I>,
}
pub type FetchPageFn<I> = dyn Fn(u32) -> Option<ApiPage<I>> + Send + Sync;
pub struct ApiResult<I> {
    offset: Arc<RwLock<u32>>,
    limit: u32,
    first_page_loaded: bool,
    pub total: u32,
    pub items: Arc<RwLock<Vec<I>>>,
    fetch_page: Arc<FetchPageFn<I>>,
}

impl<I: ListItem + Clone> ApiResult<I> {
    pub fn new(limit: u32, fetch_page: Arc<FetchPageFn<I>>) -> Self {
        let items = Arc::new(RwLock::new(Vec::new()));
        if let Some(first_page) = fetch_page(0) {
            debug!(
                "fetched first page, items: {}, total: {}",
                first_page.items.len(),
                first_page.total
            );
            items.write().unwrap().extend(first_page.items);
            Self {
                offset: Arc::new(RwLock::new(first_page.offset)),
                limit,
                first_page_loaded: true,
                total: first_page.total,
                items,
                fetch_page: fetch_page.clone(),
            }
        } else {
            Self {
                offset: Arc::new(RwLock::new(0)),
                limit,
                first_page_loaded: false,
                total: 0,
                items,
                fetch_page: fetch_page.clone(),
            }
        }
    }

    pub fn first_page_loaded(&self) -> bool {
        self.first_page_loaded
    }

    /// A failed page is not end-of-results. Callers must retain their old cache.
    pub fn all(&self) -> Option<Vec<I>> {
        if !self.first_page_loaded {
            return None;
        }
        while !self.at_end() {
            self.next()?;
        }
        Some(self.items.read().unwrap().clone())
    }

    fn offset(&self) -> u32 {
        *self.offset.read().unwrap()
    }

    pub fn at_end(&self) -> bool {
        (self.offset() + self.limit) >= self.total
    }

    pub fn apply_pagination(self, pagination: &Pagination<I>) {
        let total = self.total as usize;
        let fetched_items = self.items.read().unwrap().len();
        pagination.set(
            fetched_items,
            total,
            Box::new(move |_| {
                self.next();
            }),
        )
    }

    pub fn next(&self) -> Option<Vec<I>> {
        let offset = self.offset() + self.limit;
        debug!("fetching next page at offset {offset}");
        if !self.at_end() {
            if let Some(next_page) = (self.fetch_page)(offset) {
                *self.offset.write().unwrap() = next_page.offset;
                self.items.write().unwrap().extend(next_page.items.clone());
                Some(next_page.items)
            } else {
                None
            }
        } else {
            debug!("paginator is at end");
            None
        }
    }
}

pub type Paginator<I> = Box<dyn Fn(Arc<RwLock<Vec<I>>>) + Send + Sync>;

/// Manages the loading of ListItems, to increase performance and decrease
/// memory usage.
///
/// `loaded_content`: The amount of currently loaded items
/// `max_content`: The maximum amount of items
/// `callback`: TODO: document
/// `busy`: TODO: document
#[derive(Clone)]
pub struct Pagination<I: ListItem> {
    loaded_content: Arc<RwLock<usize>>,
    max_content: Arc<RwLock<Option<usize>>>,
    callback: Arc<RwLock<Option<Paginator<I>>>>,
    busy: Arc<RwLock<bool>>,
    total_known: Arc<RwLock<bool>>,
}

impl<I: ListItem> Default for Pagination<I> {
    fn default() -> Self {
        Self {
            loaded_content: Arc::new(RwLock::new(0)),
            max_content: Arc::new(RwLock::new(None)),
            callback: Arc::new(RwLock::new(None)),
            busy: Arc::new(RwLock::new(false)),
            total_known: Arc::new(RwLock::new(true)),
        }
    }
}

impl<I: ListItem + Clone> Pagination<I> {
    pub fn clear(&mut self) {
        *self.max_content.write().unwrap() = None;
        *self.callback.write().unwrap() = None;
    }
    pub fn set(&self, loaded_content: usize, max_content: usize, callback: Paginator<I>) {
        *self.total_known.write().unwrap() = true;
        *self.loaded_content.write().unwrap() = loaded_content;
        *self.max_content.write().unwrap() = Some(max_content);
        *self.callback.write().unwrap() = Some(callback);
    }

    /// Update search progress without retaining this paginator's own callback.
    /// Search can probe another page, but does not have a reliable result total.
    pub fn search_progress_callback(&self) -> Box<dyn Fn(usize, bool) + Send + Sync> {
        let total_known = self.total_known.clone();
        let loaded_content = self.loaded_content.clone();
        let max_content = self.max_content.clone();
        Box::new(move |loaded, has_more| {
            *total_known.write().unwrap() = false;
            *loaded_content.write().unwrap() = loaded;
            *max_content.write().unwrap() = Some(loaded + usize::from(has_more));
        })
    }

    pub fn total_known(&self) -> bool {
        *self.total_known.read().unwrap()
    }

    pub fn loaded_content(&self) -> usize {
        *self.loaded_content.read().unwrap()
    }

    pub fn max_content(&self) -> Option<usize> {
        *self.max_content.read().unwrap()
    }

    fn is_busy(&self) -> bool {
        *self.busy.read().unwrap()
    }

    pub fn call(&self, content: &Arc<RwLock<Vec<I>>>, library: Arc<Library>) {
        let pagination = self.clone();
        let content = content.clone();
        if !self.is_busy() {
            *self.busy.write().unwrap() = true;
            std::thread::spawn(move || {
                let cb = pagination.callback.read().unwrap();
                if let Some(ref cb) = *cb {
                    debug!("calling paginator!");
                    cb(content.clone());
                    *pagination.loaded_content.write().unwrap() = content.read().unwrap().len();
                }
                *pagination.busy.write().unwrap() = false;
                library.trigger_redraw();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::category::Category;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn first_page_failure_is_distinct_from_an_empty_collection() {
        let failed = ApiResult::<Category>::new(50, Arc::new(|_| None));
        assert!(!failed.first_page_loaded());
        assert!(failed.all().is_none());
        let empty = ApiResult::<Category>::new(
            50,
            Arc::new(|_| {
                Some(ApiPage {
                    offset: 0,
                    total: 0,
                    items: vec![],
                })
            }),
        );
        assert!(empty.all().unwrap().is_empty());
    }

    #[test]
    fn later_page_failure_stops_without_busy_loop_or_partial_success() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let result = ApiResult::new(
            1,
            Arc::new(move |offset| {
                counter.fetch_add(1, Ordering::Relaxed);
                if offset > 0 {
                    return None;
                }
                Some(ApiPage {
                    offset: 0,
                    total: 2,
                    items: vec![Category {
                        id: "a".to_owned(),
                        name: "A".to_owned(),
                    }],
                })
            }),
        );
        assert!(result.all().is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
