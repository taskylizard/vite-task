mod broker;

use std::{io, slice};

use memmap2::{MmapOptions, MmapRaw};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, fstat, ftruncate, memfd_create};

use crate::ShmBroker;

/// An owned Linux shared-memory mapping.
pub struct Shm {
    id: String,
    mapping: MmapRaw,
}

/// A newly created shared-memory mapping and its broker.
pub struct CreatedShm {
    /// The owned shared-memory mapping.
    pub shm: Shm,
    /// The service that makes this mapping available to other processes.
    pub broker: ShmBroker,
}

/// Creates a sealed memfd mapping of `size` bytes.
///
/// # Errors
///
/// Returns an error if the memfd, mapping, or broker listener cannot be created.
pub fn create(size: usize) -> io::Result<CreatedShm> {
    let size_u64 = valid_size(size)?;
    let memfd = memfd_create("vite-task-fspy", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
        .map_err(io::Error::from)?;
    ftruncate(&memfd, size_u64).map_err(io::Error::from)?;
    fcntl_add_seals(&memfd, SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL)
        .map_err(io::Error::from)?;
    let mapping = MmapOptions::new().len(size).map_raw(&memfd)?;
    let (id, broker) = broker::new(memfd)?;

    Ok(CreatedShm { shm: Shm { id, mapping }, broker })
}

/// Opens the memfd mapping identified by `id` through its task broker.
///
/// # Errors
///
/// Returns an error if the identifier is invalid, the broker rejects the
/// request, or the received memfd has an unexpected size.
pub fn open(id: &str, size: usize) -> io::Result<Shm> {
    let size_u64 = valid_size(size)?;
    let memfd = broker::request_memfd(id)?;
    let stat = fstat(&memfd).map_err(io::Error::from)?;
    if u64::try_from(stat.st_size).ok() != Some(size_u64) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shared-memory size does not match broker descriptor",
        ));
    }
    let mapping = MmapOptions::new().len(size).map_raw(&memfd)?;
    Ok(Shm { id: id.to_owned(), mapping })
}

fn valid_size(size: usize) -> io::Result<u64> {
    if size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shared-memory size must be nonzero",
        ));
    }
    u64::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "shared-memory size exceeds u64"))
}

#[expect(clippy::len_without_is_empty, reason = "shared-memory mappings are always non-empty")]
impl Shm {
    /// Returns this mapping's opaque broker identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the mapped length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.mapping.len()
    }

    /// Returns a raw pointer to the first mapped byte.
    #[must_use]
    pub fn as_ptr(&self) -> *mut u8 {
        self.mapping.as_mut_ptr()
    }

    /// Returns the mapped bytes as a shared slice.
    ///
    /// # Safety
    ///
    /// The caller must ensure that no process or thread mutates the mapping for
    /// the lifetime of the returned slice.
    #[must_use]
    pub unsafe fn as_slice(&self) -> &[u8] {
        // SAFETY: The mapping is valid for its full length, and the caller
        // guarantees that it is not mutated while the slice is borrowed.
        unsafe { slice::from_raw_parts(self.mapping.as_ptr(), self.mapping.len()) }
    }
}
