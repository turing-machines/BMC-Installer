//! Abstractions and code to access NAND flash

use std::io::{Read, Write};
use std::str::FromStr;

use anyhow::ensure;

#[cfg(target_os = "linux")]
pub mod mtd;

/// Convenience methods for operating on `[u8]`s that represent page contents
pub trait PageUtil {
    /// Does this page contain the all-1s bit pattern?
    fn is_erased(&self) -> bool;
}

impl PageUtil for [u8] {
    fn is_erased(&self) -> bool {
        self.iter().all(|&x| x == 0xFF)
    }
}

/// A pub-fields struct describing the data layout of a NAND flash device
#[derive(Debug, Copy, Clone)]
pub struct NandLayout {
    pub blocks: u32,
    pub pages_per_block: u32,
    pub bytes_per_page: usize,
}

/// Parse strings like "BLOCKSxPAGESxBYTES"
impl FromStr for NandLayout {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let [blocks, pages_per_block, bytes_per_page]: [&str; 3] = s
            .split('x')
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected #x#x#"))?;
        let blocks = blocks.parse()?;
        let pages_per_block = pages_per_block.parse()?;
        let bytes_per_page = bytes_per_page.parse()?;

        Ok(NandLayout {
            blocks,
            pages_per_block,
            bytes_per_page,
        })
    }
}

/// Represents a NAND flash device
pub trait Nand {
    type Block<'a>: NandBlock + 'a
    where
        Self: 'a;

    /// Get a block
    ///
    /// Returns None if `index` refers to a block marked bad
    fn block(&mut self, index: u32) -> anyhow::Result<Option<Self::Block<'_>>>;

    /// Get the layout of the NAND
    fn get_layout(&self) -> NandLayout;
}

/// Represents a block of a NAND flash device
pub trait NandBlock {
    /// How many pages in this block?
    fn page_count(&self) -> u32;

    /// How many bytes per page?
    fn page_size(&self) -> usize;

    /// Read an integral number of pages, starting at the specified page
    fn read(&self, start_page: u32, content: &mut [u8]) -> anyhow::Result<()>;

    /// Write the specified content, beginning at the specified page
    ///
    /// Note that `index` must be greater than any previously-written index, or in other words,
    /// writing a page makes it and all skipped pages nonwritable. This is to comply with the
    /// sequential-write requirements of certain MLC NANDs.
    fn program(&mut self, start_page: u32, content: &[u8]) -> anyhow::Result<()>;

    /// Erase a block, making all pages writable again
    fn erase(&mut self) -> anyhow::Result<()>;

    /// Marks the block as bad, consuming the block object (it cannot be retrieved again).
    ///
    /// This should be called if an erase() results in error, or if a (properly in-order) program()
    /// results in error and we have already tried erase() and reprogramming it.
    fn mark_bad(self) -> anyhow::Result<()>;
}

/// A simulated in-memory NAND flash, for testing purposes
#[derive(Debug, Clone)]
pub struct SimNand {
    blocks: Box<[SimBlock]>,
    layout: NandLayout,
}

/// A block of SimNand
#[derive(Debug, Clone)]
pub struct SimBlock {
    /// All bytes of all written pages (legally, can only append to this)
    data: Vec<u8>,

    /// How many pages in this block
    page_count: u32,

    /// How many bytes per page
    page_size: usize,

    /// Is this block marked bad?
    marked_bad: bool,
}

impl SimNand {
    /// Create an empty SimNand with the specified layout
    pub fn new(layout: NandLayout) -> Self {
        let blocks = vec![SimBlock::new(layout); layout.blocks as usize];
        let blocks = blocks.into_boxed_slice();

        Self { blocks, layout }
    }

    /// Initialize the NAND contents with content read from a type implementing `Read`.
    pub fn load<R: Read>(&mut self, read: &mut R) -> anyhow::Result<()> {
        let size = self.layout.bytes_per_page * self.layout.pages_per_block as usize;
        let mut buf = vec![0; size];

        for block in 0..self.layout.blocks {
            let mut block = &mut self.blocks[block as usize];
            block.marked_bad = false;
            read.read_exact(&mut buf)?;
            block.program(0, &buf)?;
        }

        Ok(())
    }

    /// Write the contents of this simulated NAND block out to a writable stream (such as a File)
    pub fn save<W: Write>(&mut self, write: &mut W) -> anyhow::Result<()> {
        let size = self.layout.bytes_per_page * self.layout.pages_per_block as usize;
        let mut buf = vec![0; size];

        for block in 0..self.layout.blocks {
            match self.block(block)? {
                None => buf.fill(0xBD),
                Some(block) => block.read(0, &mut buf)?,
            };

            write.write_all(&buf)?;
        }

        Ok(())
    }
}

impl SimBlock {
    /// Construct an empty block within the given layout
    fn new(layout: NandLayout) -> Self {
        Self {
            data: Default::default(),
            page_count: layout.pages_per_block,
            page_size: layout.bytes_per_page,
            marked_bad: false,
        }
    }

    fn write_page(&mut self, index: u32, content: &[u8]) -> anyhow::Result<()> {
        ensure!(content.len() == self.page_size, "content not page-sized");
        ensure!(index < self.page_count, "page index out of bounds");

        let begin = index as usize * self.page_size;

        ensure!(begin >= self.data.len(), "write in already-written area");

        // Writing fully-erased content is a no-op.
        if !content.is_erased() {
            self.data.resize(begin, 0xFF);
            self.data.extend_from_slice(content);
        }

        Ok(())
    }

    fn read_page(&self, index: u32, content: &mut [u8]) -> anyhow::Result<()> {
        ensure!(content.len() == self.page_size, "content not page-sized");
        ensure!(index < self.page_count, "page index out of bounds");

        let begin = index as usize * self.page_size;
        let end = begin + self.page_size;

        if let Some(page) = self.data.get(begin..end) {
            content.copy_from_slice(page);
        } else {
            content.fill(0xFF);
        }

        Ok(())
    }
}

impl Nand for SimNand {
    type Block<'a> = &'a mut SimBlock;

    fn block(&mut self, index: u32) -> anyhow::Result<Option<Self::Block<'_>>> {
        self.blocks
            .get_mut(index as usize)
            .ok_or(anyhow::anyhow!("block {index} out of range"))
            .map(|x| Some(x).filter(|y| !y.marked_bad))
    }

    fn get_layout(&self) -> NandLayout {
        self.layout
    }
}

impl NandBlock for &mut SimBlock {
    fn page_count(&self) -> u32 {
        self.page_count
    }
    fn page_size(&self) -> usize {
        self.page_size
    }

    fn read(&self, start_page: u32, content: &mut [u8]) -> anyhow::Result<()> {
        let mut page = start_page;
        for chunk in content.chunks_mut(self.page_size()) {
            self.read_page(page, chunk)?;
            page += 1;
        }
        Ok(())
    }

    fn program(&mut self, start_page: u32, content: &[u8]) -> anyhow::Result<()> {
        let mut page = start_page;
        for chunk in content.chunks(self.page_size()) {
            self.write_page(page, chunk)?;
            page += 1;
        }
        Ok(())
    }

    fn erase(&mut self) -> anyhow::Result<()> {
        self.data.clear();

        Ok(())
    }

    fn mark_bad(mut self) -> anyhow::Result<()> {
        self.erase()?;
        self.marked_bad = true;
        Ok(())
    }
}

#[cfg(test)]
const TEST_LAYOUT: NandLayout = NandLayout {
    blocks: 8,
    pages_per_block: 16,
    bytes_per_page: 256,
};

#[test]
fn test_sim_block() {
    let mut nand = SimNand::new(TEST_LAYOUT);
    assert!(nand.block(0).unwrap().is_some());
    assert!(nand.block(TEST_LAYOUT.blocks - 1).unwrap().is_some());
    assert!(nand.block(TEST_LAYOUT.blocks).is_err());
}

#[test]
fn test_sim_mark_bad() {
    let mut nand = SimNand::new(TEST_LAYOUT);
    assert!(nand.block(0).unwrap().is_some());
    nand.block(0).unwrap().unwrap().mark_bad().unwrap();
    assert!(nand.block(0).unwrap().is_none());
}

#[test]
fn test_sim_read_write() {
    let mut nand = SimNand::new(TEST_LAYOUT);

    let data_in = vec![0xA5u8; nand.get_layout().bytes_per_page];
    let mut data_out = data_in.clone();

    let mut block = nand.block(0).unwrap().unwrap();
    block.program(2, &data_in).unwrap();
    assert!(block.program(1, &data_in).is_err());

    block.read(1, &mut data_out).unwrap();
    assert!(data_out.is_erased());

    block.read(2, &mut data_out).unwrap();
    assert_eq!(data_out, data_in);

    block.read(3, &mut data_out).unwrap();
    assert!(data_out.is_erased());
}

#[test]
fn test_sim_load() {
    let mut nand = SimNand::new(TEST_LAYOUT);
    nand.load(&mut std::io::repeat(0x55u8)).unwrap();

    let mut buf =
        vec![0u8; nand.get_layout().bytes_per_page * nand.get_layout().pages_per_block as usize];

    let block = nand.block(0).unwrap().unwrap();
    block.read(0, &mut buf).unwrap();

    assert!(buf.iter().all(|&x| x == 0x55u8));
}
