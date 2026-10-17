// Connection-local request windows prevent proxy-id reuse from authorizing later errors.
use std::{cell::RefCell, collections::HashMap};

// Extract resources without running native destructors under a map borrow.
pub(crate) fn take_unselected<T>(
    routes: &RefCell<HashMap<u32, T>>,
    selected: impl Fn(u32) -> bool,
) -> Vec<(u32, T)> {
    let mut routes = routes.borrow_mut();
    let removed: Vec<_> = routes.keys().copied().filter(|id| !selected(*id)).collect();
    removed
        .into_iter()
        .filter_map(|id| routes.remove(&id).map(|resource| (id, resource)))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RequestWindow {
    pub(crate) before: i32,
    pub(crate) after: i32,
}

impl RequestWindow {
    pub(crate) fn contains(self, sequence: i32) -> bool {
        // SPA asynchronous request sequences use the low 30 bits and wrap.
        const MASK: i32 = 0x3fff_ffff;
        if [self.before, self.after, sequence]
            .iter()
            .any(|seq| !(0..=MASK).contains(seq))
        {
            return false;
        }
        let length = (self.after - self.before) & MASK;
        let offset = (sequence - self.before) & MASK;
        length > 0 && offset > 0 && offset < length
    }
}

pub(crate) struct ProxyReleases<T> {
    released: Vec<(u32, RequestWindow)>,
    deferred: Vec<(i32, Vec<u32>, T)>,
}

impl<T> Default for ProxyReleases<T> {
    fn default() -> Self {
        Self {
            released: Vec::new(),
            deferred: Vec::new(),
        }
    }
}

impl<T> ProxyReleases<T> {
    pub(crate) fn expect_destroy(&mut self, proxy_id: u32, window: RequestWindow) {
        self.released.push((proxy_id, window));
    }
    pub(crate) fn was_released(&self, proxy_id: u32, sequence: i32) -> bool {
        self.released
            .iter()
            .any(|(id, window)| *id == proxy_id && window.contains(sequence))
    }
    pub(crate) fn defer(&mut self, checkpoint: i32, routes: Vec<u32>, resource: T) {
        self.deferred.push((checkpoint, routes, resource));
    }
    pub(crate) fn is_deferred(&self, route: u32) -> bool {
        self.deferred
            .iter()
            .any(|(_, routes, _)| routes.contains(&route))
    }
    pub(crate) fn acknowledge(&mut self, checkpoint: i32) -> Vec<T> {
        self.released
            .retain(|(_, window)| window.after != checkpoint);
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.deferred.len() {
            if self.deferred[index].0 == checkpoint {
                ready.push(self.deferred.swap_remove(index).2);
            } else {
                index += 1;
            }
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn release_evidence_is_bound_to_the_destroy_request_sequence() {
        let mut owner = ProxyReleases::<()>::default();
        owner.expect_destroy(
            5,
            RequestWindow {
                before: 44,
                after: 46,
            },
        );
        assert!(owner.was_released(5, 45));
        assert!(
            !owner.was_released(5, 47),
            "a reused id cannot authorize a new request"
        );
        assert!(!owner.was_released(6, 45));
        owner.acknowledge(46);
        assert!(!owner.was_released(5, 45));
    }
    #[test]
    fn request_windows_handle_sequence_wrap_and_reject_invalid_sequences() {
        let window = RequestWindow {
            before: 0x3fff_fffe,
            after: 1,
        };
        assert!(window.contains(0x3fff_ffff));
        assert!(window.contains(0));
        assert!(!window.contains(1));
        assert!(!window.contains(-1));
        assert!(!window.contains(i32::MAX));
    }
    #[test]
    fn deferred_cleanup_blocks_only_its_own_routes_and_drops_after_the_barrier() {
        struct Resource(Rc<Cell<bool>>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let dropped = Rc::new(Cell::new(false));
        let mut owner = ProxyReleases::default();
        owner.defer(23, vec![51], Resource(dropped.clone()));
        owner.defer(24, vec![], Resource(Rc::new(Cell::new(false))));
        assert!(owner.is_deferred(51));
        assert!(!owner.is_deferred(52));
        drop(owner.acknowledge(22));
        assert!(!dropped.get());
        let ready = owner.acknowledge(23);
        assert!(!owner.is_deferred(51));
        assert!(!dropped.get());
        drop(ready);
        assert!(dropped.get());
    }
    #[test]
    fn deselected_routes_release_after_barrier_without_borrowing_route_map() {
        struct Resource {
            routes: std::rc::Weak<RefCell<HashMap<u32, Resource>>>,
            dropped: Rc<Cell<usize>>,
        }
        impl Drop for Resource {
            fn drop(&mut self) {
                if let Some(routes) = self.routes.upgrade() {
                    assert!(
                        routes.try_borrow_mut().is_ok(),
                        "native destruction cannot run under the routes borrow"
                    );
                }
                self.dropped.set(self.dropped.get() + 1);
            }
        }
        let dropped = Rc::new(Cell::new(0));
        let routes = Rc::new(RefCell::new(HashMap::new()));
        for id in [1, 2] {
            routes.borrow_mut().insert(
                id,
                Resource {
                    routes: Rc::downgrade(&routes),
                    dropped: dropped.clone(),
                },
            );
        }
        let obsolete = take_unselected(&routes, |id| id == 2);
        assert_eq!(routes.borrow().len(), 1);
        assert_eq!(dropped.get(), 0);
        let mut owner = ProxyReleases::default();
        owner.defer(23, vec![1], obsolete);
        assert!(owner.is_deferred(1));
        assert!(!owner.is_deferred(2));
        drop(owner.acknowledge(23));
        assert_eq!(dropped.get(), 1);
    }
}
