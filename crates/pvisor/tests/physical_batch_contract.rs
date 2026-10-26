//! Exercise the VM-owned batch contract through real host storage adapters.
#![cfg(target_os = "linux")]

use pvisor::ram_backing::{
    ipc::{PoolClient, serve_shared},
    resident::LocalColdRamStore,
    shared::SharedPool,
};
use pvisor_vm::api::{COLD_RAM_MAX_BATCH_PAGES, ColdRamStore};
use std::{
    io,
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, Mutex},
    time::Duration,
};

#[test]
fn physical_batch_preserves_per_page_admission_session_ownership_and_cow() {
    let pool = Arc::new(Mutex::new(SharedPool::new(4096, 1).unwrap()));
    let connect = || {
        let (client, server) = UnixStream::pair().unwrap();
        let owner = pool.clone();
        let thread = std::thread::spawn(move || serve_shared(server, owner, 8));
        let mut client = PoolClient::new(client, Duration::from_secs(3)).unwrap();
        client.enable_shared_mapping().unwrap();
        (client, thread)
    };
    let (mut a, ta) = connect();
    let (mut b, tb) = connect();
    let first = [0x73; 4096];
    let second = [0x74; 4096];
    for pages in [
        vec![],
        vec![&first[..]; COLD_RAM_MAX_BATCH_PAGES + 1],
        vec![&first[..4095]],
    ] {
        assert_eq!(
            a.put_batch(&pages).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    // One session's repeated observations cannot create cross-session authority.
    assert!(
        a.put_batch(&[&first, &first, &second])
            .unwrap()
            .iter()
            .all(Option::is_none)
    );
    assert!(a.put_batch(&[&first]).unwrap()[0].is_none());
    assert_eq!(ColdRamStore::stats(&mut a).unwrap().session_references, 0);
    let mut accepted = b.put_batch(&[&first, &second]).unwrap();
    assert!(
        accepted[1].is_none(),
        "one-slot pool must reject the second page without a reference"
    );
    let br = accepted[0].take().unwrap();
    let ar = a.put_batch(&[&first]).unwrap().remove(0).unwrap();
    assert!(
        ColdRamStore::restore(&mut a, &br, &mut [0; 4096]).is_err(),
        "references belong to their session"
    );
    let am = ColdRamStore::shared_mapping(&a, &ar).unwrap();
    let bm = ColdRamStore::shared_mapping(&b, &br).unwrap();
    assert_eq!(
        ColdRamStore::stats(&mut a).unwrap().cross_session_objects,
        1
    );
    assert_eq!(
        ColdRamStore::stats(&mut b).unwrap().cross_session_objects,
        1
    );
    assert_eq!(am.offset, bm.offset);
    let map = |mapping: &pvisor_vm::api::SharedRamMapping| unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            mapping.file.as_raw_fd(),
            mapping.offset as libc::off_t,
        )
    };
    let ap = map(&am);
    let bp = map(&bm);
    assert_ne!(ap, libc::MAP_FAILED);
    assert_ne!(bp, libc::MAP_FAILED);
    unsafe {
        *(ap as *mut u8) = 0x91;
        assert_eq!(*(bp as *const u8), 0x73);
        assert_eq!(libc::munmap(ap, 4096), 0);
        assert_eq!(libc::munmap(bp, 4096), 0);
    }
    ColdRamStore::release(&mut a, ar).unwrap();
    let mut restored = [0; 4096];
    ColdRamStore::restore(&mut b, &br, &mut restored).unwrap();
    assert_eq!(restored, first);
    ColdRamStore::release(&mut b, br).unwrap();
    let recovered = b.put_batch(&[&second]).unwrap().remove(0).unwrap();
    ColdRamStore::restore(&mut b, &recovered, &mut restored).unwrap();
    assert_eq!(restored, second);
    ColdRamStore::release(&mut b, recovered).unwrap();
    assert_eq!(ColdRamStore::stats(&mut b).unwrap().session_references, 0);
    drop(a);
    drop(b);
    ta.join().unwrap().unwrap();
    tb.join().unwrap().unwrap();
    assert_eq!(pool.lock().unwrap().object_count(), 0);
}

#[test]
fn unsupported_local_batch_does_not_change_existing_storage_references() {
    let mut store = LocalColdRamStore::new(1 << 20, 16);
    let object = store.put(&[0x55; 65536]).unwrap();
    let before = store.stats().unwrap();
    assert_eq!(
        store.put_batch(&[&[0x55; 4096]]).err().unwrap().kind(),
        io::ErrorKind::Unsupported
    );
    let after = store.stats().unwrap();
    assert_eq!(before.session_references, after.session_references);
    assert_eq!(before.objects, after.objects);
    let mut bytes = [0; 65536];
    store.restore(&object, &mut bytes).unwrap();
    assert_eq!(bytes, [0x55; 65536]);
    store.release(object).unwrap();
}
