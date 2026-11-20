//! A tiny word-based sequence lock for state that is written rarely and read on every pose update.
//!
//! All storage is atomic, so the implementation needs no unsafe code and has no data races; a
//! reader that overlaps a write retries. Writers serialise on a try-lock: a writer that loses
//! simply skips its update, which suits state that the next pose update will refresh anyway.
//!
//! The sequence number only ever increases (it never returns to an earlier value), so a reader
//! that overlaps any number of writes, including a `clear` followed by a `store`, always notices.
//! "Empty" is a flag written inside the same odd/even window as the value words.

use std::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

/// Values stored in a [`SeqCell`] encode themselves as `N` 64-bit words.
pub trait Words<const N: usize>: Copy {
    fn to_words(&self) -> [u64; N];
    fn from_words(words: &[u64; N]) -> Self;
}

pub struct SeqCell<const N: usize> {
    seq: AtomicU64,
    writer: AtomicBool,
    /// Whether a value is present; protected by `seq` like the value words.
    present: AtomicBool,
    words: [AtomicU64; N],
}

impl<const N: usize> Default for SeqCell<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> SeqCell<N> {
    pub const fn new() -> Self {
        SeqCell {
            seq: AtomicU64::new(0),
            writer: AtomicBool::new(false),
            present: AtomicBool::new(false),
            words: [const { AtomicU64::new(0) }; N],
        }
    }

    /// Returns `None` until the first write and after [`SeqCell::clear`].
    pub fn load<T: Words<N>>(&self) -> Option<T> {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let present = self.present.load(Ordering::Relaxed);
            let mut words = [0u64; N];
            for (dst, src) in words.iter_mut().zip(&self.words) {
                *dst = src.load(Ordering::Relaxed);
            }
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return present.then(|| T::from_words(&words));
            }
        }
    }

    fn lock(&self) -> bool {
        self.writer.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    /// Runs one odd/even write window. The caller holds the writer lock.
    fn write_locked(&self, words: Option<[u64; N]>) {
        let seq = self.seq.load(Ordering::Relaxed);
        // Odd = write in progress. The counter only grows (wrapping needs 2^63 writes).
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        match words {
            Some(words) => {
                for (dst, src) in self.words.iter().zip(words) {
                    dst.store(src, Ordering::Relaxed);
                }
                self.present.store(true, Ordering::Relaxed);
            }
            None => self.present.store(false, Ordering::Relaxed),
        }
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
        self.writer.store(false, Ordering::Release);
    }

    /// Publishes `value`; returns false (and writes nothing) if another writer is active.
    pub fn try_store<T: Words<N>>(&self, value: &T) -> bool {
        if !self.lock() {
            return false;
        }
        self.write_locked(Some(value.to_words()));
        true
    }

    /// Publishes `value`, spinning until any concurrent writer has finished.
    pub fn store<T: Words<N>>(&self, value: &T) {
        while !self.try_store(value) {
            std::hint::spin_loop();
        }
    }

    /// Forgets the stored value, so [`SeqCell::load`] returns `None` again.
    pub fn clear(&self) {
        while !self.lock() {
            std::hint::spin_loop();
        }
        self.write_locked(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Pair(u64, u64);

    impl Words<2> for Pair {
        fn to_words(&self) -> [u64; 2] {
            [self.0, self.1]
        }
        fn from_words(w: &[u64; 2]) -> Self {
            Pair(w[0], w[1])
        }
    }

    #[test]
    fn empty_then_value_then_cleared() {
        let cell = SeqCell::<2>::new();
        assert_eq!(cell.load::<Pair>(), None);
        cell.store(&Pair(1, 2));
        assert_eq!(cell.load::<Pair>(), Some(Pair(1, 2)));
        cell.clear();
        assert_eq!(cell.load::<Pair>(), None);
    }

    #[test]
    fn readers_never_see_torn_values() {
        let cell = Arc::new(SeqCell::<2>::new());
        cell.store(&Pair(0, 0));
        let writer = {
            let cell = cell.clone();
            std::thread::spawn(move || {
                for i in 1..20_000u64 {
                    cell.store(&Pair(i, !i));
                }
            })
        };
        for _ in 0..20_000 {
            let Pair(a, b) = cell.load::<Pair>().unwrap();
            assert!(a == 0 && b == 0 || b == !a, "torn read {a} {b}");
        }
        writer.join().unwrap();
    }

    /// Regression: `clear` used to reset the sequence to 0, so clear+store brought the sequence
    /// back to a value a slow reader had already sampled (ABA) and a torn read was accepted.
    #[test]
    fn readers_never_see_torn_values_across_clear_and_store() {
        let cell = Arc::new(SeqCell::<2>::new());
        cell.store(&Pair(0, 0));
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let cell = cell.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut seen = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        if let Some(Pair(a, b)) = cell.load::<Pair>() {
                            assert!(b == !a || (a == 0 && b == 0), "torn read {a} {b}");
                            seen += 1;
                        }
                    }
                    seen
                })
            })
            .collect();
        for i in 1..50_000u64 {
            cell.clear();
            cell.store(&Pair(i, !i));
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }
    }

    #[test]
    fn sequence_never_returns_to_an_earlier_value() {
        let cell = SeqCell::<2>::new();
        cell.store(&Pair(1, 2));
        let after_store = cell.seq.load(Ordering::Relaxed);
        cell.clear();
        cell.store(&Pair(3, 4));
        assert!(cell.seq.load(Ordering::Relaxed) > after_store);
        assert_eq!(cell.load::<Pair>(), Some(Pair(3, 4)));
    }
}
