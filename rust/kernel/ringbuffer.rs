// SPDX-License-Identifier: GPL-2.0

//! A fixed-capacity FIFO ring buffer.
//!
//! This module provides [`RingBuffer`], a circular buffer implementation that
//! supports efficient push and pop operations at opposite ends of the buffer.

use kernel::prelude::*;

/// A fixed-capacity FIFO ring buffer.
///
/// `RingBuffer` is a circular buffer that allows pushing elements to the head
/// and popping elements from the tail in constant time. The buffer has a fixed
/// capacity specified at construction time and will return an error if a push
/// is attempted when full.
///
/// # Invariants
///
/// - `self.head` points at the next empty slot.
/// - `self.tail` points at the last full slot, except if the buffer is empty.
/// - The buffer is empty when `self.head == self.tail`.
/// - The buffer will always have at least one empty slot, even when full.
pub struct RingBuffer<T> {
    nodes: KVec<Option<T>>,
    size: usize,
    head: usize,
    tail: usize,
}

impl<T> RingBuffer<T> {
    /// Creates a new `RingBuffer` with the specified capacity.
    ///
    /// The buffer will be able to hold exactly `capacity` elements. Memory is
    /// allocated during construction.
    ///
    /// # Errors
    ///
    /// Returns an error if memory allocation fails.
    pub fn new(capacity: usize) -> Result<Self> {
        let mut this = Self {
            nodes: KVec::with_capacity(capacity + 1, GFP_KERNEL)?,
            size: capacity + 1,
            head: 0,
            tail: 0,
        };

        for _ in 0..this.size {
            this.nodes.push_within_capacity(None)?;
        }

        Ok(this)
    }

    /// Returns `true` if the buffer is full.
    ///
    /// When the buffer is full, any call to [`push_head`] will return an error.
    ///
    /// [`push_head`]: Self::push_head
    pub fn full(&self) -> bool {
        (self.head + 1) % self.size == self.tail
    }

    fn empty(&self) -> bool {
        self.head == self.tail
    }

    /// Returns the number of available slots in the buffer.
    ///
    /// This is the number of elements that can be pushed before the buffer
    /// becomes full.
    pub fn free_count(&self) -> usize {
        (if self.head >= self.tail {
            self.size - (self.head - self.tail)
        } else {
            (self.size - self.tail) + self.head
        } - 1)
    }

    /// Pushes a value to the head of the buffer.
    ///
    /// # Errors
    ///
    /// Returns [`ENOSPC`] if the buffer is full.
    pub fn push_head(&mut self, value: T) -> Result {
        if self.full() {
            return Err(ENOSPC);
        }

        self.nodes[self.head] = Some(value);
        self.head = (self.head + 1) % self.size;

        Ok(())
    }

    /// Pops and returns the value at the tail of the buffer.
    ///
    /// Returns `None` if the buffer is empty. Values are returned in FIFO
    /// order, i.e., the oldest value is returned first.
    pub fn pop_tail(&mut self) -> Option<T> {
        if self.empty() {
            return None;
        }

        let value = self.nodes[self.tail].take();
        self.tail = (self.tail + 1) % self.size;
        value
    }
}

impl<T> Drop for RingBuffer<T> {
    fn drop(&mut self) {
        while !self.empty() {
            drop(self.pop_tail().expect("Not empty"));
        }
    }
}

#[kunit_tests(rust_kernel_ringbuffer)]
mod tests {
    use super::*;

    #[test]
    fn test_new_buffer_is_empty() {
        let buffer: RingBuffer<i32> = RingBuffer::new(5).expect("Failed to create buffer");
        assert!(!buffer.full());
        assert!(buffer.empty());
    }

    #[test]
    fn test_new_buffer_has_correct_capacity() {
        let capacity = 10;
        let buffer: RingBuffer<i32> = RingBuffer::new(capacity).expect("Failed to create buffer");
        assert_eq!(buffer.free_count(), capacity);
    }

    #[test]
    fn test_push_single_element() {
        let mut buffer: RingBuffer<i32> = RingBuffer::new(5).expect("Failed to create buffer");
        assert!(buffer.push_head(42).is_ok());
        assert!(!buffer.empty());
        assert!(!buffer.full());
    }

    #[test]
    fn test_push_and_pop_single_element() {
        let mut buffer: RingBuffer<i32> = RingBuffer::new(5).expect("Failed to create buffer");
        buffer.push_head(42).expect("Failed to push");
        let value = buffer.pop_tail();
        assert_eq!(value, Some(42));
        assert!(buffer.empty());
    }

    #[test]
    fn test_push_and_pop_multiple_elements() {
        let mut buffer: RingBuffer<i32> = RingBuffer::new(5).expect("Failed to create buffer");

        for i in 0..5 {
            buffer.push_head(i).expect("Failed to push");
        }

        for i in 0..5 {
            assert_eq!(buffer.pop_tail(), Some(i));
        }

        assert!(buffer.empty());
    }

    #[test]
    fn test_pop_from_empty_buffer() {
        let mut buffer: RingBuffer<i32> = RingBuffer::new(5).expect("Failed to create buffer");
        assert_eq!(buffer.pop_tail(), None);
    }

    #[test]
    fn test_buffer_full() {
        let capacity = 3;
        let mut buffer: RingBuffer<i32> =
            RingBuffer::new(capacity).expect("Failed to create buffer");

        for i in 0..capacity {
            assert!(buffer.push_head(i as i32).is_ok());
        }

        assert!(buffer.full());
        assert_eq!(buffer.free_count(), 0);
    }

    #[test]
    fn test_push_to_full_buffer_fails() {
        let capacity = 3;
        let mut buffer: RingBuffer<i32> =
            RingBuffer::new(capacity).expect("Failed to create buffer");

        for i in 0..capacity {
            buffer.push_head(i as i32).expect("Failed to push");
        }

        assert!(buffer.full());
        assert!(buffer.push_head(999).is_err());
    }

    #[test]
    fn test_free_count_decreases_on_push() {
        let capacity = 5;
        let mut buffer: RingBuffer<i32> =
            RingBuffer::new(capacity).expect("Failed to create buffer");

        assert_eq!(buffer.free_count(), capacity);

        for i in 0..capacity {
            buffer.push_head(i as i32).expect("Failed to push");
            assert_eq!(buffer.free_count(), capacity - i - 1);
        }
    }

    #[test]
    fn test_free_count_increases_on_pop() {
        let capacity = 5;
        let mut buffer: RingBuffer<i32> =
            RingBuffer::new(capacity).expect("Failed to create buffer");

        for i in 0..capacity {
            buffer.push_head(i as i32).expect("Failed to push");
        }

        assert_eq!(buffer.free_count(), 0);

        for i in 1..=capacity {
            buffer.pop_tail();
            assert_eq!(buffer.free_count(), i);
        }
    }

    #[test]
    fn test_wrap_around_behavior() {
        let capacity = 3;
        let mut buffer: RingBuffer<i32> =
            RingBuffer::new(capacity).expect("Failed to create buffer");

        // Fill the buffer.
        for i in 0..capacity {
            buffer.push_head(i as i32).expect("Failed to push");
        }

        // Pop two elements.
        assert_eq!(buffer.pop_tail(), Some(0));
        assert_eq!(buffer.pop_tail(), Some(1));

        // Push two more (should wrap around).
        buffer.push_head(10).expect("Failed to push");
        buffer.push_head(11).expect("Failed to push");

        // Verify order.
        assert_eq!(buffer.pop_tail(), Some(2));
        assert_eq!(buffer.pop_tail(), Some(10));
        assert_eq!(buffer.pop_tail(), Some(11));
        assert!(buffer.empty());
    }

    #[test]
    fn test_fifo_order() {
        let mut buffer: RingBuffer<i32> = RingBuffer::new(10).expect("Failed to create buffer");

        let values = [1, 2, 3, 4, 5];
        for &value in &values {
            buffer.push_head(value).expect("Failed to push");
        }

        for &expected in &values {
            assert_eq!(buffer.pop_tail(), Some(expected));
        }
    }

    #[test]
    fn test_interleaved_push_pop() {
        let mut buffer: RingBuffer<i32> = RingBuffer::new(5).expect("Failed to create buffer");

        buffer.push_head(1).expect("Failed to push");
        buffer.push_head(2).expect("Failed to push");
        assert_eq!(buffer.pop_tail(), Some(1));

        buffer.push_head(3).expect("Failed to push");
        assert_eq!(buffer.pop_tail(), Some(2));
        assert_eq!(buffer.pop_tail(), Some(3));

        assert!(buffer.empty());
    }

    #[test]
    fn test_buffer_state_after_fill_and_drain() {
        let capacity = 4;
        let mut buffer: RingBuffer<i32> =
            RingBuffer::new(capacity).expect("Failed to create buffer");

        // Fill and drain twice.
        for _ in 0..2 {
            for i in 0..capacity {
                buffer.push_head(i as i32).expect("Failed to push");
            }
            assert!(buffer.full());

            for _ in 0..capacity {
                buffer.pop_tail();
            }
            assert!(buffer.empty());
        }
    }

    #[test]
    fn test_single_capacity_buffer() {
        let mut buffer: RingBuffer<i32> = RingBuffer::new(1).expect("Failed to create buffer");

        assert_eq!(buffer.free_count(), 1);
        buffer.push_head(42).expect("Failed to push");
        assert!(buffer.full());
        assert_eq!(buffer.free_count(), 0);

        assert_eq!(buffer.pop_tail(), Some(42));
        assert!(buffer.empty());
    }
}
