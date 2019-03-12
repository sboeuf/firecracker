// Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::super::super::Error as DeviceError;
use super::super::{ActivateError, ActivateResult, Queue, VirtioDevice};
use memory_model::GuestMemory;
use memory_model::MemoryMapping;
use memory_model::GuestAddress;
use std::cmp;
use std::io::Write;
use std::mem::transmute;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use sys_util::EventFd;
use vhost_user_backend::message::VhostUserMemoryRegion;
use vhost_user_backend::message::VhostUserVringAddrFlags;
use vhost_user_backend::message::VhostUserRequestCode;
use vhost_user_backend::message::VhostUserFsSlaveMsg;
use vhost_user_backend::Master;
use vhost_user_backend::UserMemoryContext;
use vhost_user_backend::VhostUserMaster;
use vhost_user_backend::Endpoint;
use virtio::vhost::*;
use DeviceEventT;
use EpollHandler;
use EpollHandlerPayload;
use nix::sys::socket::{socketpair, AddressFamily, SockType, SockFlag};
use std::os::unix::net::UnixStream;
use std::os::unix::io::FromRawFd;
use std::io::Read;
use std::fs::File;

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;


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

pub struct VhostUserEpollHandler {
    endp: Endpoint,
    mem: GuestMemory,
    desc_table: Vec<MemoryMapping>,
}

impl EpollHandler for VhostUserEpollHandler {
    fn handle_event(
        &mut self,
        device_event: DeviceEventT,
        _: u32,
        _: EpollHandlerPayload,
    ) -> std::result::Result<(), DeviceError> {
        match device_event {
            0 => {
                println!("###SEB 1 VhostUserEpollHandler event received!");
                let (hdr, rfds) = self.endp.recv_header().unwrap();
                let mut size = 0;

                let buf = match hdr.get_size() {
                    0 => vec![0u8; 0],
                    len => {
                        let (size2, rbuf, _) = self.endp.recv_into_buf::<[RawFd; 0]>(len as usize).unwrap();
                        size = size2;
                        rbuf
                    }
                };

                let msg = unsafe { &*(buf.as_ptr() as *const VhostUserFsSlaveMsg) };

                match hdr.get_code() {
                    /// SLAVE_FS_MAP
                    VhostUserRequestCode::RESET_OWNER => {
                        println!("###SEB VhostUserEpollHandler(): SLAVE_FS_MAP");
			for i in 0..8 {
			    if msg.len[i] == 0 {
				continue;
			    }

			    let base = 0x550000000u64 + msg.c_off[i];

			    println!("###SEB 1 VhostUserEpollHandler(): base {} to map", base);

			    let base = self
                                          .mem
                                          .get_host_address(GuestAddress(base as usize))
                                          .unwrap();
			    let base: *mut u8 = base.clone() as *mut u8;

			    println!("###SEB 2 VhostUserEpollHandler(): base {:?} to map", base);

			    for fd in rfds.iter() {
				println!("###SEB 2.5 VhostUserEpollHandler(): fd {:?}", fd);
			    }

			    println!("###SEB 2.6 VhostUserEpollHandler(): flags {:?}", msg.flags[i]);

			    if let Some(fds) = rfds.clone() {
				println!("###SEB 3 VhostUserEpollHandler(): base {:?} to map", base);

//				{
//				let file = OpenOptions::new()
//							.read(true)
//							.write(true)
//							.open("file_to_map")
//							.unwrap();
                                {
			        let mapping = MemoryMapping::from_fd_offset_fixed(base as *mut libc::c_void, fds[0], msg.len[i] as usize, msg.fd_off[i] as usize).unwrap();
				println!("###SEB 3.1 VhostUserEpollHandler(): mapping {:?}", mapping);
				self.desc_table.push(mapping);
                                }
				println!("###SEB 3.2 VhostUserEpollHandler()");
				unsafe { libc::close(fds[0]) };
//                                }


/*
//				let mut file = unsafe{ File::from_raw_fd(fds[0]) };
//				let mut buffer = Vec::new();
//				file.read_to_end(&mut buffer).unwrap();
//				println!("###SEB 3.5 VhostUserEpollHandler() buffer{:?}", buffer);
				unsafe {
				    // It is safe to overwrite the volatile memory. Accessing the guest
				    // memory as a mutable slice is OK because nothing assumes another
				    // thread won't change what is loaded.
                                    let ptr = std::slice::from_raw_parts_mut(base, msg.len[i] as usize);
//				    ptr[0] = 1;
//                                    ptr[1] = 3;
//                                    ptr[2] = 5;
				    let dst = &mut ptr[0..30];
//				    file.read_exact(dst).unwrap();
				    println!("###SEB 3.2 VhostUserEpollHandler(): dst {:?}", dst);
				}
*/
				println!("###SEB 3.5 VhostUserEpollHandler(): msg_len {:?}", msg.len[i]);
                            }

			    println!("###SEB 4 VhostUserEpollHandler(): base {:?} to map", base);
			}
                    }
                    /// SLAVE_FS_UNMAP
                    VhostUserRequestCode::SET_MEM_TABLE => {
                        println!("###SEB VhostUserEpollHandler(): SLAVE_FS_UNMAP");
                    }
                    /// SLAVE_FS_SYNC
                    VhostUserRequestCode::SET_LOG_BASE => {
                        println!("###SEB VhostUserEpollHandler(): SLAVE_FS_SYNC");
                    }
                    _ => return Err(DeviceError::UnknownEvent {
                            device: "VhostUserEpollHandler",
                            event: 0,
                        }),
                }

                Ok(())
            }
            other => Err(DeviceError::UnknownEvent {
                device: "VhostUserEpollHandler",
                event: other,
            }),
        }
    }
}

pub struct Fs {
    vu: Master,
    queue_sizes: Vec<u16>,
    shm_sizes: Vec<u64>,
    avail_features: u64,
    acked_features: u64,
    config_space: Vec<u8>,
    epoll_config: EpollConfig,
    epoll_config_vu: EpollConfig,
    mem: GuestMemory,
    interrupt: Option<EventFd>,
    sl_req_fd: Option<RawFd>,
    sl_req_fd2: Option<RawFd>,
}

impl Fs {
    /// Create a new virtio-fs device.
    pub fn new(
        path: String,
        tag: String,
        req_num_queues: usize,
        queue_size: u16,
        cache_size: usize,
        mem: &GuestMemory,
        epoll_config: EpollConfig,
        epoll_config_vu : EpollConfig,
    ) -> Result<Fs> {
        // Calculate the actual number of queues needed.
        let num_queues = NUM_QUEUE_OFFSET + req_num_queues;
        // Connect to the vhost-user socket.
        let mut master = Master::new(&path, num_queues as u64).map_err(Error::VhostUserConnect)?;
        // Retrieve available features only when connecting the first time.
        let avail_features = master.get_features().map_err(Error::VhostUserGetFeatures)?;

        // Check for VHOST_USER_PROTOCOL_F_SLAVE_REQ bit support
	
        // Create virtio device config space.
        // First by adding the tag.
        let mut config_space = tag.into_bytes();
        config_space.resize(CONFIG_SPACE_SIZE, 0);
        // And then by copying the number of queues.
        let mut num_queues_slice: [u8; 4] = unsafe { transmute((req_num_queues as u32).to_be()) };
        num_queues_slice.reverse();
        config_space[CONFIG_SPACE_TAG_SIZE..CONFIG_SPACE_SIZE].copy_from_slice(&num_queues_slice);

        // Create socket pair for vhost user slave requests
        let (fd1, fd2) = socketpair(AddressFamily::Unix, SockType::Stream, None, SockFlag::empty()).unwrap();

        Ok(Fs {
            vu: master,
            queue_sizes: vec![queue_size; num_queues],
            shm_sizes: vec![cache_size as u64; 1],
            avail_features,
            acked_features: 0u64,
            config_space,
            epoll_config,
            epoll_config_vu,
            mem: mem.clone(),
            interrupt: Some(EventFd::new().map_err(Error::VhostIrqCreate)?),
            sl_req_fd: Some(fd1),
            sl_req_fd2: Some(fd2),
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

    fn shm_sizes(&self) -> &[u64] {
        &self.shm_sizes.as_slice()
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
            .set_features(self.acked_features | 0x400)
            .map_err(Error::VhostUserSetFeatures)?;

        // TODO: Set the protocol features.

        // Set slave's request file descriptor.
        if let Some(sl_req_fd) = self.sl_req_fd.take() {
            if let Some(sl_req_fd2) = self.sl_req_fd2.take() {
		self.vu
		    .set_slave_req_fd(Some(sl_req_fd2))
		    .map_err(Error::VhostUserSetSlaveReqFd)?;

		let sl_req_evt_raw_fd = sl_req_fd.clone();
		let stream = unsafe{ UnixStream::from_raw_fd(sl_req_fd) };

		let handler = VhostUserEpollHandler {
		    endp: Endpoint::new_slave(stream),
                    mem: self.mem.clone(),
		    desc_table: Vec::new(),
		};

		//channel should be open and working
		self.epoll_config_vu
		    .sender
		    .send(Box::new(handler))
	            .expect("Failed to send through the channel");

		epoll::ctl(
		    self.epoll_config_vu.epoll_raw_fd,
		    epoll::ControlOptions::EPOLL_CTL_ADD,
		    sl_req_evt_raw_fd,
		    epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config_vu.first_token),
		)
		.map_err(ActivateError::EpollCtl)?;
            }
        }

        let mut mem_ctx = UserMemoryContext::new();
        for region in self.mem.regions.iter() {
            if region.mapping.fd == -1 {
                continue;
            }

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
