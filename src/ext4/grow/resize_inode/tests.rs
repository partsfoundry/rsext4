//! Tests that pin each check on inode 7 before growth, and the references one
//! growth step stages for it.
//!
//! The rig formats a four-group filesystem with 1 KiB blocks, which has no
//! reserved descriptor blocks, then gives the mounted view a reserve and writes
//! the matching inode 7 tree on the device. Group 0's inode bitmap and inode
//! table are moved out of the way first so that up to 256 reserved primary
//! blocks fit after the live descriptor block.

use super::*;
use crate::endian::DiskFormat;
use crate::error::ErrorContext;
use crate::{
    Clock, DeviceCapabilities, DeviceGeometry, Ext4Timestamp, MkfsOptions, SectorId,
    mkfs_with_options,
};

const BLOCK: usize = 1024;
const BLOCKS: usize = 30000;
/// The last block of group 1, so that each operand of the group arithmetic matters.
const DOUBLE: u64 = 16384;
/// Free blocks in group 0, past the journal, that hold group 0's moved inode
/// bitmap and the first two blocks of its inode table.
const MOVED_INODE_BITMAP: u64 = 6000;
const MOVED_INODE_TABLE: u64 = 6001;
/// A free block in group 2 that stands in as group 1's block bitmap.
const DOUBLE_BITMAP: u64 = 20000;

struct MemoryDevice {
    bytes: Vec<u8>,
}

impl BlockIo for MemoryDevice {
    fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * BLOCK;
        self.bytes
            .get_mut(start..start + buffer.len())
            .ok_or_else(Ext4Error::io)?
            .copy_from_slice(buffer);
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * BLOCK;
        buffer.copy_from_slice(
            self.bytes
                .get(start..start + buffer.len())
                .ok_or_else(Ext4Error::io)?,
        );
        Ok(())
    }

    fn geometry(&self) -> DeviceGeometry {
        DeviceGeometry::new(BLOCK as u32, (self.bytes.len() / BLOCK) as u64)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        DeviceCapabilities {
            flush: true,
            ..DeviceCapabilities::default()
        }
    }

    fn flush(&mut self) -> Ext4Result<()> {
        Ok(())
    }
}

impl Clock for MemoryDevice {
    fn now(&self) -> Ext4Result<Ext4Timestamp> {
        Ok(Ext4Timestamp::new(1_700_000_000, 0))
    }
}

fn wrap(device: MemoryDevice) -> Jbd2Dev<MemoryDevice> {
    let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
    journal
        .set_filesystem_block_size(BLOCK)
        .expect("block size");
    journal
}

fn operation(result: Ext4Result<()>) -> &'static str {
    let Err(error) = result else {
        panic!("the call must be refused");
    };
    match error.context() {
        Some(ErrorContext::Operation { op }) => op,
        _ => panic!("the error names its operation"),
    }
}

fn get(bytes: &[u8], slot: usize) -> u32 {
    u32::from_le_bytes(bytes[slot * 4..slot * 4 + 4].try_into().unwrap())
}

fn put(bytes: &mut [u8], slot: usize, value: u64) {
    bytes[slot * 4..slot * 4 + 4].copy_from_slice(&u32::try_from(value).unwrap().to_le_bytes());
}

/// A mounted filesystem whose inode 7 owns `reserve` reserved descriptor blocks.
struct Rig {
    fs: Ext4FileSystem,
    device: MemoryDevice,
    layout: GrowthLayout,
}

impl Rig {
    fn new() -> Self {
        Self::with(1, |_| {})
    }

    /// Builds the rig after `adjust` changes the mounted view, which may change
    /// the group count or the features that place backups.
    fn with(reserve: u16, adjust: impl FnOnce(&mut Ext4FileSystem)) -> Self {
        let mut journal = wrap(MemoryDevice {
            bytes: vec![0; BLOCKS * BLOCK],
        });
        mkfs_with_options(
            &mut journal,
            MkfsOptions {
                block_size: BLOCK as u32,
                ..MkfsOptions::default()
            },
        )
        .expect("mkfs");
        let mut journal = wrap(journal.into_inner());
        let mut fs = Ext4FileSystem::mount(&mut journal).expect("mount");
        let device = journal.into_inner();
        fs.superblock.s_reserved_gdt_blocks = reserve;
        fs.superblock.s_feature_compat |= Ext4Superblock::EXT4_FEATURE_COMPAT_RESIZE_INODE;
        adjust(&mut fs);
        let mut rig = Self {
            layout: GrowthLayout {
                blocks: fs.superblock.blocks_count() + 10000,
                groups: fs.group_count + 1,
                gdt_blocks: 1,
            },
            fs,
            device,
        };
        rig.move_group_zero_inodes();
        rig.write_tree();
        rig
    }

    fn block(&mut self, block: u64) -> &mut [u8] {
        let start = block as usize * BLOCK;
        &mut self.device.bytes[start..start + BLOCK]
    }

    fn move_group_zero_inodes(&mut self) {
        let descriptor = self.fs.group_descs[0];
        let bitmap = descriptor.inode_bitmap();
        let table = descriptor.inode_table();
        let bitmap = self.block(bitmap).to_vec();
        self.block(MOVED_INODE_BITMAP).copy_from_slice(&bitmap);
        for offset in 0..2 {
            let bytes = self.block(table + offset).to_vec();
            self.block(MOVED_INODE_TABLE + offset)
                .copy_from_slice(&bytes);
        }
        let descriptor = &mut self.fs.group_descs[0];
        descriptor.bg_inode_bitmap_lo = MOVED_INODE_BITMAP as u32;
        descriptor.bg_inode_table_lo = MOVED_INODE_TABLE as u32;
        // Mount may have cached the bitmap; later reads must see the moved copy.
        self.fs.bitmap_cache.clear();
    }

    fn gdt(&self) -> u32 {
        self.fs
            .group_count
            .div_ceil(self.fs.superblock.descs_per_block())
    }

    fn backups(&self) -> Vec<u32> {
        (1..self.fs.group_count)
            .filter(|group| group_has_superblock(&self.fs.superblock, *group))
            .collect()
    }

    fn primary(&self, index: u32) -> u64 {
        u64::from(self.fs.superblock.s_first_data_block) + 1 + u64::from(index)
    }

    fn write_tree(&mut self) {
        let sb = self.fs.superblock;
        let reserve = u32::from(sb.s_reserved_gdt_blocks);
        let gdt = self.gdt();
        let backups = self.backups();
        let pointers = BLOCK / 4;
        let mut indirect = vec![0; BLOCK];
        for index in gdt..gdt + reserve {
            put(
                &mut indirect,
                index as usize % pointers,
                self.primary(index),
            );
            let mut list = vec![0; BLOCK];
            for (slot, group) in backups.iter().copied().enumerate() {
                put(
                    &mut list,
                    slot,
                    group_first_block(&sb, group).unwrap() + 1 + u64::from(index),
                );
            }
            let primary = self.primary(index);
            self.block(primary).copy_from_slice(&list);
        }
        self.block(DOUBLE).copy_from_slice(&indirect);
        let sectors = (1 + u64::from(reserve) * (backups.len() as u64 + 1)) * 2;
        self.set_inode(|inode| {
            inode.i_mode = Ext4Inode::S_IFREG | 0o600;
            inode.i_flags = 0;
            inode.i_block = [0; 15];
            inode.i_block[13] = DOUBLE as u32;
            inode.i_blocks_lo = sectors as u32;
        });
        let mut bitmap = vec![0; BLOCK];
        let relative = (DOUBLE - u64::from(sb.s_first_data_block)) % 8192;
        bitmap[relative as usize / 8] |= 1 << (relative % 8);
        self.set_block_bitmap(&bitmap);
    }

    /// Rewrites inode 7 on the device with `change` applied and a fresh checksum.
    fn set_inode(&mut self, change: impl FnOnce(&mut Ext4Inode)) {
        let number = InodeNumber::new(7).unwrap();
        let (block, offset) = self.fs.inode_table_location(number).unwrap();
        let size = usize::from(self.fs.superblock.s_inode_size);
        let mut raw = self.block(block.raw())[offset..offset + size].to_vec();
        let mut inode = Ext4Inode::from_disk_bytes(&raw);
        change(&mut inode);
        ext4_update_raw_inode_checksum(&self.fs.superblock, number, &mut inode, &mut raw).unwrap();
        self.block(block.raw())[offset..offset + size].copy_from_slice(&raw);
    }

    /// Makes `bitmap` group 1's block bitmap, with a matching checksum.
    fn set_block_bitmap(&mut self, bitmap: &[u8]) {
        self.block(DOUBLE_BITMAP).copy_from_slice(bitmap);
        let sb = self.fs.superblock;
        let descriptor = &mut self.fs.group_descs[1];
        descriptor.bg_block_bitmap_lo = DOUBLE_BITMAP as u32;
        descriptor.update_bitmap_checksums(&sb, Some(bitmap), None);
    }

    fn validate(mut self) -> Ext4Result<()> {
        let mut journal = wrap(self.device);
        validate(&mut self.fs, &mut journal, &self.layout)
    }

    /// Stages one step to `groups` groups that leaves `reserve` reserved blocks.
    fn update(mut self, groups: u32, reserve: u16) -> (Ext4Result<()>, Images, Self) {
        let mut candidate = self.fs.superblock;
        candidate.s_reserved_gdt_blocks = reserve;
        let mut images = Images::new();
        let mut journal = wrap(self.device);
        let result = update(&mut self.fs, &mut journal, &candidate, groups, &mut images);
        self.device = journal.into_inner();
        (result, images, self)
    }

    /// Reads inode 7 out of staged `images` and checks its checksum.
    fn staged_inode(&self, images: &Images) -> Ext4Inode {
        let number = InodeNumber::new(7).unwrap();
        let (block, offset) = self.fs.inode_table_location(number).unwrap();
        let size = usize::from(self.fs.superblock.s_inode_size);
        let raw = &images[&block][offset..offset + size];
        let inode = Ext4Inode::from_disk_bytes(raw);
        verify_raw_inode_checksum(&self.fs.superblock, number, &inode, raw)
            .expect("the staged inode carries its checksum");
        inode
    }
}

// ---- validate ----

#[test]
fn a_complete_resize_inode_is_accepted() {
    Rig::new().validate().expect("a complete tree is accepted");
}

#[test]
fn growth_that_adds_no_blocks_checks_nothing() {
    let mut rig = Rig::new();
    rig.fs.superblock.s_feature_compat &= !Ext4Superblock::EXT4_FEATURE_COMPAT_RESIZE_INODE;
    rig.layout.blocks = rig.fs.superblock.blocks_count();
    rig.validate()
        .expect("unchanged geometry needs no resize inode");
}

#[test]
fn a_filesystem_without_reserved_blocks_checks_nothing() {
    let mut rig = Rig::new();
    rig.fs.superblock.s_feature_compat &= !Ext4Superblock::EXT4_FEATURE_COMPAT_RESIZE_INODE;
    rig.fs.superblock.s_reserved_gdt_blocks = 0;
    rig.validate().expect("no reserve needs no resize inode");
}

#[test]
fn a_reserve_without_the_resize_inode_feature_is_refused() {
    let mut rig = Rig::new();
    rig.fs.superblock.s_feature_compat &= !Ext4Superblock::EXT4_FEATURE_COMPAT_RESIZE_INODE;
    assert_eq!(operation(rig.validate()), "grow:resize_inode_missing");
}

#[test]
fn inode_seven_must_be_a_regular_file() {
    let mut rig = Rig::new();
    rig.set_inode(|inode| inode.i_mode = Ext4Inode::S_IFDIR | 0o700);
    assert_eq!(operation(rig.validate()), "grow:resize_inode_shape");
}

#[test]
fn inode_seven_must_not_use_extents() {
    let mut rig = Rig::new();
    rig.set_inode(|inode| inode.i_flags |= Ext4Inode::EXT4_EXTENTS_FL);
    assert_eq!(operation(rig.validate()), "grow:resize_inode_shape");
}

#[test]
fn inode_seven_must_have_a_double_indirect_block() {
    let mut rig = Rig::new();
    rig.set_inode(|inode| inode.i_block[13] = 0);
    assert_eq!(operation(rig.validate()), "grow:resize_inode_shape");
}

#[test]
fn inode_seven_must_have_no_other_block_pointer() {
    let mut rig = Rig::new();
    rig.set_inode(|inode| inode.i_block[0] = 20001);
    assert_eq!(operation(rig.validate()), "grow:resize_inode_shape");
}

#[test]
fn a_double_indirect_block_past_the_filesystem_is_refused() {
    let mut rig = Rig::new();
    // The device is longer than the filesystem, so only the filesystem bound refuses.
    rig.device.bytes.resize((BLOCKS + 1000) * BLOCK, 0);
    rig.set_inode(|inode| inode.i_block[13] = BLOCKS as u32 + 500);
    assert_eq!(operation(rig.validate()), "grow:resize_inode_ownership");
}

#[test]
fn a_double_indirect_block_past_the_device_is_refused() {
    let mut rig = Rig::new();
    // The filesystem is longer than the device, so only the device bound refuses.
    rig.device.bytes.truncate(DOUBLE as usize * BLOCK);
    assert_eq!(operation(rig.validate()), "grow:resize_inode_ownership");
}

#[test]
fn a_double_indirect_block_in_a_system_zone_is_refused() {
    let mut rig = Rig::new();
    let backup_superblock = group_first_block(&rig.fs.superblock, 1).unwrap();
    rig.set_inode(|inode| inode.i_block[13] = backup_superblock as u32);
    assert_eq!(operation(rig.validate()), "grow:resize_inode_ownership");
}

#[test]
fn an_unallocated_inode_seven_is_refused() {
    let mut rig = Rig::new();
    let sb = rig.fs.superblock;
    let bitmap = rig.block(MOVED_INODE_BITMAP);
    bitmap[0] &= !(1 << 6);
    let bitmap = bitmap.to_vec();
    rig.fs.group_descs[0].update_bitmap_checksums(&sb, None, Some(&bitmap));
    assert_eq!(operation(rig.validate()), "grow:resize_inode_ownership");
}

#[test]
fn a_double_indirect_block_free_in_its_bitmap_is_refused() {
    let mut rig = Rig::new();
    rig.set_block_bitmap(&[0; BLOCK]);
    assert_eq!(operation(rig.validate()), "grow:resize_inode_unallocated");
}

#[test]
fn a_bitmap_with_a_stale_checksum_is_refused() {
    let mut rig = Rig::new();
    rig.fs.group_descs[1].bg_block_bitmap_csum_lo ^= 1;
    assert_eq!(operation(rig.validate()), "grow:resize_inode_unallocated");
}

#[test]
fn a_full_pointer_block_of_reserves_is_accepted() {
    let mut rig = Rig::with(256, |_| {});
    // One target table may fill every pointer slot past the live table.
    rig.layout.gdt_blocks = 256 + 1;
    rig.validate().expect("256 reserves fit one pointer block");
}

#[test]
fn more_reserves_than_pointer_slots_are_refused() {
    let rig = Rig::with(257, |_| {});
    assert_eq!(operation(rig.validate()), "grow:resize_inode_capacity");
}

#[test]
fn a_target_table_past_the_pointer_slots_is_refused() {
    let mut rig = Rig::new();
    rig.layout.gdt_blocks = 256 + 2;
    assert_eq!(operation(rig.validate()), "grow:resize_inode_capacity");
}

#[test]
fn a_stray_double_indirect_pointer_is_refused() {
    let mut rig = Rig::new();
    put(rig.block(DOUBLE), 5, 20001);
    assert_eq!(
        operation(rig.validate()),
        "grow:resize_inode_unexpected_pointer"
    );
}

fn without_sparse_super(fs: &mut Ext4FileSystem) {
    fs.superblock.s_feature_ro_compat &= !Ext4Superblock::EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER;
}

#[test]
fn every_group_is_a_backup_without_sparse_super() {
    let mut rig = Rig::with(1, without_sparse_super);
    assert_eq!(rig.backups(), [1, 2, 3]);
    // The target may hold exactly one pointer block of backups.
    rig.layout.groups = 256 + 1;
    rig.validate().expect("256 future backups fit");
}

#[test]
fn more_future_backups_than_pointer_slots_are_refused() {
    let mut rig = Rig::with(1, without_sparse_super);
    rig.layout.groups = 256 + 2;
    assert_eq!(operation(rig.validate()), "grow:backup_index_capacity");
}

#[test]
fn a_wrong_block_count_is_refused() {
    let mut rig = Rig::new();
    rig.set_inode(|inode| inode.i_blocks_lo += 2);
    assert_eq!(operation(rig.validate()), "grow:resize_inode_blocks");
}

#[test]
fn a_wrong_primary_pointer_is_refused() {
    let mut rig = Rig::new();
    let wrong = rig.primary(2);
    put(rig.block(DOUBLE), 1, wrong);
    assert_eq!(operation(rig.validate()), "grow:reserved_primary_pointer");
}

#[test]
fn a_wrong_backup_pointer_is_refused() {
    let mut rig = Rig::new();
    let primary = rig.primary(1);
    let list = rig.block(primary);
    let last = get(list, 1);
    put(list, 1, u64::from(last) + 1);
    assert_eq!(operation(rig.validate()), "grow:reserved_backup_pointer");
}

#[test]
fn a_pointer_after_the_backups_is_refused() {
    let mut rig = Rig::new();
    let primary = rig.primary(1);
    put(rig.block(primary), 2, 20001);
    assert_eq!(operation(rig.validate()), "grow:reserved_backup_tail");
}

/// Live descriptor blocks that put the last target group's second reserved
/// block exactly at the 32-bit limit when the target has 2^19 groups. Two
/// reserves keep the sum of tables and reserves apart from their product.
fn tables_at_the_32_bit_limit(fs: &mut Ext4FileSystem) {
    fs.group_count = 8187 * fs.superblock.descs_per_block();
}

#[test]
fn a_reserve_ending_at_the_32_bit_limit_is_accepted() {
    let mut rig = Rig::with(2, tables_at_the_32_bit_limit);
    rig.layout.groups = 1 << 19;
    let last = group_first_block(&rig.fs.superblock, rig.layout.groups - 1).unwrap();
    assert_eq!(last + 1 + 8187 + 2, u64::from(u32::MAX));
    rig.validate().expect("the last pointer fits 32 bits");
}

#[test]
fn a_reserve_past_the_32_bit_limit_is_refused() {
    let mut rig = Rig::with(3, tables_at_the_32_bit_limit);
    rig.layout.groups = 1 << 19;
    assert_eq!(operation(rig.validate()), "grow:resize_inode_32bit_pointer");
}

// ---- update ----

#[test]
fn a_filesystem_without_reserved_blocks_stages_nothing() {
    let mut rig = Rig::new();
    rig.fs.superblock.s_reserved_gdt_blocks = 0;
    let (result, images, _) = rig.update(6, 0);
    result.unwrap();
    assert!(images.is_empty());
}

#[test]
fn a_step_with_no_new_table_or_backup_stages_nothing() {
    // Group 4 has no backup and fits the live table.
    let (result, images, _) = Rig::new().update(5, 1);
    result.unwrap();
    assert!(images.is_empty());
}

#[test]
fn a_consumed_reserve_loses_its_pointer() {
    // Sixteen live groups fill one table; group 16 needs a second without a backup.
    let rig = Rig::with(2, |fs| fs.group_count = 16);
    let backups = rig.backups().len() as u64;
    let (result, images, rig) = rig.update(17, 1);
    result.unwrap();
    let indirect = &images[&AbsoluteBN::new(DOUBLE)];
    assert_eq!(get(indirect, 1), 0);
    assert_eq!(u64::from(get(indirect, 2)), rig.primary(2));
    assert_eq!(
        images.len(),
        2,
        "only the indirect block and inode 7 change"
    );
    let inode = rig.staged_inode(&images);
    assert_eq!(u64::from(inode.i_blocks_lo), (1 + backups + 1) * 2);
}

#[test]
fn a_consumed_reserve_past_one_pointer_block_wraps_its_slot() {
    // 4096 live groups fill 256 tables, so the consumed index 256 sits in slot 0.
    let rig = Rig::with(1, |fs| fs.group_count = 4096);
    let (result, images, _) = rig.update(4097, 0);
    result.unwrap();
    assert_eq!(get(&images[&AbsoluteBN::new(DOUBLE)], 0), 0);
}

#[test]
fn a_new_backup_group_gains_a_pointer_in_each_remaining_reserve() {
    // Group 5 is the first new group and has a backup; groups 1 and 3 already do.
    let rig = Rig::with(1, |fs| fs.group_count = 5);
    let (result, images, rig) = rig.update(6, 1);
    result.unwrap();
    let primary = rig.primary(1);
    let list = &images[&AbsoluteBN::new(primary)];
    let expected = group_first_block(&rig.fs.superblock, 5).unwrap() + 1 + 1;
    assert_eq!(u64::from(get(list, 2)), expected);
    assert_eq!(
        images.len(),
        2,
        "only the reserved primary and inode 7 change"
    );
    let inode = rig.staged_inode(&images);
    assert_eq!(inode.i_blocks_lo, (1 + 3 + 1) * 2);
}
