use std::collections::{HashSet, VecDeque};

pub(super) const PERSISTENT_RECENT_REQUEST_CAPACITY: usize = 64;

pub(super) struct RecentRequestIds {
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl RecentRequestIds {
    pub(super) fn new() -> Self {
        Self {
            order: VecDeque::with_capacity(PERSISTENT_RECENT_REQUEST_CAPACITY),
            ids: HashSet::with_capacity(PERSISTENT_RECENT_REQUEST_CAPACITY),
        }
    }

    #[cfg(test)]
    pub(super) fn contains(&self, request_id: &str) -> bool {
        self.ids.contains(request_id)
    }

    pub(super) fn insert(&mut self, request_id: String) -> bool {
        if self.ids.contains(&request_id) {
            return false;
        }
        if self.order.len() == PERSISTENT_RECENT_REQUEST_CAPACITY {
            let old = self
                .order
                .pop_front()
                .expect("recent request queue is full");
            self.ids.remove(&old);
        }
        self.ids.insert(request_id.clone());
        self.order.push_back(request_id);
        true
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.order.len()
    }
}
