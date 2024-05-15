pub mod led;

use nix::errno::Errno;
use nix::mount::{mount, MsFlags};
use std::io::{Read, Seek};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::{fs, path::Path};

use crate::{
    format, image,
    nand::{mtd::MtdNand, Nand},
    ubi::{
        self,
        ubinize::{BasicVolume, Volume},
        VolType,
    },
};

use self::led::LedState;

const BANNER: &str = r"
 _____ _   _ ____  ___ _   _  ____
|_   _| | | |  _ \|_ _| \ | |/ ___|
  | | | | | | |_) || ||  \| | |  _
  | | | |_| |  _ < | || |\  | |_| |
  |_|  \___/|_| \_\___|_| \_|\____|
";

/// Set up the basic environment (e.g. mount points).
pub fn setup_initramfs() -> anyhow::Result<()> {
    // Handle mounts
    for (mount_dev, mount_path, mount_type) in [
        (None, "/dev", "devtmpfs"),
        (None, "/proc", "proc"),
        (None, "/sys", "sysfs"),
    ] {
        let path = Path::new(mount_path);

        if !path.is_dir() {
            fs::create_dir(path)?;
        }

        let result = mount(
            mount_dev.or(Some(path)),
            path,
            Some(mount_type),
            MsFlags::empty(),
            None::<&str>,
        );

        match result {
            // Ignore EBUSY, which indicates that the mountpoint is already mounted.
            Err(errno) if errno == Errno::EBUSY => (),
            r => r?,
        };
    }

    Ok(())
}

/// Sleep until the user cuts power.
pub fn wait_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// This is the core function of the installer. Several tasks are executed to
/// upgrade from v1.x firmware or to install onto new flash.
pub fn upgrade_bmc(
    mut rootfs: impl Read + Seek,
    bootloader: impl Read,
    pre_upgrade: impl FnOnce(),
    led_tx: mpsc::Sender<&'static [LedState]>,
) -> anyhow::Result<()> {
    eprintln!("{}", BANNER);

    // Open the NAND flash partitions
    let nand_boot = MtdNand::open_named("boot")?;
    let nand_ubi = MtdNand::open_named("ubi")?;

    // Locate the rootfs and bootloader to be written
    let rootfs_size = image::erofs_size(&mut rootfs)?;

    // Define the UBI image
    let ubi_volumes: Vec<Box<dyn Volume + '_>> = vec![
        Box::new(
            BasicVolume::new(VolType::Dynamic)
                .id(0)
                .name("uboot-env")
                .size(65536),
        ),
        Box::new(
            BasicVolume::new(VolType::Static)
                .name("rootfs")
                .skipcheck() // Opening the volume at boot takes ~10sec. longer without this flag
                .size(rootfs_size)
                .image(&mut rootfs),
        ),
    ];

    // These are the tasks to be run once the user confirms the operation:
    struct TaskCtx<'a, N: Nand, R: Read> {
        rpt: howudoin::Tx,
        nand_boot: N,
        nand_ubi: N,
        ebt: Option<ubi::Ebt>,
        ubi_volumes: Vec<Box<dyn Volume + 'a>>,
        bootloader: R,
    }
    type TaskFn<Ctx> = fn(&mut Ctx) -> anyhow::Result<()>;
    let tasks: [(&str, TaskFn<TaskCtx<'_, _, _>>); 5] = [
        ("Purging boot0 code", |ctx| {
            let purged = format::purge_boot0(&mut ctx.nand_boot)?;
            if purged {
                ctx.rpt
                    .add_info("Legacy Allwinner boot code has been found and erased");
            }
            Ok(())
        }),
        ("Analyzing UBI partition", |ctx| {
            let ebt = ubi::scan_blocks(&mut ctx.nand_ubi)?;
            ctx.ebt = Some(ebt);
            Ok(())
        }),
        ("Formatting UBI partition", |ctx| {
            ubi::format(&mut ctx.nand_ubi, ctx.ebt.as_mut().unwrap())?;
            Ok(())
        }),
        ("Writing rootfs", |ctx| {
            ubi::write_volumes(
                &mut ctx.nand_ubi,
                ctx.ebt.as_mut().unwrap(),
                ctx.ubi_volumes.split_off(0),
            )?;
            Ok(())
        }),
        ("Updating bootloader", |ctx| {
            format::raw::write_raw_image(&mut ctx.nand_boot, &mut ctx.bootloader, false)?;
            Ok(())
        }),
    ];

    // Ready...
    let _ = led_tx.send(led::LED_READY);

    pre_upgrade();

    // ...go!
    howudoin::init(howudoin::consumers::TermLine::default());
    let rpt = howudoin::new()
        .label("Installing BMC firmware")
        .set_len(u64::try_from(tasks.len()).ok());
    let mut ctx = TaskCtx {
        rpt,
        nand_boot,
        nand_ubi,
        ebt: None,
        ubi_volumes,
        bootloader,
    };
    let _ = led_tx.send(led::LED_BUSY);
    for (desc, task) in tasks {
        ctx.rpt.desc(desc);
        ctx.rpt.inc();

        if let Err(error) = task(&mut ctx) {
            howudoin::disable();
            thread::sleep(Duration::from_millis(10)); // Give howudoin time to shut down
            return Err(error);
        }
    }

    ctx.rpt.finish();
    howudoin::disable();
    thread::sleep(Duration::from_millis(10)); // Give howudoin time to shut down
    let _ = led_tx.send(led::LED_DONE);

    Ok(())
}
