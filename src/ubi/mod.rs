//! This module implements UBI reformatting and reimaging operations. Notably, it also implements
//! the migration away from Allwinner's `SIMULATE_MULTIPLANE` in a UBI-aware manner.
//!
//! Allwinner's AWNAND driver includes a configuration option, `SIMULATE_MULTIPLANE`, that when
//! enabled changes the system's view of NAND:
//!
//! ```text
//! /======== Superblock 0 ========\   /======== Superblock 1 ========\
//! | +- Block 0 -+  +- Block 1 -+ |   | +- Block 2 -+  +- Block 3 -+ |
//! | |  Page  0  |++|  Page  0  | |   | |  Page  0  |++|  Page  0  | |
//! | |  Page  1  |++|  Page  1  | |   | |  Page  1  |++|  Page  1  | |   ...
//! | |    ...    |  |    ...    | |   | |    ...    |  |    ...    | |
//! | +-----------+  +-----------+ |   | +-----------+  +-----------+ |
//! \==============================/   \==============================/
//! ```
//!
//! The NAND is viewed as having half as many blocks, each with double the size. A larger, virtual
//! "superblock" N is created by concatenating block 2N and block 2N+1 in pagewise pairs, resulting
//! in superblocks having the same number of pages, but with the pages being twice as large (and
//! simulating "multiplane" operation, where one can write only one half of a page).
//!
//! This is not only incompatible with the ordinary `spi-nand` driver, it's actively a problem:
//! 1. Erases are amplified, because erasing a superblock requires erasing both physical blocks.
//! 2. Bad blocks are twice as impactful: if either physical block is bad, the whole superblock is
//!    lost.
//! 3. UBI underestimates the number of blocks that need to be reserved for bad block handling.
//! 4. We lose granularity on erasures, and thus have to do more needless rewriting of data.
//!
//! UBI populates the first writable page of every block with an "erase counter" ("EC") header, and
//! if the block is in use, a "volume ID" ("VID") header in the second (half-)page.
//! If `SIMULATE_MULTIPLANE` is active, this VID header will land in page 0 of an odd block, which
//! is how we detect that the migration is necessary. Rather than merely erase everything, try
//! to preserve ECs (per UBI docs), and copy the even-block EC values to the odd blocks as well.

mod format;
mod headers;
mod scan;
pub mod ubinize;

pub use format::{format, write_volumes};
pub use headers::VolType;
pub use scan::{scan_blocks, Ebt};
