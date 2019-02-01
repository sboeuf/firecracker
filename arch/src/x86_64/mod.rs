// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

mod gdt;
pub mod interrupts;
pub mod layout;
mod mptable;
pub mod regs;

use std::mem;

use arch_gen::x86::bootparam::{boot_params, E820_RAM};
use memory_model::{AddressSpace, GuestAddress, GuestMemory};

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Invalid e820 setup params.
    E820Configuration,
    /// Error writing MP table to memory.
    MpTableSetup(mptable::Error),
    /// Failure in creating address space
    AddressSpaceSetup,
}

impl From<Error> for super::Error {
    fn from(e: Error) -> super::Error {
        super::Error::X86_64Setup(e)
    }
}

// Where BIOS/VGA magic would live on a real PC.
const EBDA_START: u64 = 0x9fc00;
const FIRST_ADDR_PAST_32BITS: usize = (1 << 32);
const MEM_32BIT_GAP_SIZE: usize = (768 << 20);

/// Create the address space for the virtual machine.
/// These should be used to configure the GuestMemory structure for the platform.
/// For x86_64 all addresses are valid from the start of the kernel except a
/// carve out at the end of 32bit address space.
pub fn create_address_space(size: usize) -> Result<AddressSpace, Error> {
    let memory_gap_start = GuestAddress(FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE);
    let memory_gap_end = GuestAddress(FIRST_ADDR_PAST_32BITS);
    let requested_memory_size = GuestAddress(size);
    let mut address_space = AddressSpace::with_capacity(1);

    // case1: guest memory fits before the gap
    if requested_memory_size <= memory_gap_start {
        address_space
            .add_default_memory(GuestAddress(0), size)
            .map_err(|_| Error::AddressSpaceSetup)?;
    // case2: guest memory extends beyond the gap
    } else {
        // push memory before the gap
        address_space
            .add_default_memory(GuestAddress(0), memory_gap_start.offset())
            .map_err(|_| Error::AddressSpaceSetup)?;
        address_space
            .add_default_memory(
                memory_gap_end,
                requested_memory_size.offset_from(memory_gap_start),
            )
            .map_err(|_| Error::AddressSpaceSetup)?;
    }

    Ok(address_space)
}

/// X86 specific memory hole/memory mapped devices/reserved area.
pub fn get_32bit_gap_start() -> usize {
    FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE
}

/// Configures the system and should be called once per vm before starting vcpu threads.
///
/// # Arguments
///
/// * `guest_mem` - The memory to be used by the guest.
/// * `cmdline_addr` - Address in `guest_mem` where the kernel command line was loaded.
/// * `cmdline_size` - Size of the kernel command line in bytes including the null terminator.
/// * `num_cpus` - Number of virtual CPUs the guest will have.
pub fn configure_system(
    guest_mem: &GuestMemory,
    cmdline_addr: GuestAddress,
    cmdline_size: usize,
    num_cpus: u8,
) -> super::Result<()> {
    const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
    const KERNEL_HDR_MAGIC: u32 = 0x53726448;
    const KERNEL_LOADER_OTHER: u8 = 0xff;
    const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x1000000; // Must be non-zero.
    let first_addr_past_32bits = GuestAddress(FIRST_ADDR_PAST_32BITS);
    let end_32bit_gap_start = GuestAddress(get_32bit_gap_start());

    let himem_start = GuestAddress(super::HIMEM_START);

    // Note that this puts the mptable at the last 1k of Linux's 640k base RAM
    mptable::setup_mptable(guest_mem, num_cpus).map_err(Error::MpTableSetup)?;

    let mut params: boot_params = Default::default();

    params.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    params.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
    params.hdr.header = KERNEL_HDR_MAGIC;
    params.hdr.cmd_line_ptr = cmdline_addr.offset() as u32;
    params.hdr.cmdline_size = cmdline_size as u32;
    params.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;

    add_e820_entry(&mut params, 0, EBDA_START, E820_RAM)?;

    let mem_end = guest_mem.end_addr();
    if mem_end < end_32bit_gap_start {
        add_e820_entry(
            &mut params,
            himem_start.offset() as u64,
            mem_end.offset_from(himem_start) as u64,
            E820_RAM,
        )?;
    } else {
        add_e820_entry(
            &mut params,
            himem_start.offset() as u64,
            end_32bit_gap_start.offset_from(himem_start) as u64,
            E820_RAM,
        )?;
        if mem_end > first_addr_past_32bits {
            add_e820_entry(
                &mut params,
                first_addr_past_32bits.offset() as u64,
                mem_end.offset_from(first_addr_past_32bits) as u64,
                E820_RAM,
            )?;
        }
    }

    let zero_page_addr = GuestAddress(layout::ZERO_PAGE_START);
    guest_mem
        .checked_offset(zero_page_addr, mem::size_of::<boot_params>())
        .ok_or(super::Error::ZeroPagePastRamEnd)?;
    guest_mem
        .write_obj_at_addr(params, zero_page_addr)
        .map_err(|_| super::Error::ZeroPageSetup)?;

    Ok(())
}

/// Add an e820 region to the e820 map.
/// Returns Ok(()) if successful, or an error if there is no space left in the map.
fn add_e820_entry(
    params: &mut boot_params,
    addr: u64,
    size: u64,
    mem_type: u32,
) -> Result<(), Error> {
    if params.e820_entries >= params.e820_map.len() as u8 {
        return Err(Error::E820Configuration);
    }

    params.e820_map[params.e820_entries as usize].addr = addr;
    params.e820_map[params.e820_entries as usize].size = size;
    params.e820_map[params.e820_entries as usize].type_ = mem_type;
    params.e820_entries += 1;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arch_gen::x86::bootparam::e820entry;
    use memory_model::AddressRegionType;

    #[test]
    fn regions_lt_4gb() {
        let space = create_address_space(1usize << 29).unwrap();
        assert_eq!(1, space.len());

        let region = space.get_region(0).unwrap();
        assert_eq!(GuestAddress(0), region.get_base());
        assert_eq!(1usize << 29, region.get_size());
    }

    #[test]
    fn regions_gt_4gb() {
        let space = create_address_space((1usize << 32) + 0x8000).unwrap();
        assert_eq!(2, space.len());

        let region = space.get_region(0).unwrap();
        assert_eq!(GuestAddress(0), region.get_base());
        let region = space.get_region(1).unwrap();
        assert_eq!(GuestAddress(1usize << 32), region.get_base());
    }

    #[test]
    fn test_32bit_gap() {
        assert_eq!(
            get_32bit_gap_start(),
            FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE
        );
    }

    #[test]
    fn test_system_configuration() {
        let no_vcpus = 4;
        let gm = GuestMemory::new(&vec![(GuestAddress(0), 0x10000)]).unwrap();
        let config_err = configure_system(&gm, GuestAddress(0), 0, 1);
        assert!(config_err.is_err());
        assert_eq!(
            config_err.unwrap_err(),
            super::super::Error::X86_64Setup(super::Error::MpTableSetup(
                mptable::Error::NotEnoughMemory
            ))
        );

        // Now assigning some memory that falls before the 32bit memory hole.
        let mem_size = 128 << 20;
        let space = create_address_space(mem_size).unwrap();
        let gm = space
            .map_guest_memory(&[AddressRegionType::DefaultMemory])
            .unwrap();
        configure_system(&gm, GuestAddress(0), 0, no_vcpus).unwrap();

        // Now assigning some memory that is equal to the start of the 32bit memory hole.
        let mem_size = 3328 << 20;
        let space = create_address_space(mem_size).unwrap();
        let gm = space
            .map_guest_memory(&[AddressRegionType::DefaultMemory])
            .unwrap();
        configure_system(&gm, GuestAddress(0), 0, no_vcpus).unwrap();

        // Now assigning some memory that falls after the 32bit memory hole.
        let mem_size = 3330 << 20;
        let space = create_address_space(mem_size).unwrap();
        let gm = space
            .map_guest_memory(&[AddressRegionType::DefaultMemory])
            .unwrap();
        configure_system(&gm, GuestAddress(0), 0, no_vcpus).unwrap();
    }

    #[test]
    fn test_add_e820_entry() {
        let e820_map = [(e820entry {
            addr: 0x1,
            size: 4,
            type_: 1,
        }); 128];

        let expected_params = boot_params {
            e820_map,
            e820_entries: 1,
            ..Default::default()
        };

        let mut params: boot_params = Default::default();
        add_e820_entry(
            &mut params,
            e820_map[0].addr,
            e820_map[0].size,
            e820_map[0].type_,
        )
        .unwrap();
        assert_eq!(
            format!("{:?}", params.e820_map[0]),
            format!("{:?}", expected_params.e820_map[0])
        );
        assert_eq!(params.e820_entries, expected_params.e820_entries);

        // Exercise the scenario where the field storing the length of the e820 entry table is
        // is bigger than the allocated memory.
        params.e820_entries = params.e820_map.len() as u8 + 1;
        assert!(add_e820_entry(
            &mut params,
            e820_map[0].addr,
            e820_map[0].size,
            e820_map[0].type_
        )
        .is_err());
    }
}
