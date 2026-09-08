//! Byte scratch buffers that are safe to cast to Win32 structures.
//!
//! Several Win32 APIs write variable-length arrays of structures into a
//! caller-provided buffer and are then addressed as `*const T`
//! (`EnumServicesStatusExW`, `QueryServiceConfigW`, `QueryServiceConfig2W`,
//! `QueryServiceStatusEx`, `PdhGetFormattedCounterArrayW`,
//! `GetLogicalProcessorInformationEx`, `GetAdaptersAddresses`). Rust requires
//! the pointer used for such a read to carry `T`'s alignment, but `Vec<u8>`
//! only promises alignment 1: casting its pointer to a structure type is
//! undefined behavior even when the allocator happens to return aligned
//! memory today. `net_info` already worked around this with a `Vec<u64>`;
//! this type generalizes the same idea so every site uses one reviewed
//! implementation.
//!
//! `u64` words cover every structure used here (pointer and 64-bit field
//! alignment, 8 bytes on all supported targets). The byte length is tracked
//! separately from the rounded allocation so callers can still pass exact
//! buffer sizes to the Win32 API.

/// An 8-byte-aligned byte buffer with a logical byte length.
pub(super) struct AlignedBuf {
    words: Vec<u64>,
    len: usize,
}

impl AlignedBuf {
    /// Zeroed buffer of `len` bytes.
    pub(super) fn zeroed(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(std::mem::size_of::<u64>())],
            len,
        }
    }

    /// Resize to `len` bytes. Existing bytes are preserved up to `len` and
    /// new bytes are zeroed, matching `Vec::resize`.
    pub(super) fn resize(&mut self, len: usize) {
        self.words
            .resize(len.div_ceil(std::mem::size_of::<u64>()), 0);
        self.len = len;
    }

    /// Logical byte length, not the rounded allocation size.
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Base address, aligned for any structure used with this type.
    pub(super) fn as_ptr(&self) -> *const u8 {
        self.words.as_ptr().cast()
    }

    /// Mutable base address, aligned for any structure used with this type.
    pub(super) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.words.as_mut_ptr().cast()
    }

    /// The initialized bytes (exactly [`Self::len`]).
    pub(super) fn as_slice(&self) -> &[u8] {
        // SAFETY: `as_ptr` points at an allocation of at least `len` bytes
        // (the allocation is rounded up to whole words) and is non-null.
        unsafe { std::slice::from_raw_parts(self.as_ptr(), self.len) }
    }

    /// The initialized bytes mutably (exactly [`Self::len`]).
    pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: same allocation/length argument as `as_slice`, and `&mut
        // self` proves exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr(), self.len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_is_pointer_aligned_and_length_tracks_the_logical_size() {
        let mut buf = AlignedBuf::zeroed(3);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.as_slice(), &[0, 0, 0]);
        assert_eq!(
            buf.as_ptr() as usize % std::mem::align_of::<usize>(),
            0,
            "the base address must satisfy structure alignment"
        );

        buf.resize(24);
        assert_eq!(buf.len(), 24);
        assert_eq!(buf.as_slice().len(), 24);
        assert!(buf.as_slice().iter().all(|byte| *byte == 0));
        assert_eq!(buf.as_ptr() as usize % std::mem::align_of::<u64>(), 0);

        // Shrinking keeps the same (aligned) allocation and re-exposes only
        // the logical length.
        buf.resize(5);
        assert_eq!(buf.as_slice().len(), 5);
        assert_eq!(buf.as_mut_slice().len(), 5);
    }
}
