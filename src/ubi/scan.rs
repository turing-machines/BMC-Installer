//! This module contains code to scan NAND blocks and determine their contents (per UBI).

use super::headers::*;
use crate::nand::{Nand, NandBlock, PageUtil};

/// These are the states that a given block may be detected in
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum BlockContent {
    /// The block is bad, and cannot be accessed
    Bad,

    /// The block is fully erased, perhaps because UBI has never used it
    Erased,

    /// The block is erased, but has a UBI EC header that should be preserved
    EcErased(Ec),

    /// The block is in normal use, with a UBI EC header that should be preserved
    EcData(Ec, Option<Vid>),

    /// The block starts with a UBI VID header, due to AWNAND's interleaving of UBI's writes
    ///
    /// This is useful for detecting SIMULATE_MULTPLANE, but should otherwise be treated
    /// identically to Garbage
    RawVid(Vid),

    /// The block is in some other (invalid, per UBI) state, and needs to be erased
    Garbage,
}

impl BlockContent {
    /// Read a NAND block and characterize its content
    fn scan_block<B: NandBlock>(block: &B) -> anyhow::Result<Self> {
        // How many pages do we read at a time? A higher number helps in high-latency situations.
        const PAGE_CHUNKS: u32 = 4;

        let mut buf = vec![0; block.page_size() * PAGE_CHUNKS as usize];

        let mut echdr: Option<Ec> = None;
        for start_page in (0..block.page_count()).step_by(PAGE_CHUNKS as usize) {
            if echdr.is_some() {
                // Optimization: If we have found an EC header, but we're still looping, it means
                // the first few pages were [EC, erased, ...], so we can probably just assume the
                // rest of the pages are erased.
                break;
            }

            // Clip the buffer down to the size of the page(s) read on this iteration
            let end_page = std::cmp::min(block.page_count(), start_page + PAGE_CHUNKS);
            let buf = &mut buf[..block.page_size() * (end_page - start_page) as usize];

            // Read pages `start_page..end_page`
            block.read(start_page, buf)?;

            for (page, page_bytes) in
                (start_page..end_page).zip(buf.chunks_exact(block.page_size()))
            {
                if page == 0 {
                    if let Some(hdr) = Vid::decode(page_bytes) {
                        return Ok(Self::RawVid(hdr));
                    } else if let Some(hdr) = Ec::decode(page_bytes) {
                        echdr = Some(hdr);
                        continue;
                    }
                }

                // Not first page, or first page doesn't contain a UBI header, so this loop is now
                // finding out if the block is fully-erased.
                if !page_bytes.is_erased() {
                    let vid = match page {
                        1 => Vid::decode(page_bytes),
                        _ => None,
                    };

                    // Non-erased page found means this block is in use
                    return Ok(echdr.map_or(Self::Garbage, |x| Self::EcData(x, vid)));
                }
            }
        }

        // If we got out of the loop, we didn't encounter any data pages, so it's erased
        Ok(echdr.map_or(Self::Erased, Self::EcErased))
    }
}

/// The (E)rase(b)lock (t)able. A map of the current state of the NAND flash as determined by
/// [scan_blocks], which should be kept up-to-date as other operations are performed on flash.
pub type Ebt = Box<[BlockContent]>;

/// Read all blocks of the NAND (only as much as necessary to determine content), return the [Ebt]
pub fn scan_blocks<N: Nand>(nand: &mut N) -> anyhow::Result<Ebt> {
    let block_count = nand.get_layout().blocks;
    let rpt = howudoin::new()
        .label("Scanning blocks")
        .set_len(u64::from(block_count));

    // Grr, try_collect() isn't stable yet, so:
    let mut ebt = Vec::with_capacity(block_count as usize);
    for block_result in (0..block_count).map(|n| {
        nand.block(n)?
            .as_ref()
            .map_or(Ok(BlockContent::Bad), BlockContent::scan_block)
    }) {
        rpt.inc();
        ebt.push(block_result?);
    }

    rpt.close();

    Ok(ebt.into())
}

#[test]
fn test_scan() -> anyhow::Result<()> {
    use crate::nand::{NandLayout, SimNand};

    const TEST_LAYOUT: NandLayout = NandLayout {
        blocks: 16,
        pages_per_block: 16,
        bytes_per_page: 128,
    };

    let mut nand = SimNand::new(TEST_LAYOUT);

    // Confirm that, on a fresh NAND, every block scans as "erased"
    let blocks = scan_blocks(&mut nand)?;
    assert_eq!(blocks.len(), nand.get_layout().blocks as usize);
    assert!(blocks.into_iter().all(|&x| x == BlockContent::Erased));

    // Now modify several blocks for various states:
    use BlockContent::*;
    let desired_content = [
        Bad,
        Erased,
        EcErased(Default::default()),
        EcData(Default::default(), None),
        RawVid(Default::default()),
        Garbage,
        EcErased(Default::default()),
        Erased,
        Garbage,
        EcData(Default::default(), Some(Default::default())),
        Erased,
        Bad,
        RawVid(Default::default()),
    ];

    let mut buf = vec![0; nand.get_layout().bytes_per_page];
    for (i, content) in desired_content.iter().enumerate() {
        let mut block = nand.block(i as u32)?.unwrap();
        match content {
            Bad => block.mark_bad()?,
            Erased => block.erase()?,
            EcErased(ec) => {
                ec.encode(&mut buf)?;
                block.program(0, &buf)?;
            }
            EcData(ec, vid) => {
                ec.encode(&mut buf)?;
                block.program(0, &buf)?;
                if let Some(vid) = vid {
                    vid.encode(&mut buf)?;
                    block.program(1, &buf)?;
                }
                buf.fill(0xAA);
                block.program(i as u32, &buf)?;
            }
            RawVid(vid) => {
                vid.encode(&mut buf)?;
                block.program(0, &buf)?;
            }
            Garbage => {
                buf.fill(0xAA);
                block.program(i as u32, &buf)?;
            }
        }
    }

    // Now scan it again
    let blocks = scan_blocks(&mut nand)?;
    assert_eq!(blocks[..desired_content.len()], desired_content);

    Ok(())
}
