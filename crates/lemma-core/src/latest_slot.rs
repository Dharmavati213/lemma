use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// Single-value mailbox: `put` replaces any unconsumed value (latest wins).
///
/// Used where only the newest request matters, e.g. render requests for the PDF thread.
/// A panic while the lock is held does not poison the slot: workers are restarted after a
/// caught panic and keep using it.
#[derive(Debug)]
pub struct LatestSlot<T> {
    state: Mutex<SlotState<T>>,
    ready: Condvar,
}

#[derive(Debug)]
struct SlotState<T> {
    value: Option<T>,
    closed: bool,
}

impl<T> LatestSlot<T> {
    /// An empty, open slot.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SlotState {
                value: None,
                closed: false,
            }),
            ready: Condvar::new(),
        })
    }

    /// Stores `v`, dropping any value not yet taken. A put after [`close`](Self::close) is ignored.
    pub fn put(&self, v: T) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        state.value = Some(v);
        drop(state);
        self.ready.notify_one();
    }

    /// Blocks until a value is available; `None` once closed and empty.
    ///
    /// A value put before `close` is still delivered.
    pub fn take_blocking(&self) -> Option<T> {
        let state = self.lock();
        let mut state = self
            .ready
            .wait_while(state, |s| s.value.is_none() && !s.closed)
            .unwrap_or_else(PoisonError::into_inner);
        state.value.take()
    }

    /// Takes the value if there is one, without blocking.
    pub fn try_take(&self) -> Option<T> {
        self.lock().value.take()
    }

    /// Closes the slot and wakes every blocked [`take_blocking`](Self::take_blocking).
    pub fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, SlotState<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn latest_value_wins() {
        let slot = LatestSlot::new();
        assert_eq!(slot.try_take(), None);
        slot.put(1);
        slot.put(2);
        slot.put(3);
        assert_eq!(slot.try_take(), Some(3));
        assert_eq!(slot.try_take(), None);
        slot.put(4);
        assert_eq!(slot.take_blocking(), Some(4));
    }

    #[test]
    fn take_blocking_waits_for_put() {
        let slot = LatestSlot::new();
        let worker = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || slot.take_blocking())
        };
        thread::sleep(Duration::from_millis(20));
        slot.put("hello");
        assert_eq!(worker.join().unwrap(), Some("hello"));
    }

    #[test]
    fn close_unblocks_take_blocking() {
        let slot = LatestSlot::<u32>::new();
        let (tx, rx) = mpsc::channel();
        let worker = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || tx.send(slot.take_blocking()).unwrap())
        };
        // Still blocked while the slot is open and empty.
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        slot.close();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(None));
        worker.join().unwrap();
    }

    #[test]
    fn value_put_before_close_is_still_delivered() {
        let slot = LatestSlot::new();
        slot.put(7);
        slot.close();
        slot.put(8);
        assert_eq!(slot.take_blocking(), Some(7));
        assert_eq!(slot.take_blocking(), None);
    }

    #[test]
    fn survives_a_panicking_holder() {
        let slot = LatestSlot::new();
        let poisoner = Arc::clone(&slot);
        let _ = thread::spawn(move || {
            let _guard = poisoner.lock();
            panic!("worker crashed while holding the lock");
        })
        .join();
        slot.put(1);
        assert_eq!(slot.try_take(), Some(1));
    }
}
