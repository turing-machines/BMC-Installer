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

use retry::{delay::Fixed, retry};

use std::{
    fs,
    io::{self, Read, Seek},
    path::Path,
    sync::{self, atomic, mpsc},
    thread,
    time::Duration,
};

use bmc_installer::{
    format, image,
    nand::{mtd::MtdNand, Nand},
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
const KEYS_EVDEV_PATH: &str = "/dev/input/event0";

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
        /// All LEDs on, for the specified duration
        On(Duration),

        /// All LEDs off, for the specified duration
        Off(Duration),

        /// A custom combination of FP LED and swport LEDs, for the specified duration
        Custom(Duration, bool, [bool; 4]),
    }
    use LedState::*;

    impl LedState {
        pub fn get_duration(self) -> Duration {
            match self {
                On(x) => x,
                Off(x) => x,
                Custom(x, _, _) => x,
            }
        }

        pub fn get_leds(self) -> (bool, [bool; 4]) {
            match self {
                On(_) => (true, [true; 4]),
                Off(_) => (false, [false; 4]),
                Custom(_, x, y) => (x, y),
            }
        }
    }

    pub const LED_READY: &[LedState] = &[
        On(Duration::from_millis(1500)),
        Off(Duration::from_millis(1500)),
    ];

    pub const LED_BUSY: &[LedState] = &[
        Custom(Duration::from_millis(66), true, [true, false, false, false]),
        Custom(Duration::from_millis(66), false, [false, true, false, false]),
        Custom(Duration::from_millis(66), true, [false, false, true, false]),
        Custom(Duration::from_millis(66), false, [false, false, false, true]),
        Custom(Duration::from_millis(66), true, [false, false, false, false]),
        Custom(Duration::from_millis(66), false, [false, false, false, false]),
        Custom(Duration::from_millis(66), true, [false, false, false, false]),
        Custom(Duration::from_millis(66), false, [false, false, false, false]),
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

/// This runs in a thread that communicates with the RTL8370MB to control its LEDs. These LEDs are
/// useful in case the user doesn't have other indicators connected (like front panel LEDs).
///
/// It's a separate thread because these operations may block a bit more than typical GPIO, and we
/// don't want to interfere with the timings on the main LED blinking thread.
///
/// Send `[bool; 4]` arrays to control the 4 external port LEDs (left-to-right).
fn rtl8370mb_led_thread(rx: mpsc::Receiver<[bool; 4]>) {
    const I2C_PATH: &str = "/dev/i2c-0";
    const I2C_ADDR: u8 = 0x5c;
    const REG_GPIO_O: [u16; 2] = [0x1d1d, 0x1d1e];
    const REG_GPIO_OE: [u16; 2] = [0x1d21, 0x1d22];
    const REG_GPIO_MODE: [u16; 2] = [0x1d25, 0x1d26];
    const LED_GPIO_MAP: [u8; 4] = [15, 16, 1, 7];

    let mut i2c = match i2c_linux::I2c::from_path(I2C_PATH) {
        Ok(x) => x,
        Err(_) => return,
    };

    // Updates the specified register on the IC, ignoring errors
    let mut write_reg = |index: u16, value: u16| {
        let mut data = [0u8; 4];
        data[0..2].copy_from_slice(&index.to_le_bytes());
        data[2..4].copy_from_slice(&value.to_le_bytes());
        let _ = i2c.i2c_transfer(&mut [i2c_linux::Message::Write {
            address: I2C_ADDR.into(),
            data: &data,
            flags: Default::default(),
        }]);
    };

    // Converts a sequence of GPIO indexes to the u16s representing those pins
    let to_bitmaps = |indexes: &mut dyn Iterator<Item = u8>| {
        let map: u32 = indexes.map(|index| 1 << index).fold(0, |x, y| x | y);
        [map as u16, (map >> 16) as u16]
    };

    let bitmaps = to_bitmaps(&mut LED_GPIO_MAP.into_iter());
    for regs in [REG_GPIO_O, REG_GPIO_OE, REG_GPIO_MODE] {
        for (reg, value) in regs.into_iter().zip(bitmaps) {
            write_reg(reg, value);
        }
    }

    // All LEDs are off
    let mut current_bitmaps = bitmaps;

    while let Ok(states) = rx.recv() {
        // Compute the bitmaps for this new state
        let new_bitmaps = to_bitmaps(&mut LED_GPIO_MAP.into_iter().zip(states).filter_map(
            |(index, on)| match on {
                // off -> '1'
                false => Some(index),

                // on -> '0'
                true => None,
            },
        ));

        // Update only the registers that need it
        for ((reg, to), from) in REG_GPIO_O.into_iter().zip(new_bitmaps).zip(current_bitmaps) {
            if to != from {
                write_reg(reg, to);
            }
        }

        current_bitmaps = new_bitmaps;
    }
}

/// This runs in a thread and manages the LED blinking.
///
/// Send new blink patterns through the MPSC channel to change the active pattern.
fn led_blink_thread(rx: mpsc::Receiver<&'static [led::LedState]>) {
    let mut pattern = &[led::LedState::Off(Duration::from_secs(3600))][..];
    let mut remaining = &[][..];

    let (rtl_tx, rtl_rx) = mpsc::channel();
    thread::spawn(move || rtl8370mb_led_thread(rtl_rx));

    let mut brightness_file = fs::File::options().write(true).open(STATUS_LED_PATH).ok();
    let mut set_leds = move |fp, swports| {
        use std::io::Write;
        let _ = rtl_tx.send(swports);

        let chr = match fp {
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

        let (fw, swports) = cmd.get_leds();
        set_leds(fw, swports);

        match rx.recv_timeout(cmd.get_duration()) {
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

    set_leds(false, [false; 4]);
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
    let signals = sync::Arc::new((atomic::AtomicBool::new(false), thread::current()));
    let signals_1 = signals.clone();
    let signals_2 = signals.clone();
    let mut threads = [
        Some(thread::spawn(move || {
            let (stop_flag, main_thread) = &*signals_1;
            let ret = confirm_prompt(stop_flag);
            main_thread.unpark();
            ret
        })),
        Some(thread::spawn(move || {
            let (stop_flag, main_thread) = &*signals_2;
            let ret = confirm_keypress(stop_flag);
            main_thread.unpark();
            ret
        })),
    ];

    thread::park();

    loop {
        for thread in &mut threads {
            if thread.as_ref().map_or(false, |thread| thread.is_finished()) {
                let ret = thread
                    .take()
                    .unwrap()
                    .join()
                    .expect("thread should not panic");
                if ret {
                    signals.0.store(true, atomic::Ordering::Relaxed);
                    return;
                }
                thread::park();
            }
        }
    }
}

/// Repeatedly nag the user to type "CONFIRM"
fn confirm_prompt(stop_flag: &atomic::AtomicBool) -> bool {
    const CONFIRM_KEYWORD: &str = "CONFIRM";

    let mut input = String::new();
    loop {
        if stop_flag.load(atomic::Ordering::Relaxed) {
            return false;
        }
        eprint!("Type \"{CONFIRM_KEYWORD}\" to continue: ");
        input.clear();
        match io::stdin().read_line(&mut input) {
            Ok(_) if input.trim_end() == CONFIRM_KEYWORD => return true,
            Ok(0) => return false,
            _ => continue,
        };
    }
}

/// Monitor for a key being pressed three times
fn confirm_keypress(stop_flag: &atomic::AtomicBool) -> bool {
    const KEYPRESS_TIMEOUT: Duration = Duration::from_millis(500);
    const KEYPRESS_TIMES: u8 = 3;

    let mut device = match evdev::raw_stream::RawDevice::open(KEYS_EVDEV_PATH) {
        Ok(device) => device,
        Err(_) => return false,
    };

    let mut last_key = None;
    let mut last_time = None;
    let mut times_pressed = 0;

    loop {
        if stop_flag.load(atomic::Ordering::Relaxed) {
            return false;
        }

        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(_) => return false,
        };

        for event in events {
            // Only follow key events
            let key = match event.kind() {
                evdev::InputEventKind::Key(key) => key,
                _ => continue,
            };

            // All keypresses have to be the same key; start over if the user switched keys
            if last_key != Some(key) {
                last_key = Some(key);
                times_pressed = 0;
            }

            // Only handle key-up events past this point
            if event.value() != 0 {
                continue;
            }

            // Determine how long has passed since the last key-up event (or None)
            let timestamp = event.timestamp();
            let time_elapsed = last_time
                .replace(timestamp)
                .and_then(|x| timestamp.duration_since(x).ok());

            // If past the timeout (or None), start over
            if time_elapsed.map_or(true, |x| x > KEYPRESS_TIMEOUT) {
                times_pressed = 0;
            }

            times_pressed += 1;
            if times_pressed >= KEYPRESS_TIMES {
                return true;
            }
        }
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
    let nand_boot = MtdNand::open_named("boot").unwrap_or_else(|e| init_error(e));
    let nand_ubi = MtdNand::open_named("ubi").unwrap_or_else(|e| init_error(e));

    // Locate the rootfs and bootloader to be written
    let mut rootfs = retry(Fixed::from_millis(100).take(10), || {
        fs::File::open(ROOTFS_PATH)
    })
    .unwrap_or_else(|e| init_error(e.into()));
    let rootfs_size = image::erofs_size(&mut rootfs).unwrap_or_else(|e| init_error(e));

    let mut bootloader = retry(Fixed::from_millis(100).take(10), || {
        fs::File::open(BOOTLOADER_PATH)
    })
    .unwrap_or_else(|e| init_error(e.into()));
    bootloader
        .seek(io::SeekFrom::Start(BOOTLOADER_OFFSET))
        .unwrap_or_else(|e| init_error(e.into()));
    let bootloader = bootloader.take(BOOTLOADER_SIZE);

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
    eprintln!("{INSTRUCTIONS}");
    let _ = led_tx.send(led::LED_READY);
    wait_for_confirmation();

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
            eprintln!("[-] Installation error:\n{error}");
            let _ = led_tx.send(led::LED_ERROR);
            wait_forever();
        }
    }

    ctx.rpt.finish();
    howudoin::disable();
    thread::sleep(Duration::from_millis(10)); // Give howudoin time to shut down
    eprintln!("[+] DONE: Please remove the microSD card and reset the BMC.");
    let _ = led_tx.send(led::LED_DONE);

    wait_forever()
}
