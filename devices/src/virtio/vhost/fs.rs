// Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::super::super::Error as DeviceError;
use super::super::{ActivateError, ActivateResult, Queue, VirtioDevice};
use memory_model::GuestMemory;
use std::cmp;
use std::io::Write;
use std::mem::transmute;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use sys_util::EventFd;
use vhost_user_backend::message::VhostUserMemoryRegion;
use vhost_user_backend::message::VhostUserVringAddrFlags;
use vhost_user_backend::Master;
use vhost_user_backend::UserMemoryContext;
use vhost_user_backend::VhostUserMaster;
use virtio::vhost::*;
use DeviceEventT;
use EpollHandler;
use EpollHandlerPayload;

const CONFIG_SPACE_TAG_SIZE: usize = 36;
const CONFIG_SPACE_NUM_QUEUES_SIZE: usize = 4;
const CONFIG_SPACE_SIZE: usize = CONFIG_SPACE_TAG_SIZE + CONFIG_SPACE_NUM_QUEUES_SIZE;
const NUM_QUEUE_OFFSET: usize = 2;

pub const FS_EVENTS_COUNT: usize = 1;

pub struct EpollConfig {
    first_token: u64,
    epoll_raw_fd: RawFd,
    sender: mpsc::Sender<Box<EpollHandler>>,
}

impl EpollConfig {
    pub fn new(
        first_token: u64,
        epoll_raw_fd: RawFd,
        sender: mpsc::Sender<Box<EpollHandler>>,
    ) -> Self {
        EpollConfig {
            first_token,
            epoll_raw_fd,
            sender,
        }
    }
}

pub struct FsEpollHandler {
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
    queue_evt: EventFd,
}

impl EpollHandler for FsEpollHandler {
    fn handle_event(
        &mut self,
        device_event: DeviceEventT,
        _: u32,
        _: EpollHandlerPayload,
    ) -> std::result::Result<(), DeviceError> {
        match device_event {
            0 => {
                if let Err(e) = self.queue_evt.read() {
                    error!("Failed to get queue event: {:?}", e);
                    Err(DeviceError::FailedReadingQueue {
                        event_type: "queue event",
                        underlying: e,
                    })
                } else {
                    self.interrupt_status
                        .fetch_or(INTERRUPT_STATUS_USED_RING as usize, Ordering::SeqCst);
                    self.interrupt_evt
                        .write(1)
                        .map_err(|e| DeviceError::FailedSignalingUsedQueue(e))
                }
            }
            other => Err(DeviceError::UnknownEvent {
                device: "FsEpollHandler",
                event: other,
            }),
        }
    }
}

pub struct Fs {
    vu: Master,
    queue_sizes: Vec<u16>,
    avail_features: u64,
    acked_features: u64,
    config_space: Vec<u8>,
    epoll_config: EpollConfig,
    mem: GuestMemory,
    interrupt: Option<EventFd>,
}

impl Fs {
    /// Create a new virtio-fs device.
    pub fn new(
        path: String,
        tag: String,
        req_num_queues: usize,
        queue_size: u16,
        mem: &GuestMemory,
        epoll_config: EpollConfig,
    ) -> Result<Fs> {
        // Calculate the actual number of queues needed.
        let num_queues = NUM_QUEUE_OFFSET + req_num_queues;
        // Connect to the vhost-user socket.
        let mut master = Master::new(&path, num_queues as u64).map_err(Error::VhostUserConnect)?;
        // Retrieve available features only when connecting the first time.
        let avail_features = master.get_features().map_err(Error::VhostUserGetFeatures)?;
        // Create virtio device config space.
        // First by adding the tag.
        let mut config_space = tag.into_bytes();
        config_space.resize(CONFIG_SPACE_SIZE, 0);
        // And then by copying the number of queues.
        let mut num_queues_slice: [u8; 4] = unsafe { transmute((req_num_queues as u32).to_be()) };
        num_queues_slice.reverse();
        config_space[CONFIG_SPACE_TAG_SIZE..CONFIG_SPACE_SIZE].copy_from_slice(&num_queues_slice);

        Ok(Fs {
            vu: master,
            queue_sizes: vec![queue_size; num_queues],
            avail_features,
            acked_features: 0u64,
            config_space,
            epoll_config,
            mem: mem.clone(),
            interrupt: Some(EventFd::new().map_err(Error::VhostIrqCreate)?),
        })
    }
}

impl VirtioDevice for Fs {
    fn device_type(&self) -> u32 {
        TYPE_FS
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes.as_slice()
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            // Get the lower 32-bits of the features bitfield.
            0 => self.avail_features as u32,
            // Get the upper 32-bits of the features bitfield.
            1 => (self.avail_features >> 32) as u32,
            _ => {
                warn!("fs: virtio-fs got request for features page: {}", page);
                0u32
            }
        }
    }

    fn ack_features(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => value as u64,
            1 => (value as u64) << 32,
            _ => {
                warn!(
                    "fs: virtio-fs device cannot ack unknown feature page: {}",
                    page
                );
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            warn!("fs: virtio-fs got unknown feature ack: {:x}", v);

            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_len = self.config_space.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write(&self.config_space[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let data_len = data.len() as u64;
        let config_len = self.config_space.len() as u64;
        if offset + data_len > config_len {
            error!("Failed to write config space");
            return;
        }
        let (_, right) = self.config_space.split_at_mut(offset as usize);
        right.copy_from_slice(&data[..]);
    }

    fn activate(
        &mut self,
        _: GuestMemory,
        interrupt_evt: EventFd,
        interrupt_status: Arc<AtomicUsize>,
        queues: Vec<Queue>,
        queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        if queues.len() != self.queue_sizes.len() || queue_evts.len() != self.queue_sizes.len() {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                self.queue_sizes.len(),
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        // Set vhost-user owner.
        self.vu.set_owner().map_err(Error::VhostUserSetOwner)?;

        // Set backend features.
        self.vu
            .set_features(self.acked_features)
            .map_err(Error::VhostUserSetFeatures)?;

        let mut mem_ctx = UserMemoryContext::new();
        for region in self.mem.regions.iter() {
            let vu_mem_reg = VhostUserMemoryRegion {
                guest_phys_addr: region.guest_base.offset() as u64,
                memory_size: region.mapping.size as u64,
                userspace_addr: region.mapping.addr as u64,
                mmap_offset: region.mapping.offset as u64,
            };

            mem_ctx.append(&vu_mem_reg, region.mapping.fd);
        }
        self.vu
            .set_mem_table(&mem_ctx)
            .map_err(Error::VhostUserSetMemTable)?;

        if let Some(interrupt) = self.interrupt.take() {
            for (queue_index, ref queue) in queues.iter().enumerate() {
                self.vu
                    .set_vring_num(queue_index as u32, queue.get_max_size() as u32)
                    .map_err(Error::VhostUserSetVringNum)?;

                let desc_addr = self
                    .mem
                    .get_host_address(queue.desc_table)
                    .map_err(Error::DescriptorTableAddress)?;
                let used_addr = self
                    .mem
                    .get_host_address(queue.used_ring)
                    .map_err(Error::UsedAddress)?;
                let avail_addr = self
                    .mem
                    .get_host_address(queue.avail_ring)
                    .map_err(Error::AvailAddress)?;

                self.vu
                    .set_vring_addr(
                        queue_index as u32,
                        VhostUserVringAddrFlags::empty(),
                        desc_addr as u64,
                        used_addr as u64,
                        avail_addr as u64,
                        0u64,
                    )
                    .map_err(Error::VhostUserSetVringAddr)?;

                self.vu
                    .set_vring_base(queue_index as u32, 0u32)
                    .map_err(Error::VhostUserSetVringBase)?;

                self.vu
                    .set_vring_call(queue_index as u8, Some(interrupt.as_raw_fd()))
                    .map_err(Error::VhostUserSetVringCall)?;

                self.vu
                    .set_vring_kick(queue_index as u8, Some(queue_evts[queue_index].as_raw_fd()))
                    .map_err(Error::VhostUserSetVringKick)?;
            }

            let queue_evt_raw_fd = interrupt.as_raw_fd();

            let handler = FsEpollHandler {
                interrupt_status,
                interrupt_evt,
                queue_evt: interrupt,
            };

            //channel should be open and working
            self.epoll_config
                .sender
                .send(Box::new(handler))
                .expect("Failed to send through the channel");

            epoll::ctl(
                self.epoll_config.epoll_raw_fd,
                epoll::ControlOptions::EPOLL_CTL_ADD,
                queue_evt_raw_fd,
                epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.first_token),
            )
            .map_err(ActivateError::EpollCtl)?;
        }

        return Ok(());
    }
}
