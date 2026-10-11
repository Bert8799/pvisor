//! Real-guest DAX contract probe through the public API. Supply a prepared
//! Python rootfs, firmware file and an immutable directory containing payload.
use pvisor_vm::api::{OverlayConfig, PermissionSemantics, VmBuilder, VmConfiguration, VmRuntime};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err("expected ROOTFS FIRMWARE IMMUTABLE_DIRECTORY".into());
    }
    // Keep the prepared rootfs unchanged while the supervisor initializes /dev
    // and /proc. This writable root is separate from the immutable DAX share.
    let stage = tempfile::tempdir()?;
    let upper = stage.path().join("upper");
    let work = stage.path().join("work");
    std::fs::create_dir(&upper)?;
    std::fs::create_dir(&work)?;
    let mut vm = VmBuilder::new(2, 512)?;
    vm.set_firmware_path(PathBuf::from(&args[1]))?;
    vm.overlay(
        "/dev/root",
        OverlayConfig {
            lower_dirs: vec![PathBuf::from(&args[0])],
            lower_mutability: vec![],
            upper_dir: upper,
            work_dir: Some(work),
            preimage_dir: None,
            apply_target: None,
            baseline_lower: None,
            baseline_content_index: None,
            excluded_paths: vec![],
            access_policy: Default::default(),
            semantics: PermissionSemantics::LinuxComplete,
        },
        0,
    )?;
    // Deliberately smaller than payload: exercise window eviction/reuse too.
    vm.immutable_filesystem("pvisor-dax", &PathBuf::from(&args[2]), 2 << 20)?;
    vm.virtual_file(
        "pvisor-dax",
        "/virtual",
        b"virtual bytes".to_vec(),
        0o400,
        false,
    )?;
    let script = r#"
import concurrent.futures,ctypes,errno,mmap,os
c=ctypes.CDLL(None,use_errno=True)
def mount(src,dst,kind,flags,data):
    if c.mount(src,dst,kind,ctypes.c_ulong(flags),data)!=0: raise OSError(ctypes.get_errno(),dst)
mount(b'tmpfs',b'/tmp',b'tmpfs',0,None)
os.mkdir('/tmp/dax')
mount(b'pvisor-dax',b'/tmp/dax',b'virtiofs',1,b'dax=inode')
p=b'/tmp/dax/payload'
s=ctypes.create_string_buffer(256)
assert c.statx(-100,p,0,0x7ff,s)==0
assert int.from_bytes(s.raw[8:16],'little') & 0x200000, 'STATX_ATTR_DAX absent'
assert c.statx(-100,b'/tmp/dax/virtual',0,0x7ff,s)==0
assert not int.from_bytes(s.raw[8:16],'little') & 0x200000, 'virtual file marked DAX'
with open('/tmp/dax/virtual','rb') as v: assert v.read()==b'virtual bytes'
f=os.open(p,os.O_RDONLY)
size=os.fstat(f).st_size
assert size>3*(2<<20), 'payload must exceed the DAX window'
for off in range(0,size,65536):
    b=os.pread(f,min(65536,size-off),off)
    assert b==bytes((i%251 for i in range(off,off+len(b))))
def read_range(off):
    b=os.pread(f,min(65536,size-off),off)
    assert b==bytes((i%251 for i in range(off,off+len(b))))
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as workers:
    list(workers.map(read_range,range(0,size,65536)))
with mmap.mmap(f,0,access=mmap.ACCESS_COPY) as m:
    m[0]=0xff
    assert m[0]==0xff and os.pread(f,1,0)==b'\0'
with mmap.mmap(f,0,access=mmap.ACCESS_READ) as m:
    os.close(f)
    for off in range(0,size,4096): assert m[off]==off%251
    assert m[-1]==(size-1)%251
try:
    os.open(p,os.O_WRONLY)
    raise AssertionError('write accepted')
except OSError as e: assert e.errno==errno.EROFS
assert c.umount(b'/tmp/dax')==0
print('DAX_CASE_PASS bytes='+str(size),flush=True)
"#;
    let config = serde_json::json!({
        "argv": ["/usr/bin/python3", "-B", "-u", "-c", script],
        "env": {"PYTHONDONTWRITEBYTECODE": "1"},
        "cwd": "/"
    });
    vm.virtual_file(
        "/dev/root",
        "/.pvisor-guest.json",
        serde_json::to_vec(&config)?,
        0o400,
        true,
    )?;
    vm.run(|_| Ok(()))?;
    Ok(())
}
