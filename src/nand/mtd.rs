//! NAND abstraction layer implementation over the Linux MTD subsystem

use super::{Nand, NandBlock, NandLayout};

use anyhow::{bail, ensure};

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::mem::MaybeUninit;
use std::os::{fd::AsRawFd, unix::fs::FileExt};
use std::path::Path;

/// NAND flash that wraps an open /dev/mtdX file
#[derive(Debug)]
pub struct MtdNand {
    file: File,
    layout: NandLayout,
}

impl MtdNand {
    /// Open an `mtd` device, by path (e.g. "/dev/mtd0")
    pub fn open<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let file = File::options().read(true).write(true).open(path)?;
        let layout = unsafe {
            let mut info = MaybeUninit::<ioctl::mtd_info_user>::uninit();
            ioctl::memgetinfo(file.as_raw_fd(), info.as_mut_ptr())?;
            info.assume_init()
        }
        .try_into()?;

        Ok(Self { file, layout })
    }

    /// Open an `mtd` device by its name, by searching `/proc/mtd`
    pub fn open_named(name: &str) -> anyhow::Result<Self> {
        // Put `name` in quotes
        let name = format!("\"{name}\"");

        let proc_mtd = File::open("/proc/mtd")?;
        let proc_mtd = BufReader::new(proc_mtd);
        for line in proc_mtd.lines() {
            let line = line?;
            if line.contains(&name) {
                let mtd_dev = line.split(':').next().unwrap();
                return Self::open(Path::new("/dev").join(mtd_dev));
            }
        }

        bail!("MTD device {name} could not be found");
    }
}

impl Nand for MtdNand {
    type Block<'a> = MtdBlock<'a>;

    fn block(&mut self, index: u32) -> anyhow::Result<Option<MtdBlock<'_>>> {
        ensure!(index < self.layout.blocks, "block {index} out of range");

        let block_size = self.layout.pages_per_block * self.layout.bytes_per_page as u32;
        let block_base: u64 = (block_size * index) as u64;
        let bad = unsafe { ioctl::memgetbadblock(self.file.as_raw_fd(), &block_base)? };
        if bad == 0 {
            Ok(Some(MtdBlock { nand: self, index }))
        } else {
            Ok(None)
        }
    }

    fn get_layout(&self) -> NandLayout {
        self.layout
    }
}

pub struct MtdBlock<'a> {
    nand: &'a MtdNand,
    index: u32,
}

impl MtdBlock<'_> {
    /// Compute the number of bytes in this block
    fn size(&self) -> u32 {
        self.nand.layout.pages_per_block * self.nand.layout.bytes_per_page as u32
    }

    /// Compute the offset of the first byte of this block
    fn base(&self) -> u32 {
        self.size() * self.index
    }

    /// Ensure that the byte count and starting page range is valid, and compute the /dev/mtdX
    /// offset for the page
    fn offset_for(&self, start_page: u32, bytes: usize) -> anyhow::Result<u64> {
        ensure!(
            bytes % self.page_size() == 0,
            "buffer not multiple of page size"
        );

        let end_page = start_page + (bytes / self.page_size()) as u32;
        ensure!(
            end_page <= self.page_count(),
            "block {0}, page range {start_page}..{end_page} out of bounds",
            self.index
        );

        Ok((self.base() + self.page_size() as u32 * start_page) as u64)
    }
}

impl NandBlock for MtdBlock<'_> {
    fn page_count(&self) -> u32 {
        self.nand.layout.pages_per_block
    }
    fn page_size(&self) -> usize {
        self.nand.layout.bytes_per_page
    }
    fn read(&self, start_page: u32, content: &mut [u8]) -> anyhow::Result<()> {
        let offset = self.offset_for(start_page, content.len())?;
        Ok(self.nand.file.read_exact_at(content, offset)?)
    }
    fn program(&mut self, start_page: u32, content: &[u8]) -> anyhow::Result<()> {
        let offset = self.offset_for(start_page, content.len())?;
        Ok(self.nand.file.write_all_at(content, offset)?)
    }
    fn erase(&mut self) -> anyhow::Result<()> {
        let erase_info = ioctl::erase_info_user {
            start: self.base(),
            length: self.size(),
        };
        unsafe {
            ioctl::memerase(self.nand.file.as_raw_fd(), &erase_info)?;
        }
        Ok(())
    }
    fn mark_bad(self) -> anyhow::Result<()> {
        let block_base: u64 = self.base() as u64;
        unsafe {
            ioctl::memsetbadblock(self.nand.file.as_raw_fd(), &block_base)?;
        }
        Ok(())
    }
}

mod ioctl {
    //! The private ioctls for interfacing with MTD devices

    use super::NandLayout;

    use anyhow::ensure;
    use nix::{ioctl_read, ioctl_write_ptr};

    const MTD_IOC_MAGIC: u8 = b'M';

    #[repr(C)]
    pub struct mtd_info_user {
        pub r#type: u8,
        pub flags: u32,
        pub size: u32,
        pub erasesize: u32,
        pub writesize: u32,
        pub oobsize: u32,
        pub padding: u64,
    }
    ioctl_read!(memgetinfo, MTD_IOC_MAGIC, 1, mtd_info_user);

    impl TryInto<NandLayout> for mtd_info_user {
        type Error = anyhow::Error;

        fn try_into(mut self) -> anyhow::Result<NandLayout> {
            if self.writesize == 1 {
                // Hack for debugging on mtdram devices
                self.writesize = 64;
            }

            ensure!(
                self.size % self.erasesize == 0,
                "MTD size not multiple of erasesize"
            );
            ensure!(
                self.erasesize % self.writesize == 0,
                "MTD erasesize not multiple of writesize"
            );

            let blocks = self.size / self.erasesize;
            let pages_per_block = self.erasesize / self.writesize;
            let bytes_per_page = self.writesize as usize;

            Ok(NandLayout {
                blocks,
                pages_per_block,
                bytes_per_page,
            })
        }
    }

    #[repr(C)]
    pub struct erase_info_user {
        pub start: u32,
        pub length: u32,
    }
    ioctl_write_ptr!(memerase, MTD_IOC_MAGIC, 2, erase_info_user);

    ioctl_write_ptr!(memgetbadblock, MTD_IOC_MAGIC, 11, u64);
    ioctl_write_ptr!(memsetbadblock, MTD_IOC_MAGIC, 12, u64);
}
