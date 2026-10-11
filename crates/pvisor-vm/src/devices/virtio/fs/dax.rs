//! Read-only DAX mappings into a caller-owned, permanently allocated window.
//! All replacements leave readable/writable memory at the same addresses. File
//! pages use private COW: even a hostile guest writing physical window addresses
//! cannot modify an immutable host file. Normal authorized reads share its pages.
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Mutex;

use super::fuse::{RemovemappingOne, SetupmappingFlags};
use crate::devices::virtio::VirtioShmRegion;

pub(super) struct ReadOnlyDax {
    region: VirtioShmRegion,
    page: u64,
    // Window offset -> exclusive end. Serialized across setup/remove; mappings
    // outlive OPEN handles, as the mmap itself pins the native file object.
    mappings: Mutex<BTreeMap<u64, u64>>,
}

fn invalid() -> io::Error {
    io::Error::from_raw_os_error(libc::EINVAL)
}

impl ReadOnlyDax {
    pub(super) fn new(region: VirtioShmRegion) -> io::Result<Self> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page <= 0 {
            return Err(invalid());
        }
        let page = page as u64;
        if region.size == 0
            || !region.host_addr.is_multiple_of(page)
            || !(region.size as u64).is_multiple_of(page)
            || region.host_addr.checked_add(region.size as u64).is_none()
        {
            return Err(invalid());
        }
        Ok(Self {
            region,
            page,
            mappings: Mutex::new(BTreeMap::new()),
        })
    }

    fn range(&self, offset: u64, len: u64, base: u64, size: u64) -> io::Result<u64> {
        if base != self.region.host_addr
            || size != self.region.size as u64
            || len == 0
            || !offset.is_multiple_of(self.page)
            || !len.is_multiple_of(self.page)
        {
            return Err(invalid());
        }
        offset
            .checked_add(len)
            .filter(|end| *end <= size)
            .ok_or_else(invalid)
    }

    fn replace(&self, offset: u64, len: u64, file: Option<(&File, u64)>) -> io::Result<()> {
        let (fd, foffset, flags) = match file {
            Some((file, off)) => (
                file.as_raw_fd(),
                off as libc::off_t,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
            ),
            None => (
                -1,
                0,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            ),
        };
        // range() has checked address arithmetic/alignment and the window is
        // owned by GuestMemory for the entire worker lifetime. MAP_PRIVATE is
        // essential: a guest's physical writes must never mutate backing bytes.
        let result = unsafe {
            libc::mmap(
                (self.region.host_addr + offset) as *mut libc::c_void,
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                foffset,
            )
        };
        if result == libc::MAP_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn setup(
        &self,
        file: &File,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        base: u64,
        size: u64,
    ) -> io::Result<()> {
        if flags & SetupmappingFlags::WRITE.bits() != 0 {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        if flags != SetupmappingFlags::READ.bits()
            || !foffset.is_multiple_of(self.page)
            || foffset
                .checked_add(len)
                .filter(|end| *end <= i64::MAX as u64)
                .is_none()
        {
            return Err(invalid());
        }
        let end = self.range(moffset, len, base, size)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || foffset >= metadata.len() {
            return Err(invalid());
        }
        let available = (metadata.len() - foffset).min(len);
        let file_len =
            available.checked_add(self.page - 1).ok_or_else(invalid)? / self.page * self.page;
        let mut mappings = self.mappings.lock().unwrap();
        if mappings
            .range(..end)
            .next_back()
            .is_some_and(|(_, old_end)| *old_end > moffset)
        {
            return Err(invalid());
        }
        // Never leave file-backed pages wholly beyond EOF: they would SIGBUS on
        // access. The last partial file page is kernel-zero-filled; subsequent
        // pages are anonymous zero padding, including malicious physical reads.
        if file_len < len {
            self.replace(moffset + file_len, len - file_len, None)?;
        }
        self.replace(moffset, file_len, Some((file, foffset)))?;
        mappings.insert(moffset, end);
        Ok(())
    }

    pub(super) fn remove(
        &self,
        requests: &[RemovemappingOne],
        base: u64,
        size: u64,
    ) -> io::Result<()> {
        // Validate the entire request before modifying a single mapping. On a
        // later mmap syscall failure earlier removals remain removed, never
        // leaving a stale file alias represented as free in the registry.
        for req in requests {
            self.range(req.moffset, req.len, base, size)?;
        }
        let mut mappings = self.mappings.lock().unwrap();
        for req in requests {
            let end = req.moffset + req.len;
            self.replace(req.moffset, req.len, None)?;
            let affected: Vec<_> = mappings
                .range(..end)
                .filter(|(_, old_end)| **old_end > req.moffset)
                .map(|(start, end)| (*start, *end))
                .collect();
            for (start, old_end) in affected {
                mappings.remove(&start);
                if start < req.moffset {
                    mappings.insert(start, req.moffset);
                }
                if old_end > end {
                    mappings.insert(end, old_end);
                }
            }
        }
        Ok(())
    }

    pub(super) fn clear(&self) -> io::Result<()> {
        self.remove(
            &[RemovemappingOne {
                moffset: 0,
                len: self.region.size as u64,
            }],
            self.region.host_addr,
            self.region.size as u64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

    #[test]
    fn private_mapping_zero_pads_eof_and_cannot_modify_backing() {
        let memory = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 4 * 4096)]).unwrap();
        let region = VirtioShmRegion {
            host_addr: memory.get_host_address(GuestAddress(0)).unwrap() as u64,
            guest_addr: 0,
            size: 4 * 4096,
        };
        let dax = ReadOnlyDax::new(region.clone()).unwrap();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&vec![0x5a; 4103]).unwrap();
        dax.setup(
            &file,
            0,
            region.size as u64,
            2,
            0,
            region.host_addr,
            region.size as u64,
        )
        .unwrap();
        let mut bytes = vec![0; region.size];
        memory.read_slice(&mut bytes, GuestAddress(0)).unwrap();
        assert!(bytes[..4103].iter().all(|b| *b == 0x5a));
        assert!(bytes[4103..].iter().all(|b| *b == 0));
        memory.write_obj(0x11_u8, GuestAddress(0)).unwrap();
        use std::os::unix::fs::FileExt;
        let mut byte = [0];
        file.read_exact_at(&mut byte, 0).unwrap();
        assert_eq!(byte, [0x5a]);
        dax.remove(
            &[RemovemappingOne {
                moffset: 0,
                len: region.size as u64,
            }],
            region.host_addr,
            region.size as u64,
        )
        .unwrap();
        memory.read_slice(&mut bytes, GuestAddress(0)).unwrap();
        assert!(bytes.iter().all(|b| *b == 0));
        // Removed slots remain valid writable guest memory and can be reused.
        memory.write_obj(0x22_u8, GuestAddress(0)).unwrap();
        dax.setup(&file, 0, 4096, 2, 0, region.host_addr, region.size as u64)
            .unwrap();
        assert_eq!(unsafe { *(region.host_addr as *const u8) }, 0x5a);
    }

    #[test]
    fn rejects_overflow_alignment_write_overlap_and_invalid_remove_atomically() {
        let memory = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 4 * 4096)]).unwrap();
        let region = VirtioShmRegion {
            host_addr: memory.get_host_address(GuestAddress(0)).unwrap() as u64,
            guest_addr: 0,
            size: 4 * 4096,
        };
        let dax = ReadOnlyDax::new(region.clone()).unwrap();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&vec![0x5a; 4 * 4096]).unwrap();
        for (foffset, len, flags, moffset) in [
            (0, 4096, 1, 0),
            (0, 4096, 3, 0),
            (0, 4096, 0, 0),
            (1, 4096, 2, 0),
            (0, 1, 2, 0),
            (0, 0, 2, 0),
            (0, 4096, 2, 1),
            (0, 4096, 2, u64::MAX - 4095),
            (u64::MAX - 4095, 4096, 2, 0),
        ] {
            assert!(dax
                .setup(
                    &file,
                    foffset,
                    len,
                    flags,
                    moffset,
                    region.host_addr,
                    region.size as u64
                )
                .is_err());
        }
        dax.setup(&file, 0, 12288, 2, 0, region.host_addr, region.size as u64)
            .unwrap();
        assert!(dax
            .setup(
                &file,
                0,
                4096,
                2,
                4096,
                region.host_addr,
                region.size as u64
            )
            .is_err());
        assert!(dax
            .remove(
                &[
                    RemovemappingOne {
                        moffset: 0,
                        len: 4096
                    },
                    RemovemappingOne {
                        moffset: region.size as u64,
                        len: 4096
                    }
                ],
                region.host_addr,
                region.size as u64
            )
            .is_err());
        assert_eq!(unsafe { *(region.host_addr as *const u8) }, 0x5a);
        dax.remove(
            &[RemovemappingOne {
                moffset: 4096,
                len: 4096,
            }],
            region.host_addr,
            region.size as u64,
        )
        .unwrap();
        dax.setup(
            &file,
            0,
            4096,
            2,
            4096,
            region.host_addr,
            region.size as u64,
        )
        .unwrap();
        assert!(dax
            .setup(
                &file,
                0,
                4096,
                2,
                8192,
                region.host_addr,
                region.size as u64
            )
            .is_err());
    }
}
