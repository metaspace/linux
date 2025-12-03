// SPDX-License-Identifier: GPL-2.0

use kernel::prelude::*;

/// A buffer for preallocating XArray nodes.
///
/// This structure allows preallocating memory for XArray insertions to avoid
/// allocation failures during operations where allocation is not desirable.
pub struct XArrayPreloadBuffer {
    nodes: KVec<*mut bindings::xa_node>,
    size: usize,
    head: usize,
    tail: usize,
}

impl XArrayPreloadBuffer {
    /// Creates a new preload buffer with capacity for the given number of leaf values.
    ///
    /// Inserting a leaf value into an [`XArray`] may require allocating a
    /// number of internal nodes. This buffer will calculate the upper limit of
    /// required internal nodes for inserting `entry_count` leaf values and use
    /// that to size the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::XArrayPreloadBuffer};
    /// let buffer = XArrayPreloadBuffer::new(10)?;
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    /// [`XArray`]: super::XArray
    pub fn new(entry_count: usize) -> Result<Self> {
        let node_count = entry_count
            * ((usize::BITS as usize / bindings::XA_CHUNK_SHIFT)
                + if (usize::BITS as usize % bindings::XA_CHUNK_SHIFT) == 0 {
                    0
                } else {
                    1
                });

        let mut this = Self {
            nodes: KVec::new(),
            size: node_count + 1,
            head: 0,
            tail: 0,
        };

        for _ in 0..this.size {
            this.nodes.push(core::ptr::null_mut(), GFP_KERNEL)?;
        }

        Ok(this)
    }

    /// Allocates internal nodes until the buffer is full.
    pub fn preload(&mut self, flags: kernel::alloc::Flags) -> Result {
        while !self.full() {
            self.alloc(flags)?
        }
        Ok(())
    }

    /// Fills the buffer with preallocated nodes from the given vector.
    ///
    /// Nodes are moved from the vector into the buffer until the buffer is full
    /// or the vector is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{XArrayPreloadBuffer, XArrayPreloadNode}};
    /// let mut buffer = XArrayPreloadBuffer::new(5)?;
    /// let mut nodes = KVec::new();
    /// nodes.push(XArrayPreloadNode::new(GFP_KERNEL)?, GFP_KERNEL)?;
    /// buffer.preload_with(&mut nodes)?;
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn preload_with(&mut self, nodes: &mut KVec<XArrayPreloadNode>) -> Result {
        while !self.full() {
            if let Some(node) = nodes.pop() {
                self.push(node)?
            } else {
                break;
            }
        }

        Ok(())
    }

    /// Returns `true` if the buffer is full and cannot accept more nodes.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{XArrayPreloadBuffer, XArrayPreloadNode}};
    /// let mut buffer = XArrayPreloadBuffer::new(1)?;
    /// if !buffer.full() {
    ///     let count = buffer.free_count();
    ///     let mut nodes = KVec::new();
    ///     for _ in 0..count {
    ///         nodes.push(XArrayPreloadNode::new(GFP_KERNEL)?, GFP_KERNEL)?;
    ///     }
    ///     buffer.preload_with(&mut nodes)?;
    /// }
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn full(&self) -> bool {
        (self.head + 1) % self.size != self.tail
    }

    fn empty(&self) -> bool {
        self.head == self.tail
    }

    /// Returns the number of available slots in the buffer.
    pub fn free_count(&self) -> usize {
        (if self.head >= self.tail {
            self.size - (self.head - self.tail)
        } else {
            (self.size - self.tail) + self.head
        } - 1)
    }

    fn alloc(&mut self, flags: kernel::alloc::Flags) -> Result {
        if self.full() {
            return Err(ENOSPC);
        }

        self.push(XArrayPreloadNode::new(flags)?)?;

        Ok(())
    }

    fn push(&mut self, node: XArrayPreloadNode) -> Result {
        if self.full() {
            return Err(ENOSPC);
        }

        self.nodes[self.head] = node.into_raw();
        self.head = (self.head + 1) % self.size;

        Ok(())
    }

    /// Removes and returns one preallocated node from the buffer.
    ///
    /// Returns `None` if the buffer is empty.
    pub(crate) fn take_one(&mut self) -> Option<XArrayPreloadNode> {
        if self.empty() {
            return None;
        }

        let node = self.nodes[self.tail];
        self.tail = (self.tail + 1) % self.size;

        Some(XArrayPreloadNode(node))
    }
}

impl Drop for XArrayPreloadBuffer {
    fn drop(&mut self) {
        while !self.empty() {
            drop(self.take_one().expect("Not empty"));
        }
    }
}

/// A preallocated XArray node.
///
/// This represents a single preallocated internal node for an XArray.
/// Nodes can be stored in an [`XArrayPreloadBuffer`] for later use.
pub struct XArrayPreloadNode(*mut bindings::xa_node);

impl XArrayPreloadNode {
    /// Allocates a new XArray node.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::XArrayPreloadNode};
    /// let node = XArrayPreloadNode::new(GFP_KERNEL)?;
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn new(flags: kernel::alloc::Flags) -> Result<Self> {
        // SAFETY: `radix_tree_node_cachep` is a valid kmem cache for XArray nodes.
        let ptr = unsafe {
            bindings::kmem_cache_alloc_noprof(bindings::radix_tree_node_cachep, flags.as_raw())
        };

        if ptr.is_null() {
            return Err(ENOMEM);
        }

        // SAFETY: `ptr` is non-null and was allocated from `radix_tree_node_cachep`.
        Ok(unsafe { XArrayPreloadNode::from_raw(ptr.cast()) })
    }

    pub(crate) fn into_raw(self) -> *mut bindings::xa_node {
        self.0
    }

    /// Creates an `XArrayPreloadNode` from a raw pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid pointer to an XArray node allocated from `radix_tree_node_cachep`.
    pub(crate) unsafe fn from_raw(ptr: *mut bindings::xa_node) -> Self {
        Self(ptr)
    }
}

impl Drop for XArrayPreloadNode {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid pointer allocated from `radix_tree_node_cachep`.
        unsafe { bindings::kmem_cache_free(bindings::radix_tree_node_cachep, self.0.cast()) }
    }
}
