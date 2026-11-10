//! A value one thread publishes whole and others read: the snapshots the
//! refresh thread builds, the sessions the sessions thread finds.
//!
//! It is swapped under its lock and read by taking the `Arc` and letting go
//! at once, so a paint never holds the lock while it works, and a publish
//! never waits on a paint or copies anything under it.
//!
//! A poisoned lock is taken anyway: what it guards is plain data, replaced
//! whole, with no invariant a panic could have broken half-way, and refusing
//! it would freeze the tray on whatever was last published.

use std::sync::{Arc, Mutex, PoisonError};

/// The latest of a `T`, shared between threads.
pub type Latest<T> = Arc<Mutex<Arc<T>>>;

/// A `Latest` holding `value`.
pub fn new<T>(value: T) -> Latest<T> {
    Arc::new(Mutex::new(Arc::new(value)))
}

/// What it holds now.
pub fn get<T>(latest: &Mutex<Arc<T>>) -> Arc<T> {
    Arc::clone(&latest.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Replace what it holds.
pub fn set<T>(latest: &Mutex<Arc<T>>, value: T) {
    *latest.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(value);
}

/// Change what it holds, as one swap: a reader that already took it keeps
/// what it took.
pub fn edit<T: Clone>(latest: &Mutex<Arc<T>>, change: impl FnOnce(&mut T)) {
    let mut guard = latest.lock().unwrap_or_else(PoisonError::into_inner);
    change(Arc::make_mut(&mut guard));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reader_keeps_what_it_took_while_a_change_swaps_in_a_new_one() {
        let latest = new(vec!["a", "b"]);
        let before = get(&latest);
        edit(&latest, |names| names.retain(|name| *name != "a"));
        assert_eq!(*before, ["a", "b"], "the reader's copy is untouched");
        assert_eq!(*get(&latest), ["b"], "the next reader sees the change");
        set(&latest, vec!["c"]);
        assert_eq!(*get(&latest), ["c"]);
        assert_eq!(*before, ["a", "b"]);
    }

    #[test]
    fn a_poisoned_lock_still_gives_what_was_published() {
        let latest = new(1);
        let poisoner = Arc::clone(&latest);
        std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison it");
        })
        .join()
        .unwrap_err();
        assert!(latest.is_poisoned());
        assert_eq!(*get(&latest), 1);
        set(&latest, 2);
        edit(&latest, |n| *n += 1);
        assert_eq!(*get(&latest), 3);
    }
}
