//! The 1.0.x firmware series had a very different NAND layout:
//!
//! -   1 MiB: boot0
//! -   4 MiB: boot package, secure data area
//! - 123 MiB: UBI partition, with SIMULATE_MULTIPLANE enabled
//!
//! This module implements:
//! 1. If an Allwinner boot0 is detected in the `boot` partition, erase it, so that it doesn't
//!    conflict with the U-Boot SPL.
//! 2. General flash-writing code that can write raw images to the NAND.
//!
//! These steps are meant to be idempotent and no-ops on post-migrated NAND layouts, so they should
//! always run unconditionally as part of the installation process.

pub mod raw;
use crate::nand::{Nand, NandBlock};

/// Scan each block looking for an Allwinner boot0 header, and erase the found blocks.
///
/// Returns `true` if any blocks were erased.
pub fn purge_boot0<N: Nand>(nand: &mut N) -> anyhow::Result<bool> {
    let mut any_erased = false;

    let mut page_buf = vec![0; nand.get_layout().bytes_per_page];
    for block_index in 0..nand.get_layout().blocks {
        if let Some(mut block) = nand.block(block_index)? {
            block.read(0, &mut page_buf)?;
            if is_boot0(&page_buf) == Some(true) {
                block.erase()?;
                any_erased = true;
            }
        }
    }

    Ok(any_erased)
}

/// Scan a buffer and determine if this is an Allwinner boot0 header.
///
/// This is careful not to detect U-Boot SPL headers, which are formatted very similarly.
fn is_boot0(buffer: &[u8]) -> Option<bool> {
    // Check for the BT0 magic
    if buffer.get(0x04..0x0c)? != b"eGON.BT0" {
        return Some(false);
    }

    // Check if this is actually a U-Boot SPL
    if buffer.get(0x14..0x17)? == b"SPL" {
        return Some(false);
    }

    Some(true)
}
