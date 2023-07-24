//! This module implements logic to write raw blobs to NAND flash.

use crate::nand::{Nand, NandBlock, PageUtil};
use crate::util::ReadExt;

use std::io::Read;

/// Scan a block to confirm that its contents match the provided slice.
///
/// The provided slice should be no longer than the block contents. If it is shorter, the remaining
/// bytes are "don't care."
///
/// The return value is the number of pages of the block that match (and therefore the index of the
/// page where write can start), or None if there is no partial match and the block must be erased.
fn check_raw_block<B: NandBlock>(block: &B, mut data: &[u8]) -> Option<u32> {
    // How many pages do we read at a time? A higher number helps in high-latency situations.
    const PAGE_CHUNKS: u32 = 8;

    let mut buf = vec![0; block.page_size() * PAGE_CHUNKS as usize];
    let mut remaining: &[u8] = &[];

    let mut page: u32 = 0;
    let mut data_correct_upto: Option<u32> = None;
    loop {
        if data_correct_upto.is_none() && data.is_empty() {
            // No data mismatch found and no further data to compare
            break Some(page);
        }

        if remaining.is_empty() {
            buf.truncate(block.page_size() * (block.page_count() - page) as usize);

            if buf.is_empty() {
                // Nothing more to read
                break data_correct_upto;
            }

            if block.read(page, &mut buf).is_err() {
                // Read errors are considered "needs erase"
                break None;
            }
            remaining = &buf;
        }

        let (page_content, rem) = remaining.split_at(block.page_size());
        remaining = rem;

        // Still comparing data?
        if data_correct_upto.is_none() {
            let cmp_len = std::cmp::min(page_content.len(), data.len());
            if page_content[..cmp_len] == data[..cmp_len] {
                data = &data[cmp_len..];
            } else {
                // A mismatch means we're now looking for erased pages; if all others are erased,
                // this becomes the return value.
                data_correct_upto = Some(page);
            }
        }

        // Enforcing all further pages are erased?
        if data_correct_upto.is_some() {
            // A non-erased page after a data mismatch means an erase is required
            if !page_content.is_erased() {
                break None;
            }
        }

        page += 1;
    }
}

/// Update the specified block's contents, resuming from a partial write if possible.
///
/// The provided slice should be no longer than the block contents. If it is shorter, the remaining
/// bytes are "don't care."
fn update_raw_block<B: NandBlock>(block: &mut B, data: &[u8]) -> anyhow::Result<()> {
    let start_page = match check_raw_block(block, data) {
        None => {
            block.erase()?;
            0
        }
        Some(x) => x,
    };

    // Ensure `data` is a multiple of the page size
    let mut data_len = data.len() + block.page_size() - 1;
    data_len -= data_len % block.page_size();
    let mut vec;
    let mut data = data;
    if data_len != data.len() {
        // Not padded to a multiple of page size
        vec = Vec::with_capacity(data_len);
        vec.extend(data);
        vec.resize(data_len, 0xFF);
        data = &vec[..];
    }

    block.program(start_page, data)
}

/// Write a raw blob to the NAND flash device.
///
/// This operation is idempotent; if the image is already written, no erase/writes will occur.
///
/// The `skip_bad` parameter will cause bad blocks to be skipped over. If this is `false`,
/// encountering a bad block is an error.
pub fn write_raw_image<N: Nand, R: Read>(
    nand: &mut N,
    image: &mut R,
    skip_bad: bool,
) -> anyhow::Result<()> {
    let block_size = nand.get_layout().pages_per_block as usize * nand.get_layout().bytes_per_page;

    let mut data = Vec::with_capacity(block_size);
    let mut block_index: u32 = 0;
    loop {
        data.clear();
        image.read_to_vec(&mut data, block_size)?;
        if data.is_empty() {
            // EOF encountered means the write is complete
            break Ok(());
        }

        'find_block_and_write: loop {
            let block = nand.block(block_index)?;
            block_index += 1;

            if let Some(mut block) = block {
                // Give 5 attempts to update it
                for _ in 0..5 {
                    match update_raw_block(&mut block, &data) {
                        Ok(()) => break 'find_block_and_write,
                        Err(_) => block.erase()?,
                    }
                }

                // Block must have gone bad
                block.mark_bad()?;
            }

            // Block is bad; if we can't tolerate it, bail. Otherwise, loop to find a good one.
            anyhow::ensure!(skip_bad, "unhandled bad block encountered");
        }
    }
}

#[test]
fn test_check_raw_block() -> anyhow::Result<()> {
    use crate::nand::{NandLayout, SimNand};

    const TEST_LAYOUT: NandLayout = NandLayout {
        blocks: 1,
        pages_per_block: 43,
        bytes_per_page: 128,
    };

    let mut nand = SimNand::new(TEST_LAYOUT);
    let mut block = nand.block(0)?.unwrap();

    assert_eq!(check_raw_block(&block, &[]), Some(0));
    assert_eq!(check_raw_block(&block, &[0xFF, 0xFF]), Some(1));
    assert_eq!(check_raw_block(&block, &[0xFF, 0x7F]), Some(0));

    // Generate a bunch of test data: 10 pages of data, 20 empty, 10 more of data
    let mut test_data: [u8; 128 * 40] = std::array::from_fn(|i| match i / 128 {
        0..=9 => (i * 23) as u8,
        30..=39 => (i * 11) as u8,
        _ => 0xFF,
    });

    // Program only first 35 blocks
    block.program(0, &test_data[..35 * 128])?;

    assert_eq!(check_raw_block(&block, &test_data[..128 * 5]), Some(5));
    assert_eq!(check_raw_block(&block, &test_data[..128 * 15]), Some(15));
    assert_eq!(check_raw_block(&block, &test_data), Some(35));

    test_data[25 * 128] = 0x00;
    assert_eq!(check_raw_block(&block, &test_data), None);

    Ok(())
}

#[test]
fn test_update_raw_block() -> anyhow::Result<()> {
    use crate::nand::{NandLayout, SimNand};

    const TEST_LAYOUT: NandLayout = NandLayout {
        blocks: 1,
        pages_per_block: 43,
        bytes_per_page: 128,
    };

    let mut nand = SimNand::new(TEST_LAYOUT);
    let mut block = nand.block(0)?.unwrap();

    update_raw_block(&mut block, &[])?;
    update_raw_block(&mut block, &[0xAA])?;

    Ok(())
}
