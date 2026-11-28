//! Tests that pin the bounds of the directory block and dx tail checksums.

use super::*;
use alloc::vec::Vec;

const INO: u32 = 704_258;
const GENERATION: u32 = 7_817_325;

/// A directory data block of `len` bytes whose last 12 bytes are a valid tail.
fn leaf_block(len: usize) -> Vec<u8> {
    let mut block = alloc::vec![0x5A; len];
    Ext4DirEntryTail::new().to_disk_bytes(&mut block[len - 12..]);
    update_ext4_dirblock_csum32(&metadata_csum_superblock(), INO, GENERATION, &mut block);
    block
}

/// A dx node of `len` bytes, whose single record spans the block, with
/// `limit` and `count` at byte 8.
fn dx_node(len: usize, limit: u16, count: u16) -> Vec<u8> {
    let mut block = alloc::vec![0; len];
    if len >= 12 {
        block[8..10].copy_from_slice(&limit.to_le_bytes());
        block[10..12].copy_from_slice(&count.to_le_bytes());
    }
    block
}

/// A dx root of `len` bytes, with a 12-byte first record, an 8-byte root
/// information record at byte 24, and `limit` and `count` at byte 32.
fn dx_root(len: usize, limit: u16, count: u16) -> Vec<u8> {
    let mut block = alloc::vec![0; len];
    block[4..6].copy_from_slice(&12u16.to_le_bytes());
    block[29] = 8;
    if len >= 36 {
        block[32..34].copy_from_slice(&limit.to_le_bytes());
        block[34..36].copy_from_slice(&count.to_le_bytes());
    }
    block
}

fn update_dx(block: &mut [u8]) -> bool {
    update_ext4_dx_checksum(&metadata_csum_superblock(), INO, GENERATION, block)
}

fn verify_dx(block: &[u8]) -> Option<bool> {
    verify_ext4_dx_checksum(&metadata_csum_superblock(), INO, GENERATION, block)
}

#[test]
fn a_directory_block_that_is_only_its_tail_is_checksummed() {
    let sb = metadata_csum_superblock();
    let block = leaf_block(12);
    let expected = ext4_dirblock_csum32(&sb, INO, GENERATION, &[]);
    assert_eq!(block[8..12], expected.to_le_bytes());
    assert!(verify_ext4_dirblock_checksum(&sb, INO, GENERATION, &block));
}

#[test]
fn a_directory_block_shorter_than_a_tail_is_left_alone() {
    let mut block = [0x5A; 8];
    update_ext4_dirblock_csum32(&metadata_csum_superblock(), INO, GENERATION, &mut block);
    assert_eq!(block, [0x5A; 8]);
}

#[test]
fn each_field_of_the_tail_is_checked_alone() {
    let sb = metadata_csum_superblock();
    // The checksum covers only the bytes before the tail, so each change below
    // leaves it valid and the field check alone must refuse the block.
    for (offset, value) in [(0, 1), (4, 16), (6, 1), (7, 0)] {
        let mut block = leaf_block(BLOCK_SIZE);
        block[BLOCK_SIZE - 12 + offset] = value;
        assert!(
            !verify_ext4_dirblock_checksum(&sb, INO, GENERATION, &block),
            "tail byte {offset} set to {value} must be refused"
        );
    }
}

#[test]
fn a_tail_that_ends_the_block_gets_its_checksum() {
    let sb = metadata_csum_superblock();
    let mut block = alloc::vec![0x5A; 64];
    let tail = block.len() - 12;
    ext4_update_dirblock_tail_checksum(&sb, INO, GENERATION, &mut block, tail);
    let expected = ext4_dirblock_csum32(&sb, INO, GENERATION, &[0x5A; 52]);
    assert_eq!(block[60..], expected.to_le_bytes());
    assert_eq!(block[..60], [0x5A; 60], "only the checksum field changes");
}

#[test]
fn a_dx_node_with_one_entry_gets_a_checksum_its_reader_accepts() {
    let mut block = dx_node(24, 1, 1);
    block[12..20].fill(0xA5);
    let before = block.clone();
    assert!(update_dx(&mut block));
    assert_eq!(block[..20], before[..20], "only the checksum field changes");
    assert_ne!(block[20..24], [0; 4]);
    assert_eq!(verify_dx(&block), Some(true));
}

#[test]
fn a_dx_root_with_one_entry_gets_a_checksum_its_reader_accepts() {
    let mut block = dx_root(48, 1, 1);
    assert!(update_dx(&mut block));
    assert_ne!(block[44..48], [0; 4]);
    assert_eq!(verify_dx(&block), Some(true));
}

#[test]
fn a_dx_block_with_no_entries_is_not_checksummed() {
    let mut block = dx_node(24, 1, 0);
    assert!(!update_dx(&mut block));
    assert_eq!(block, dx_node(24, 1, 0));
}

#[test]
fn a_dx_limit_whose_tail_passes_the_block_is_not_checksummed() {
    let mut block = dx_node(24, 2, 1);
    assert!(!update_dx(&mut block));
    assert_eq!(block, dx_node(24, 2, 1));
}

#[test]
fn a_dx_block_too_short_for_its_count_and_limit_is_not_checksummed() {
    for len in [8, 11, 12] {
        let mut block = dx_node(len, 0, 0);
        assert!(
            !update_dx(&mut block),
            "{len} bytes hold no count, limit and tail"
        );
    }
    let mut block = [0u8; 4];
    assert!(!update_dx(&mut block), "4 bytes hold no record length");
}

#[test]
fn a_dx_root_of_exactly_the_root_header_is_read_as_a_root() {
    // The count and limit would run past a 32-byte root, so it is refused
    // rather than not being recognized as a dx block at all.
    assert_eq!(verify_dx(&dx_root(32, 0, 0)), Some(false));
}

#[test]
fn a_twelve_byte_first_record_in_a_block_shorter_than_a_root_is_no_dx_block() {
    let mut block = alloc::vec![0; 24];
    block[4..6].copy_from_slice(&12u16.to_le_bytes());
    assert_eq!(verify_dx(&block), None);
}

#[test]
fn each_byte_of_the_reserved_root_word_is_checked() {
    for offset in 24..28 {
        let mut block = dx_root(48, 1, 1);
        assert!(update_dx(&mut block));
        block[offset] = 1;
        assert_eq!(
            verify_dx(&block),
            None,
            "reserved byte {offset} must be zero"
        );
    }
}
