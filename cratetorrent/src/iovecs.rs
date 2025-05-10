//! Zero-copy vectored I/O utilities for bounded, radiation-hardened systems.
//! Provides memory-safe, predictable vectored I/O with fault containment.

use std::slice;

/// A thin wrapper around a byte slice for vectored I/O.
#[derive(Debug)]
pub struct IoVec<T> {
    inner: T,
}

impl<'a> IoVec<&'a [u8]> {
    /// Wrap an immutable slice.
    pub fn from_slice(buf: &'a [u8]) -> Self {
        IoVec { inner: buf }
    }

    /// Access as an immutable slice.
    pub fn as_slice(&self) -> &'a [u8] {
        self.inner
    }
}

impl<'a> IoVec<&'a mut [u8]> {
    /// Wrap a mutable slice.
    pub fn from_mut_slice(buf: &'a mut [u8]) -> Self {
        IoVec { inner: buf }
    }

    /// Access as an immutable slice (tied to &self's borrow).
    pub fn as_slice(&self) -> &[u8] {
        self.inner
    }

    /// Access as a mutable slice (tied to &mut self's borrow).
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.inner
    }
}

/// Metadata about where a split occurred.
#[derive(Debug)]
struct Split<'slice> {
    /// Index of the buffer that was split (or boundary index).
    pos: usize,
    /// If splitting within a buffer, holds the second-half slice.
    second: Option<&'slice [u8]>,
}

/// A view into a mutable array of immutable `IoVec` slices,
/// potentially split at a byte boundary.
#[derive(Debug)]
pub struct IoVecs<'buf, 'slice> {
    bufs: &'buf mut [IoVec<&'slice [u8]>],
    split: Option<Split<'slice>>,
}

impl<'buf, 'slice> IoVecs<'buf, 'slice> {
    /// Create an I/O vector view bounded to at most `max_len` bytes.
    /// Splits the view if the total length exceeds `max_len`.
    /// Panics if `max_len == 0`.
    pub fn bounded(
        bufs: &'buf mut [IoVec<&'slice [u8]>],
        max_len: usize,
    ) -> Self {
        assert!(max_len > 0, "max_len must be > 0");

        let mut acc = 0;
        // find first buffer where cumulative length > max_len
        if let Some(i) = bufs.iter().position(|b| {
            acc += b.as_slice().len();
            acc > max_len
        }) {
            if acc == max_len {
                // split exactly at a buffer boundary
                if i + 1 == bufs.len() {
                    // no split needed
                    IoVecs::unbounded(bufs)
                } else {
                    IoVecs {
                        bufs,
                        split: Some(Split { pos: i, second: None }),
                    }
                }
            } else {
                // split within bufs[i]
                let prev = acc - bufs[i].as_slice().len();
                let cut = max_len - prev;
                let whole = bufs[i].as_slice();
                let first = &whole[..cut];
                let second = &whole[cut..];
                bufs[i] = IoVec::from_slice(first);
                IoVecs {
                    bufs,
                    split: Some(Split { pos: i, second: Some(second) }),
                }
            }
        } else {
            // fits entirely
            IoVecs::unbounded(bufs)
        }
    }

    /// Create an unbounded view (no split).
    pub fn unbounded(bufs: &'buf mut [IoVec<&'slice [u8]>]) -> Self {
        IoVecs { bufs, split: None }
    }

    /// Return the first segment (up to the split point), or all if unbounded.
    pub fn as_slice(&self) -> &[IoVec<&'slice [u8]>] {
        if let Some(Split { pos, .. }) = &self.split {
            &self.bufs[..=*pos]
        } else {
            &self.bufs[..]
        }
    }

    /// Advance the view by `n` bytes, dropping full buffers
    /// and shrinking the first remaining buffer if partially consumed.
    /// Panics if `n` exceeds the available length.
    pub fn advance(&mut self, n: usize) {
        if n == 0 {
            return;
        }

        let mut dropped = 0;
        let mut removed = 0;
        for buf in self.as_slice() {
            let len = buf.as_slice().len();
            if removed + len > n {
                break;
            }
            removed += len;
            dropped += 1;
        }

        // Drop consumed buffers
        let rem = std::mem::take(&mut self.bufs);
        self.bufs = &mut rem[dropped..];

        // Adjust split position
        if let Some(sp) = &mut self.split {
            if dropped > sp.pos {
                sp.pos = 0;
            } else {
                sp.pos -= dropped;
            }
        }

        // Shrink the next buffer if partially consumed
        let left = n - removed;
        if left > 0 && !self.bufs.is_empty() {
            let buf = self.bufs[0].as_slice();
            assert!(left <= buf.len(), "overflow advance");
            let new = &buf[left..];
            self.bufs[0] = IoVec::from_slice(new);
        }
    }

    /// Consume `self` and return the second half of the split (or empty if none).
    pub fn into_tail(mut self) -> &'buf mut [IoVec<&'slice [u8]>] {
        // Take split metadata first to release borrow on self
        let split = self.split.take();
        if let Some(Split { pos, second }) = split {
            if let Some(sec) = second {
                // Restore the second half into the buffer at `pos`
                self.bufs[pos] = IoVec::from_slice(sec);
            }
            &mut self.bufs[pos..]
        } else {
            // No split → return an empty tail
            let len = self.bufs.len();
            &mut self.bufs[len..]
        }
    }
}

/// A simpler advance helper for mutable buffers (e.g. read cursors).
pub fn advance<'a>(
    bufs: &'a mut [IoVec<&'a mut [u8]>],
    n: usize,
) -> &'a mut [IoVec<&'a mut [u8]>] {
    if n == 0 {
        return bufs;
    }

    let mut dropped = 0;
    let mut removed = 0;
    for b in bufs.iter() {
        let len = b.as_slice().len();
        if removed + len > n {
            break;
        }
        removed += len;
        dropped += 1;
    }

    let rest = &mut bufs[dropped..];
    let left = n - removed;
    if left > 0 && !rest.is_empty() {
        let slice = rest[0].as_mut_slice();
        assert!(left <= slice.len(), "overflow advance");
        // SAFETY: creating a subslice of the original `&mut [u8]`.
        let new = unsafe {
            slice::from_raw_parts_mut(slice.as_mut_ptr().add(left), slice.len() - left)
        };
        rest[0] = IoVec::from_mut_slice(new);
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_split_at_boundary() {
        let mut data = vec![[0u8; 16], [0u8; 16]];
        let mut bufs: Vec<_> = data.iter().map(|d| IoVec::from_slice(&d[..])).collect();
        let iov = IoVecs::bounded(&mut bufs, 16);
        assert_eq!(iov.as_slice().len(), 1);
        let tail = iov.into_tail();
        assert_eq!(tail.len(), 1);
    }

    #[test]
    fn bounded_split_within() {
        let mut data = vec![[1u8; 10], [2u8; 10]];
        let mut bufs: Vec<_> = data.iter().map(|d| IoVec::from_slice(&d[..])).collect();
        let iov = IoVecs::bounded(&mut bufs, 12);
        let first = iov.as_slice();
        assert_eq!(first.len(), 2);
        assert_eq!(first[1].as_slice().len(), 2);
        let tail = iov.into_tail();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].as_slice().len(), 8);
    }

    #[test]
    fn advance_partial() {
        let mut data = vec![[0u8; 5], [0u8; 5]];
        let mut bufs: Vec<_> = data.iter().map(|d| IoVec::from_slice(&d[..])).collect();
        let mut iov = IoVecs::bounded(&mut bufs, 10);
        iov.advance(3);
        assert_eq!(iov.as_slice()[0].as_slice().len(), 2);
    }

    #[test]
    fn advance_mutable() {
        let mut a = [0u8; 4];
        let mut b = [0u8; 4];
        let mut bufs = [
            IoVec::from_mut_slice(&mut a[..]),
            IoVec::from_mut_slice(&mut b[..]),
        ];
        let tail = advance(&mut bufs, 6);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].as_slice().len(), 2);
    }
}
