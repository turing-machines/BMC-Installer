//! This is the main binary that runs when the installer is used on the SD Card installation image.
//!
//! It runs in the initramfs environment, and is the only binary, which means:
//! 1. Its runtime path is `/init`.
//! 2. Exiting will panic the kernel. This is fine in principle, but may bother the user, so errors
//!    and successful completion alike should end with telling the user to remove the card and
//!    reset the BMC, then waiting.
//! 3. There are no other binaries to call into. As tempted as we may be, we can't rely on
//!    subprocesses to do any of the work. This binary needs to be self-contained.
//! 4. The filesystem starts empty. Essential mountpoints like `/proc` and `/sys` need to be
//!    established before any meaningful work can be done.

use nix::errno::Errno;
use nix::mount::{mount, MsFlags};

use std::{
    cell::RefCell,
    fs,
    io::{self, Read, Seek},
    path::Path,
    sync::mpsc,
    thread,
    time::Duration,
};

use bmc_installer::{
    format, image,
    nand::mtd::MtdNand,
    ubi::{
        self,
        ubinize::{BasicVolume, Volume},
        VolType,
    },
};

const BANNER: &str = r"
 _____ _   _ ____  ___ _   _  ____ 
|_   _| | | |  _ \|_ _| \ | |/ ___|
  | | | | | | |_) || ||  \| | |  _ 
  | | | |_| |  _ < | || |\  | |_| |
  |_|  \___/|_| \_\___|_| \_|\____|
";

const INSTRUCTIONS: &str = "\
This utility will perform a fresh installation of the Turing Pi 2 BMC firmware.

Note that this will ERASE ALL USER DATA stored on the Turing Pi 2 BMC, thus
restoring back to factory defaults. Do NOT proceed unless you have first backed
up any files that you care about!

If you wish to confirm the operation and proceed, either:
1) Type 'CONFIRM' at the below prompt
2) Press one of the front panel buttons (POWER or RESET), or the KEY1 button on
   the Turing Pi 2 board itself, three times in a row

If you are here in error, please remove the microSD card from the Turing Pi 2
board and reset the BMC.
";

const STATUS_LED_PATH: &str = "/sys/class/leds/fp:sys/brightness";

const ROOTFS_PATH: &str = "/dev/mmcblk0p2";
const BOOTLOADER_PATH: &str = "/dev/mmcblk0";
const BOOTLOADER_OFFSET: u64 = 8192; // Boot ROM expects this offset, so it will never change
const BOOTLOADER_SIZE: u64 = 5 * 64 * 2048;

/// Set up the basic environment (e.g. mount points).
fn setup_initramfs() -> anyhow::Result<()> {
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

/// LED blink patterns
mod led {
    use std::time::Duration;

    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    pub enum LedState {
        On(Duration),
        Off(Duration),
    }
    use LedState::*;

    pub const LED_READY: &[LedState] = &[
        On(Duration::from_millis(1500)),
        Off(Duration::from_millis(1500)),
    ];

    pub const LED_BUSY: &[LedState] = &[
        On(Duration::from_millis(66)),
        Off(Duration::from_millis(66)),
    ];

    pub const LED_DONE: &[LedState] = &[
        On(Duration::from_millis(100)),
        Off(Duration::from_millis(200)),
        On(Duration::from_millis(100)),
        Off(Duration::from_millis(2600)),
    ];

    const DIT: Duration = Duration::from_millis(150);
    const DAH: Duration = Duration::from_millis(450);
    const GAP: Duration = Duration::from_millis(1050);
    pub const LED_ERROR: &[LedState] = &[
        // S ...
        On(DIT),
        Off(DIT),
        On(DIT),
        Off(DIT),
        On(DIT),
        Off(DAH),
        // O ---
        On(DAH),
        Off(DIT),
        On(DAH),
        Off(DIT),
        On(DAH),
        Off(DAH),
        // S ...
        On(DIT),
        Off(DIT),
        On(DIT),
        Off(DIT),
        On(DIT),
        Off(GAP),
    ];
}

/// This runs in a thread and manages the LED blinking.
///
/// Send new blink patterns through the MPSC channel to change the active pattern.
fn led_blink_thread(rx: mpsc::Receiver<&'static [led::LedState]>) {
    let mut pattern = &[led::LedState::Off(Duration::from_secs(3600))][..];
    let mut remaining = &[][..];

    let mut brightness_file = fs::File::options().write(true).open(STATUS_LED_PATH).ok();
    let mut set_led = move |state| {
        use std::io::Write;
        let chr = match state {
            true => b'1',
            false => b'0',
        };
        if let Some(ref mut f) = &mut brightness_file {
            let _ = f.write_all(&[chr, b'\n']);
        }
    };

    loop {
        let cmd = match remaining.split_first() {
            Some((first, elements)) => {
                remaining = elements;
                *first
            }
            None => {
                remaining = pattern;
                continue;
            }
        };

        let (on, delay) = match cmd {
            led::LedState::On(delay) => (true, delay),
            led::LedState::Off(delay) => (false, delay),
        };

        set_led(on);

        match rx.recv_timeout(delay) {
            Err(err) => match err {
                mpsc::RecvTimeoutError::Timeout => (),
                mpsc::RecvTimeoutError::Disconnected => break,
            },
            Ok(new_pattern) => {
                pattern = new_pattern;
                remaining = &[];
            }
        }
    }

    set_led(false);
}

/// Sleep until the user cuts power.
fn wait_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// Wait until the user confirms the installation operation, through either the serial prompt or
/// by pressing a GPIO key multiple times.
///
/// This spawns one thread each, for both methods.
fn wait_for_confirmation() {
    let threads = [thread::spawn(confirm_prompt)];
    threads.into_iter().for_each(|x| {
        let _ = x.join();
    });
}

/// Repeatedly nag the user to type "CONFIRM"
fn confirm_prompt() -> bool {
    const CONFIRM_KEYWORD: &str = "CONFIRM";

    let mut input = String::new();
    loop {
        eprint!("Type \"{CONFIRM_KEYWORD}\" to continue: ");
        input.clear();
        match io::stdin().read_line(&mut input) {
            Ok(_) if input.trim_end() == CONFIRM_KEYWORD => return true,
            Ok(0) => return false,
            _ => continue,
        };
    }
}

/// The main SD Card installation program.
///
/// This function must never return.
fn main() -> ! {
    eprintln!("{BANNER}");

    // Do all initialization before showing the instructions and prompting for confirmation
    if let Err(err) = setup_initramfs() {
        eprintln!("[-] The installer could not initialize properly:\n{err}");
        wait_forever();
    }

    // Set up the LED blinking thread, in order to indicate further init errors
    let (led_tx, led_rx) = mpsc::channel();
    thread::spawn(move || led_blink_thread(led_rx));
    let init_error = |err: anyhow::Error| -> ! {
        eprintln!("[-] The installer could not initialize properly:\n{err}");
        let _ = led_tx.send(led::LED_ERROR);
        wait_forever();
    };

    // Open the NAND flash partitions
    let nand_boot = RefCell::new(MtdNand::open_named("boot").unwrap_or_else(|e| init_error(e)));
    let nand_ubi = RefCell::new(MtdNand::open_named("ubi").unwrap_or_else(|e| init_error(e)));
    let ebt_cell: RefCell<Option<ubi::Ebt>> = RefCell::new(None);

    // Locate the rootfs and bootloader to be written
    let mut rootfs = fs::File::open(ROOTFS_PATH).unwrap_or_else(|e| init_error(e.into()));
    let rootfs_size = image::erofs_size(&mut rootfs).unwrap_or_else(|e| init_error(e));

    let mut bootloader = fs::File::open(BOOTLOADER_PATH).unwrap_or_else(|e| init_error(e.into()));
    bootloader
        .seek(io::SeekFrom::Start(BOOTLOADER_OFFSET))
        .unwrap_or_else(|e| init_error(e.into()));
    let mut bootloader = bootloader.take(BOOTLOADER_SIZE);

    // Define the UBI image
    let ubi_volumes: [Box<dyn Volume + '_>; 2] = [
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
    type TaskFn<'a> = dyn FnOnce(&howudoin::Tx) -> anyhow::Result<()> + 'a;
    let tasks: [(&str, Box<TaskFn<'_>>); 5] = [
        (
            "Purging boot0 code",
            Box::new(|rpt| {
                let purged = format::purge_boot0(&mut *nand_boot.borrow_mut())?;
                if purged {
                    rpt.add_info("Legacy Allwinner boot code has been found and erased");
                }
                Ok(())
            }),
        ),
        (
            "Analyzing UBI partition",
            Box::new(|_| {
                let ebt = ubi::scan_blocks(&mut *nand_ubi.borrow_mut())?;
                *ebt_cell.borrow_mut() = Some(ebt);
                Ok(())
            }),
        ),
        (
            "Formatting UBI partition",
            Box::new(|_| {
                ubi::format(
                    &mut *nand_ubi.borrow_mut(),
                    ebt_cell.borrow_mut().as_mut().unwrap(),
                )?;
                Ok(())
            }),
        ),
        (
            "Writing rootfs",
            Box::new(|_| {
                ubi::write_volumes(
                    &mut *nand_ubi.borrow_mut(),
                    ebt_cell.borrow_mut().as_mut().unwrap(),
                    ubi_volumes,
                )?;
                Ok(())
            }),
        ),
        (
            "Updating bootloader",
            Box::new(|_| {
                format::raw::write_raw_image(&mut *nand_boot.borrow_mut(), &mut bootloader, false)?;
                Ok(())
            }),
        ),
    ];

    // Ready...
    eprintln!("{INSTRUCTIONS}");
    let _ = led_tx.send(led::LED_READY);
    wait_for_confirmation();

    // ...go!
    howudoin::init(howudoin::consumers::TermLine::default());
    let rpt = howudoin::new()
        .label("Installing BMC firmware")
        .set_len(u64::try_from(tasks.len()).ok());
    let _ = led_tx.send(led::LED_BUSY);
    for (desc, task) in tasks {
        rpt.desc(desc);
        rpt.inc();

        if let Err(error) = task(&rpt) {
            howudoin::disable();
            thread::sleep(Duration::from_millis(10)); // Give howudoin time to shut down
            eprintln!("[-] Installation error:\n{error}");
            let _ = led_tx.send(led::LED_ERROR);
            wait_forever();
        }
    }

    rpt.finish();
    howudoin::disable();
    thread::sleep(Duration::from_millis(10)); // Give howudoin time to shut down
    eprintln!("[+] DONE: Please remove the microSD card and reset the BMC.");
    let _ = led_tx.send(led::LED_DONE);

    wait_forever()
}
