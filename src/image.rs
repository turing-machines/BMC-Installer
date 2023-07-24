//! Utilities for working with images.
//!
//! Currently just a function to determine the meaningful size of some EROFS partition.

use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;

use crc::{Algorithm, Crc, CRC_32_ISCSI};
const CRC_32_EROFS: Algorithm<u32> = Algorithm {
    xorout: 0,
    ..CRC_32_ISCSI
};
const EROFS_CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_EROFS);

const EROFS_SUPER_OFFSET: u64 = 1024;
const EROFS_SUPER_SIZE: usize = 4096 - EROFS_SUPER_OFFSET as usize;

const EROFS_SUPER_POS_MAGIC: usize = 0;
const EROFS_SUPER_POS_CKSUM: usize = 4;
const EROFS_SUPER_POS_BLKSZBITS: usize = 12;
const EROFS_SUPER_POS_BLOCKS: usize = 36;

const EROFS_SUPER_MAGIC_V1: u32 = 0xE0F5E1E2;

/// Given an open EROFS image (or partition), determine its total size in bytes.
pub fn erofs_size<F: Read + Seek>(input: &mut F) -> anyhow::Result<u64> {
    let mut superblock: [u8; EROFS_SUPER_SIZE] = [0; EROFS_SUPER_SIZE];
    input.seek(SeekFrom::Start(EROFS_SUPER_OFFSET))?;
    input.read_exact(&mut superblock)?;
    input.seek(SeekFrom::Start(0))?;

    let magic = u32::from_le_bytes(
        superblock[EROFS_SUPER_POS_MAGIC..][..size_of::<u32>()]
            .try_into()
            .unwrap(),
    );
    anyhow::ensure!(magic == EROFS_SUPER_MAGIC_V1, "EROFS filesystem not found");

    let cksum = u32::from_le_bytes(
        superblock[EROFS_SUPER_POS_CKSUM..][..size_of::<u32>()]
            .try_into()
            .unwrap(),
    );
    superblock[EROFS_SUPER_POS_CKSUM..][..size_of::<u32>()].fill(0u8);
    anyhow::ensure!(
        cksum == EROFS_CRC.checksum(&superblock),
        "EROFS superblock is corrupt",
    );

    let blocks = u32::from_le_bytes(
        superblock[EROFS_SUPER_POS_BLOCKS..][..size_of::<u32>()]
            .try_into()
            .unwrap(),
    );
    let blkszbits = superblock[EROFS_SUPER_POS_BLKSZBITS];

    u64::from(blocks)
        .checked_shl(blkszbits.into())
        .ok_or(anyhow::anyhow!("Overflow in computing EROFS image size"))
}
