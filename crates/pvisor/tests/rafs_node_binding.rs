//! Transport handoff contracts with a fake peer; no native-format or KVM claim.
#![cfg(target_os = "linux")]
use pvisor::rafs::{RafsNodeBinding, RafsNodeRootfs};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
};

struct Peer(Child);
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const PEER: &str = r#"
import socket,sys,json,threading,hashlib,os
from pathlib import Path
root=Path(sys.argv[1]); listener=socket.socket(socket.AF_UNIX)
listener.bind(str(root/'node.sock'));os.chmod(root/'node.sock',0o600);listener.listen(8)
def frame(stream,value):
    data=json.dumps(value).encode();stream.sendall(len(data).to_bytes(4,'big')+data)
def exact(stream,n):
    data=b''
    while len(data)<n:
        block=stream.recv(n-len(data))
        if not block:raise EOFError()
        data+=block
    return data
def serve(stream):
    with stream:
        try:
            while True:
                request=json.loads(exact(stream,int.from_bytes(exact(stream,4),'big')))['request']
                if request['op']=='ping':frame(stream,{'status':'ready'});continue
                generation=(root/'generation').read_text()
                if request.get('generation') not in (None,generation):
                    reply={'status':'error','errno':116,'message':'stale node'}
                elif request['request']['op']=='open':
                    reply={'status':'ok','handle':request['handle'],'node_generation':generation,
                        'private_roots':[str(root/'store'),str(root/'work/control')],
                        'client_limits':{'metadata_entries':2,'directory_bytes':65536}}
                else:
                    reply={'status':'ok','node':{'kind':'directory','inode':1,'mode':493,
                        'uid':os.getuid(),'gid':os.getgid(),'size':0,'nlink':2,
                        'mtime':1,'mtime_nsec':0,'target':None,'xattrs':{}}}
                frame(stream,{'status':'rafs','frame':reply,'length':0,
                    'sha256':'sha256:'+hashlib.sha256(b'').hexdigest()})
        except (EOFError,ConnectionError):pass
print('ready',flush=True)
while True:
    stream,_=listener.accept();threading.Thread(target=serve,args=(stream,),daemon=True).start()
"#;

#[test]
fn admission_binding_fences_service_and_physical_artifact_and_view_owns_lease() {
    let temp = tempfile::Builder::new()
        .prefix("rafsh-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let store = root.join("store");
    let work = root.join("work");
    for directory in [&store, &store.join("merged"), &work] {
        fs::create_dir(directory).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::create_dir(work.join("control")).unwrap();
    fs::set_permissions(work.join("control"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.join("raw"), b"fixed merged environment").unwrap();
    let revision = pvisor::environment_snapshot::file_hash(&root.join("raw")).unwrap();
    let bundle = store.join("merged").join(&revision);
    fs::create_dir(&bundle).unwrap();
    fs::set_permissions(&bundle, fs::Permissions::from_mode(0o700)).unwrap();
    let commit = bundle.join("MERGED.json");
    fs::rename(root.join("raw"), &commit).unwrap();
    fs::set_permissions(&commit, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        root.join("generation"),
        "12345678-1234-4234-8234-123456789abc",
    )
    .unwrap();
    let mut peer = Peer(
        Command::new("python3")
            .args(["-c", PEER])
            .arg(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(peer.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    let source = RafsNodeRootfs {
        store,
        handle: format!("pvisor-rafs-merged-v1:{revision}"),
        endpoint: format!("unix://{}", root.join("node.sock").display()),
    };
    let admission = source.bind().unwrap();
    let binding: RafsNodeBinding =
        serde_json::from_slice(&serde_json::to_vec(&admission.binding()).unwrap()).unwrap();
    assert_eq!(binding.source(), &source);
    let recipient = binding.acquire().unwrap();
    assert!(recipient.prepare(&work.join("control")).is_err());
    assert!(fs::read_dir(work.join("control")).unwrap().next().is_none());
    let exclusive = File::open(&commit).unwrap();
    drop(admission);
    assert!(fs2::FileExt::try_lock_exclusive(&exclusive).is_err());
    let view = recipient.prepare(&work).unwrap();
    let path = view.root().to_owned();
    drop(recipient);
    assert!(fs2::FileExt::try_lock_exclusive(&exclusive).is_err());
    fs::write(
        root.join("generation"),
        "22345678-1234-4234-8234-123456789abc",
    )
    .unwrap();
    assert!(binding.acquire().is_err());
    // New admission after a restart is permitted, but old handoffs stay fenced.
    let new = source.bind().unwrap();
    let new_binding = new.binding();
    drop(new);
    fs::rename(&commit, bundle.join("old")).unwrap();
    fs::write(&commit, fs::read(bundle.join("old")).unwrap()).unwrap();
    fs::set_permissions(&commit, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(new_binding.acquire().is_err());
    drop(view);
    assert!(!path.exists());
    fs2::FileExt::try_lock_exclusive(&exclusive).unwrap();
}
