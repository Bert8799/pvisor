//! Stop/join ownership for pristine network templates. No packets, connection
//! state or host descriptors are serialized. Normal network workers do not use it.
use super::worker::NetWorker;
use crate::devices::virtio::QueueSnapshot;
use crate::utils::eventfd::{EventFd, EFD_NONBLOCK};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;

pub(super) fn backend_ready(fd: std::os::fd::RawFd) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // POLLHUP/POLLERR/POLLNVAL are also activity, never an empty live backend.
    Ok(result > 0)
}

#[derive(Clone)]
pub(super) struct Stop {
    requested: Arc<AtomicBool>,
    pub event: Arc<EventFd>,
}
impl Stop {
    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
    fn request(&self) -> Result<(), String> {
        if !self.requested.swap(true, Ordering::AcqRel) {
            self.event.write(1).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

pub(super) struct Worker {
    stop: Stop,
    running: Option<JoinHandle<NetWorker>>,
    parked: Option<NetWorker>,
    fault: bool,
}
impl Worker {
    pub fn new(mut worker: NetWorker, frozen: bool) -> std::io::Result<Self> {
        let stop = Stop {
            requested: Arc::new(AtomicBool::new(frozen)),
            event: Arc::new(EventFd::new(EFD_NONBLOCK)?),
        };
        worker.stop = Some(stop.clone());
        let (running, parked) = if frozen {
            (None, Some(worker))
        } else {
            (Some(worker.run()?), None)
        };
        Ok(Self {
            stop,
            running,
            parked,
            fault: false,
        })
    }
    pub fn freeze(&mut self) -> Result<bool, String> {
        if self.fault {
            return Err("network worker failed".into());
        }
        self.stop.request()?;
        if self
            .running
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            match self.running.take().unwrap().join() {
                Ok(worker) => self.parked = Some(worker),
                Err(_) => {
                    self.fault = true;
                    return Err("network worker panicked".into());
                }
            }
        }
        if let Some(worker) = &self.parked {
            worker.validate_pristine()?;
            return Ok(true);
        }
        Ok(false)
    }
    pub fn capture(&self) -> Result<Vec<QueueSnapshot>, String> {
        if !self.stop.requested() || self.running.is_some() || self.fault {
            return Err("network worker must be parked before capture".into());
        }
        self.parked
            .as_ref()
            .ok_or("network worker is missing")?
            .snapshot_queues()
    }
    pub fn thaw(&mut self) -> Result<(), String> {
        if self.fault {
            return Err("network worker failed".into());
        }
        if self.running.is_some() {
            return Ok(());
        }
        let worker = self.parked.take().ok_or("network freeze still pending")?;
        let _ = self.stop.event.read();
        self.stop.requested.store(false, Ordering::Release);
        match worker.run() {
            Ok(thread) => {
                self.running = Some(thread);
                Ok(())
            }
            Err(error) => {
                self.fault = true;
                Err(error.to_string())
            }
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.request();
    }
}
