//! The plugin's vector of `tm_external_message` (element 0x80 bytes)
//! as Update uses it: static begin/end/capacity pointers, emptied with
//! `end = begin` on every call, `push_back` with an out-of-line
//! reallocation, and the
//! exit-time destructor.

pub const ELEMENT_SIZE: usize = 0x80;
/// `max_size()`: `SIZE_MAX / 0x80`
pub const MAX_SIZE: usize = 0x01FF_FFFF_FFFF_FFFF;

/// An element count computed from two of the vector's
/// pointers: the pointer difference shifted right *arithmetically* by 7,
/// so a pointer below `first`
/// gives a count near `usize::MAX`.
pub const fn count(first: usize, last: usize) -> usize {
    ((last.wrapping_sub(first) as isize) >> 7) as usize
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowthFailure {
    /// length_error("vector too long"): size == max_size
    LengthError,
    /// new capacity above max_size
    BadArrayNewLength,
}

/// The new capacity for
/// inserting one element into a vector of `size` elements and `capacity`
/// (both from `count`). The arithmetic wraps.
pub const fn grow(size: usize, capacity: usize) -> Result<usize, GrowthFailure> {
    if size == MAX_SIZE {
        return Err(GrowthFailure::LengthError);
    }
    let new_size = size.wrapping_add(1);
    let new_cap = if MAX_SIZE.wrapping_sub(capacity >> 1) < capacity {
        new_size
    } else {
        let g = capacity.wrapping_add(capacity >> 1);
        if g < new_size {
            new_size
        } else {
            g
        }
    };
    if new_cap > MAX_SIZE {
        return Err(GrowthFailure::BadArrayNewLength);
    }
    Ok(new_cap)
}

#[cfg(windows)]
pub use self::win::MessageVector;

#[cfg(windows)]
mod win {
    use super::*;
    use crate::cxx_eh::{throw_bad_array_new_length, throw_length_error_vector_too_long};
    use crate::message::ExternalMessage;
    use crate::msvc_heap::{allocate, deallocate};
    use core::ptr::null_mut;

    /// begin / end / capacity end.
    #[repr(C)]
    pub struct MessageVector {
        first: *mut ExternalMessage,
        last: *mut ExternalMessage,
        end: *mut ExternalMessage,
    }

    impl MessageVector {
        pub const fn new() -> Self {
            MessageVector { first: null_mut(), last: null_mut(), end: null_mut() }
        }

        pub fn len(&self) -> usize {
            count(self.first as usize, self.last as usize)
        }

        fn capacity(&self) -> usize {
            count(self.first as usize, self.end as usize)
        }

        /// The elements (`[begin, end)`).
        pub fn as_slice(&self) -> &[ExternalMessage] {
            if self.first.is_null() {
                &[]
            } else {
                unsafe { core::slice::from_raw_parts(self.first, self.len()) }
            }
        }

        /// `clear()` as Update does it: end = begin, storage kept.
        pub fn clear(&mut self) {
            self.last = self.first;
        }

        /// `push_back`: copy into the
        /// spare slot, or reallocate when end == capacity end.
        pub unsafe fn push_back(&mut self, m: &ExternalMessage) {
            if self.last != self.end {
                *self.last = *m;
                self.last = self.last.add(1);
            } else {
                self.emplace_reallocate(m);
            }
        }

        /// Reallocation for an element at the end position: new
        /// block, new element, old elements copied in front of it, old block
        /// released (capacity bytes), pointers updated. A throw from the
        /// allocator leaves the vector untouched. Kept out of line,
        /// so it reads begin/end/capacity end from the vector
        /// itself when it is called.
        #[inline(never)]
        unsafe fn emplace_reallocate(&mut self, m: &ExternalMessage) {
            let size = self.len();
            let new_cap = match grow(size, self.capacity()) {
                Ok(c) => c,
                Err(GrowthFailure::LengthError) => throw_length_error_vector_too_long(),
                Err(GrowthFailure::BadArrayNewLength) => throw_bad_array_new_length(),
            };
            let block = allocate(new_cap * ELEMENT_SIZE) as *mut ExternalMessage;
            *block.add(size) = *m;
            if size != 0 {
                core::ptr::copy_nonoverlapping(self.first, block, size);
            }
            if !self.first.is_null() {
                deallocate(self.first as *mut u8, self.capacity() * ELEMENT_SIZE);
            }
            self.first = block;
            self.last = block.add(size + 1);
            self.end = block.add(new_cap);
        }

        /// Exit handler (also for the
        /// two other static vectors, which nothing ever fills).
        pub unsafe fn destroy(&mut self) {
            if !self.first.is_null() {
                deallocate(self.first as *mut u8, self.capacity() * ELEMENT_SIZE);
                self.first = null_mut();
                self.last = null_mut();
                self.end = null_mut();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_limits() {
        assert_eq!(grow(MAX_SIZE, MAX_SIZE), Err(GrowthFailure::LengthError));
        assert_eq!(grow(MAX_SIZE - 1, MAX_SIZE - 1), Ok(MAX_SIZE));
        assert_eq!(grow(10, MAX_SIZE - 1), Ok(11)); // 1.5x would overflow max_size
    }

    /// Counts are signed shifts of the pointer difference: a
    /// difference of 2^63 or more is negative, so `count` can never reach
    /// MAX_SIZE (MAX_SIZE << 7 has the top bit set) and the length_error
    /// branch is dead; end below begin gives a huge count and
    /// the bad_array_new_length branch.
    #[test]
    fn counts_are_arithmetic_shifts() {
        let b = 0x3000_0000usize;
        assert_eq!(count(b, b + 3 * 0x80), 3);
        assert_eq!(count(b, b + 0x7f), 0);
        assert_eq!(count(b, b - 0x80), usize::MAX);
        assert_eq!(count(b, b - 0x100), usize::MAX - 1);
        assert_eq!(count(0, MAX_SIZE << 7), usize::MAX);
        let max_positive = count(0, isize::MAX as usize);
        assert!(max_positive < MAX_SIZE);
        // end = cap = begin - 0x100: new size wraps to usize::MAX > max_size
        assert_eq!(grow(usize::MAX - 1, usize::MAX - 1), Err(GrowthFailure::BadArrayNewLength));
        // end = cap = begin - 0x80: new size wraps to 0, capacity 0 -> no block
        assert_eq!(grow(usize::MAX, usize::MAX), Ok(0));
    }
}
