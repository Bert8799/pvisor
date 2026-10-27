// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
use crate::devices::virtio::net::Result;
use crate::devices::virtio::net::{NUM_QUEUES, QUEUE_CONFIG};
use crate::devices::virtio::queue::Error as QueueError;
use crate::devices::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, InterruptTransport, QueueConfig,
    VirtioDevice, TYPE_NET,
};
use crate::devices::Error as DeviceError;

use super::backend::{ReadError, WriteError};
use super::worker::NetWorker;

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use virtio_bindings::virtio_net::VIRTIO_NET_F_MAC;
use virtio_bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_memory::{ByteValued, GuestMemoryError, GuestMemoryMmap};

const VIRTIO_F_VERSION_1: u32 = 32;

#[derive(Debug)]
pub enum FrontendError {
    DescriptorChainTooSmall,
    EmptyQueue,
    GuestMemory(GuestMemoryError),
    QueueError(QueueError),
    ReadOnlyDescriptor,
}

#[derive(Debug)]
pub enum RxError {
    Backend(ReadError),
    DeviceError(DeviceError),
}

#[derive(Debug)]
pub enum TxError {
    Backend(WriteError),
    DeviceError(DeviceError),
    QueueError(QueueError),
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioNetConfig {
    mac: [u8; 6],
    status: u16,
    max_virtqueue_pairs: u16,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioNetConfig {}

#[derive(Clone)]
pub enum VirtioNetBackend {
    UnixstreamFd(std::sync::Arc<OwnedFd>),
    UnixstreamPath(PathBuf),
    UnixgramFd(std::sync::Arc<OwnedFd>),
    UnixgramPath(PathBuf, bool),
    #[cfg(target_os = "linux")]
    Tap(String),
}

pub struct Net {
    snapshot_enabled: bool,
    frozen: bool,
    worker: Option<super::snapshot::Worker>,
    id: String,
    pub cfg_backend: VirtioNetBackend,

    avail_features: u64,
    acked_features: u64,

    pub(crate) device_state: DeviceState,

    config: VirtioNetConfig,
}

impl Net {
    /// Create a new virtio network device using the backend
    pub fn new(
        id: String,
        cfg_backend: VirtioNetBackend,
        mac: [u8; 6],
        features: u32,
    ) -> Result<Self> {
        let avail_features = features as u64
            | (1 << VIRTIO_NET_F_MAC)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | (1 << VIRTIO_F_VERSION_1);

        let config = VirtioNetConfig {
            mac,
            status: 0,
            max_virtqueue_pairs: 0,
        };

        Ok(Net {
            snapshot_enabled: false,
            frozen: false,
            worker: None,
            id,
            cfg_backend,

            avail_features,
            acked_features: 0u64,

            device_state: DeviceState::Inactive,
            config,
        })
    }

    /// Provides the ID of this net device.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Provides the virtio-net backend of this net device.
    pub fn backend(&self) -> &VirtioNetBackend {
        &self.cfg_backend
    }

    pub(crate) fn enable_snapshot_profile(&mut self) {
        self.snapshot_enabled = true;
    }

    fn validate_inactive_backend(&self) -> std::result::Result<(), String> {
        let VirtioNetBackend::UnixstreamFd(fd) = &self.cfg_backend else {
            return Err("inactive network capture requires an owned Unix-stream backend".into());
        };
        if super::snapshot::backend_ready(fd.as_raw_fd()).map_err(|e| e.to_string())? {
            return Err("network template backend became active".into());
        }
        Ok(())
    }
}

impl VirtioDevice for Net {
    fn freeze(&mut self) -> std::result::Result<bool, String> {
        if !self.snapshot_enabled
            || !matches!(
                self.cfg_backend,
                VirtioNetBackend::UnixstreamFd(_) | VirtioNetBackend::UnixstreamPath(_)
            )
        {
            return Err("network capture requires a pristine Unix-stream template profile".into());
        }
        if let Some(worker) = &mut self.worker {
            if !worker.freeze()? {
                return Ok(false);
            }
        } else {
            self.validate_inactive_backend()?;
        }
        self.frozen = true;
        Ok(true)
    }
    fn thaw(&mut self) -> std::result::Result<(), String> {
        if let Some(worker) = &mut self.worker {
            worker.thaw()?;
        }
        self.frozen = false;
        Ok(())
    }
    fn capture_state(&self) -> std::result::Result<super::super::DeviceSnapshot, String> {
        if !self.frozen {
            return Err("network device must be frozen".into());
        }
        if self.worker.is_none() {
            self.validate_inactive_backend()?;
        }
        Ok(super::super::DeviceSnapshot {
            queues: self.worker.as_ref().map(|w| w.capture()).transpose()?,
            state: super::super::DeviceSnapshotState::PristineNet {
                mac: self.config.mac,
            },
        })
    }
    fn restore_state(
        &mut self,
        state: &super::super::DeviceSnapshotState,
    ) -> std::result::Result<(), String> {
        let super::super::DeviceSnapshotState::PristineNet { mac } = state else {
            return Err("network snapshot type mismatch".into());
        };
        if !self.snapshot_enabled
            || self.device_state.is_activated()
            || self.worker.is_some()
            || *mac != self.config.mac
            || !matches!(
                self.cfg_backend,
                VirtioNetBackend::UnixstreamFd(_) | VirtioNetBackend::UnixstreamPath(_)
            )
        {
            return Err("network template configuration mismatch".into());
        }
        self.frozen = true;
        Ok(())
    }
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn device_name(&self) -> &str {
        "net"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        super::super::device::read_config_space(self.config.as_slice(), offset, data);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        log::warn!(
            "Net: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        let [rx_q, tx_q]: [_; NUM_QUEUES] = queues.try_into().map_err(|_| {
            error!("Cannot perform activate. Expected {} queue(s)", NUM_QUEUES);
            ActivateError::BadActivate
        })?;

        match NetWorker::new(
            rx_q,
            tx_q,
            interrupt.clone(),
            mem.clone(),
            self.acked_features,
            self.cfg_backend.clone(),
        ) {
            Ok(worker) => {
                if self.snapshot_enabled {
                    self.worker = Some(
                        super::snapshot::Worker::new(worker, self.frozen)
                            .map_err(|_| ActivateError::BadActivate)?,
                    );
                } else {
                    let _thread = worker.run().map_err(|_| ActivateError::BadActivate)?;
                }
                self.device_state = DeviceState::Activated(mem, interrupt);
                Ok(())
            }
            Err(err) => {
                error!(
                    "Error activating virtio-net ({}) backend: {err:?}",
                    self.id()
                );
                Err(ActivateError::BadActivate)
            }
        }
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::legacy::DummyIrqChip;
    use crate::devices::virtio::{DeviceSnapshot, DeviceSnapshotState, Queue};
    use crate::utils::eventfd::{EventFd, EFD_NONBLOCK};
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        sync::Arc,
        time::{Duration, Instant},
    };
    use vm_memory::{Bytes, GuestAddress};

    fn device() -> (Net, UnixStream) {
        let (local, peer) = UnixStream::pair().unwrap();
        let mut net = Net::new(
            "eth0".into(),
            VirtioNetBackend::UnixstreamFd(Arc::new(local.into())),
            [2, 0, 0, 0, 0, 1],
            0,
        )
        .unwrap();
        net.enable_snapshot_profile();
        (net, peer)
    }
    fn activate(
        net: &mut Net,
        saved: Option<&DeviceSnapshot>,
    ) -> (GuestMemoryMmap, Vec<Arc<EventFd>>) {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
        let queues = (0..2)
            .map(|index| {
                let queue = if let Some(saved) = saved {
                    saved.queues.as_ref().unwrap()[index]
                        .restore(1024, &mem)
                        .unwrap()
                } else {
                    let mut queue = Queue::new(1024);
                    queue.size = 8;
                    queue.ready = true;
                    let base = 0x1000 + index as u64 * 0x5000;
                    queue.desc_table = GuestAddress(base);
                    queue.avail_ring = GuestAddress(base + 0x200);
                    queue.used_ring = GuestAddress(base + 0x400);
                    queue
                };
                DeviceQueue::new(queue, Arc::new(EventFd::new(EFD_NONBLOCK).unwrap()))
            })
            .collect::<Vec<_>>();
        let events = queues.iter().map(|queue| queue.event.clone()).collect();
        net.activate(
            mem.clone(),
            InterruptTransport::new(DummyIrqChip::new().into(), "net-template-test".into())
                .unwrap(),
            queues,
        )
        .unwrap();
        (mem, events)
    }
    fn freeze(net: &mut Net) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !net.freeze().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    fn submit(mem: &GuestMemoryMmap) {
        let base = 0x6000;
        let mut frame = vec![0; super::super::VNET_HDR_LEN];
        frame.extend_from_slice(&[0xab; 60]);
        mem.write_slice(&frame, GuestAddress(0x18000)).unwrap();
        mem.write_obj(0x18000u64, GuestAddress(base)).unwrap();
        mem.write_obj(frame.len() as u32, GuestAddress(base + 8))
            .unwrap();
        mem.write_obj(0u16, GuestAddress(base + 12)).unwrap();
        mem.write_obj(0u16, GuestAddress(base + 0x204)).unwrap();
        mem.write_obj(1u16, GuestAddress(base + 0x202)).unwrap();
    }

    #[test]
    fn pristine_network_restore_parks_workers_then_uses_new_backend() {
        let (mut source, _source_peer) = device();
        let (_source_mem, _events) = activate(&mut source, None);
        assert!(source.capture_state().is_err());
        freeze(&mut source);
        let saved = source.capture_state().unwrap();
        let saved: DeviceSnapshot =
            serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        let (mut target, mut peer) = device();
        target.restore_state(&saved.state).unwrap();
        let (mem, _events) = activate(&mut target, Some(&saved));
        submit(&mem);
        peer.set_nonblocking(true).unwrap();
        assert_eq!(
            peer.read(&mut [0; 4]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        target.thaw().unwrap();
        peer.set_nonblocking(false).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut length = [0; 4];
        peer.read_exact(&mut length).unwrap();
        assert_eq!(u32::from_be_bytes(length), 60);
        let mut frame = [0; 60];
        peer.read_exact(&mut frame).unwrap();
        assert_eq!(frame, [0xab; 60]);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match target.freeze() {
                Err(error) => {
                    assert!(error.contains("before any traffic"));
                    break;
                }
                Ok(false) => {
                    assert!(Instant::now() < deadline);
                    std::thread::yield_now();
                }
                Ok(true) => panic!("active network was accepted as an inert template"),
            }
        }
    }

    #[test]
    fn unread_backend_bytes_and_pending_guest_tx_refuse_capture() {
        let (mut net, mut peer) = device();
        net.frozen = true;
        let (_mem, _events) = activate(&mut net, None);
        peer.write_all(&[0, 0]).unwrap(); // An incomplete frame header is already activity.
        assert!(net.freeze().is_err());
        let (mut net, _peer) = device();
        net.frozen = true;
        let (mem, _events) = activate(&mut net, None);
        submit(&mem);
        assert!(net.freeze().is_err());
    }

    #[test]
    fn ordinary_network_and_wrong_mac_do_not_accept_template_restore() {
        let (local, _peer) = UnixStream::pair().unwrap();
        let mut net = Net::new(
            "eth0".into(),
            VirtioNetBackend::UnixstreamFd(Arc::new(local.into())),
            [2; 6],
            0,
        )
        .unwrap();
        assert!(net.freeze().is_err());
        assert!(net
            .restore_state(&DeviceSnapshotState::PristineNet { mac: [2; 6] })
            .is_err());
        net.enable_snapshot_profile();
        assert!(net
            .restore_state(&DeviceSnapshotState::PristineNet { mac: [3; 6] })
            .is_err());
    }

    #[test]
    fn inactive_network_rechecks_backend_at_freeze_and_capture() {
        let (mut net, mut peer) = device();
        freeze(&mut net);
        assert!(net.capture_state().unwrap().queues.is_none());
        peer.write_all(&[0, 0]).unwrap();
        assert!(net.capture_state().is_err());
        assert!(net.freeze().is_err());
        let (mut net, peer) = device();
        drop(peer);
        assert!(net.freeze().is_err());
    }
}
