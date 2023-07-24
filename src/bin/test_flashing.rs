//! A test program for the reflashing functionality.
//!
//! This is not a unit test because it's meant to be used interactively on a real Linux system so
//! that its UBI implementation can be used to check our work.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use std::fs::File;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use bmc_installer::nand::mtd::MtdNand;
use bmc_installer::{
    format::{purge_boot0, raw::write_raw_image},
    nand::{NandLayout, SimNand},
    ubi::{
        format, scan_blocks,
        ubinize::{BasicVolume, Volume},
        write_volumes, Ebt, VolType,
    },
};

#[derive(Args, Debug)]
#[group(required = true)]
struct NandOptions {
    /// Name of the MTD device or partition
    #[cfg(target_os = "linux")]
    #[clap(long, group = "nand-options")]
    mtd_name: Option<String>,

    /// Path to a `/dev/mtdX` device
    #[cfg(target_os = "linux")]
    #[clap(long, group = "nand-options")]
    mtd_dev: Option<PathBuf>,

    /// Path to the NAND image to use
    #[clap(long, group = "nand-options", requires = "sim_layout")]
    sim_path: Option<PathBuf>,

    /// Layout of the NAND to simulate
    #[clap(long)]
    sim_layout: Option<NandLayout>,

    /// Write back the NAND file when done
    #[clap(long, requires = "sim_path")]
    sim_write: bool,
}

impl NandOptions {
    fn open(&self) -> Result<NandImpl> {
        let nandimpl = if let Some(layout) = self.sim_layout {
            let mut sim = SimNand::new(layout);
            if let Some(path) = &self.sim_path {
                sim.load(&mut File::open(path)?)?;
            }

            NandImpl::Sim(sim)
        } else {
            #[cfg(target_os = "linux")]
            {
                let mtd = {
                    if let Some(name) = &self.mtd_name {
                        MtdNand::open_named(name)?
                    } else if let Some(dev) = &self.mtd_dev {
                        MtdNand::open(dev)?
                    } else {
                        unreachable!()
                    }
                };

                NandImpl::Mtd(mtd)
            }

            #[cfg(not(target_os = "linux"))]
            unreachable!()
        };

        Ok(nandimpl)
    }

    fn cleanup(&self, nand: NandImpl) -> anyhow::Result<()> {
        if self.sim_write {
            if let Some(path) = &self.sim_path {
                if let NandImpl::Sim(mut sim_nand) = nand {
                    sim_nand.save(&mut File::create(path)?)?;
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
enum NandImpl {
    Sim(SimNand),

    #[cfg(target_os = "linux")]
    Mtd(MtdNand),
}

impl NandImpl {
    fn do_scan(&mut self) -> anyhow::Result<Ebt> {
        match self {
            NandImpl::Sim(nand) => scan_blocks(nand),

            #[cfg(target_os = "linux")]
            NandImpl::Mtd(nand) => scan_blocks(nand),
        }
    }

    fn do_format(&mut self, ebt: &mut Ebt) -> anyhow::Result<()> {
        match self {
            Self::Sim(nand) => format(nand, ebt),

            #[cfg(target_os = "linux")]
            Self::Mtd(nand) => format(nand, ebt),
        }
    }
}

#[derive(Args, Debug, Clone)]
#[group(required = true, id = "vol-type")]
struct UbiVolume {
    /// The type of the volume
    #[clap(long, group = "vol-type")]
    r#static: bool,
    #[clap(long, group = "vol-type")]
    dynamic: bool,

    /// The volume ID
    #[clap(long)]
    id: Option<u32>,

    /// The name of the volume
    #[clap(long)]
    name: Option<String>,

    /// The path to the image
    #[clap(long)]
    image: Option<PathBuf>,
}

impl From<UbiVolume> for BasicVolume<'static> {
    fn from(value: UbiVolume) -> Self {
        let vol_type = match value.dynamic {
            true => VolType::Dynamic,
            false => VolType::Static,
        };

        let mut volume = BasicVolume::new(vol_type);
        if let Some(id) = value.id {
            volume = volume.id(id);
        }
        if let Some(name) = value.name {
            volume = volume.name(name);
        }
        if let Some(image) = value.image {
            let file = File::open(image).expect("could not open image file");
            volume = volume.size(
                file.metadata()
                    .expect("could not get image file metadata")
                    .len(),
            );
            let boxed = Box::new(file);
            let leaked = Box::leak(boxed);
            volume = volume.image(leaked);
        }
        volume
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print a summary of the content of each PEB; this is a read-only operation
    UbiOverview,

    /// Perform a UBI format operation, erasing every PEB and filling in the proper EC header
    UbiFormat,

    /// Write UBI volumes
    UbiWrite(UbiVolume),

    /// Write a raw image to the NAND
    RawWrite {
        /// The path to the image to write to NAND
        path: PathBuf,

        /// Whether to skip over (thereby tolerating) any bad blocks encountered
        #[clap(long)]
        skip_bad: bool,
    },

    /// Look for Allwinner's boot0 blocks and erase them.
    PurgeBoot0,
}

impl Command {
    fn execute(self, nand: &mut NandImpl) -> Result<()> {
        match self {
            Command::UbiOverview => {
                let ebt = nand.do_scan()?;

                for (i, content) in ebt.iter().enumerate() {
                    println!("{i:4} => {content:?}");
                }
            }

            Command::UbiFormat => {
                let mut ebt = nand.do_scan()?;

                nand.do_format(&mut ebt)?;
            }

            Command::UbiWrite(volume) => {
                let volume: BasicVolume<'static> = volume.into();
                let volume: Box<dyn Volume> = Box::new(volume);

                let mut ebt = nand.do_scan()?;

                nand.do_format(&mut ebt)?;

                match nand {
                    NandImpl::Sim(nand) => write_volumes(nand, &mut ebt, [volume])?,

                    #[cfg(target_os = "linux")]
                    NandImpl::Mtd(nand) => write_volumes(nand, &mut ebt, [volume])?,
                }
            }

            Command::RawWrite { path, skip_bad } => {
                let mut image = File::open(path)?;

                match nand {
                    NandImpl::Sim(nand) => write_raw_image(nand, &mut image, skip_bad)?,

                    #[cfg(target_os = "linux")]
                    NandImpl::Mtd(nand) => write_raw_image(nand, &mut image, skip_bad)?,
                }
            }

            Command::PurgeBoot0 => {
                let purged = match nand {
                    NandImpl::Sim(nand) => purge_boot0(nand)?,

                    #[cfg(target_os = "linux")]
                    NandImpl::Mtd(nand) => purge_boot0(nand)?,
                };

                println!("Purged: {purged:?}");
            }
        };

        Ok(())
    }
}

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Cli {
    /// The NAND to use
    #[clap(flatten)]
    nand: NandOptions,

    /// The flashing command to run against this NAND
    #[clap(subcommand)]
    cmd: Command,
}

fn main() -> Result<()> {
    let args = Cli::parse();
    howudoin::init(howudoin::consumers::TermLine::default());

    let mut nand = args.nand.open()?;
    args.cmd.execute(&mut nand)?;
    args.nand.cleanup(nand)?;
    Ok(())
}
