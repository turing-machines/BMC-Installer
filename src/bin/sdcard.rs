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
use anyhow::Context;
use bmc_installer::turing_pi::{led, setup_initramfs, upgrade_bmc, wait_forever};
use retry::{delay::Fixed, retry};
use std::{
    fs,
    io::{self, Read, Seek},
    sync::{self, atomic},
    thread,
    time::Duration,
};

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

const KEYS_EVDEV_PATH: &str = "/dev/input/event0";
const ROOTFS_PATH: &str = "/dev/mmcblk0p2";
const BOOTLOADER_PATH: &str = "/dev/mmcblk0";

const BOOTLOADER_SIZE: u64 = 6 * 64 * 2048;
const BOOTLOADER_OFFSET: u64 = 8192; // Boot ROM expects this offset, so it will never change

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
    // Set up the LED blinking thread, in order to indicate further init errors
    let led_tx = led::led_blink_thread();

    let result = setup_initramfs().and_then(|_| {
        // Locate the rootfs and bootloader to be written
        let rootfs = retry(Fixed::from_millis(100).take(10), || {
            fs::File::open(ROOTFS_PATH)
        })
        .context(ROOTFS_PATH)?;

        let mut bootloader = retry(Fixed::from_millis(100).take(10), || {
            fs::File::open(BOOTLOADER_PATH)
        })
        .context(BOOTLOADER_PATH)?;
        bootloader.seek(io::SeekFrom::Start(BOOTLOADER_OFFSET))?;
        let bootloader = bootloader.take(BOOTLOADER_SIZE);
        Ok((bootloader, rootfs))
    });

    let Ok((bootloader, rootfs)) = result else {
        eprintln!(
            "[-] The installer could not initialize properly:\n{}",
            result.unwrap_err()
        );
        let _ = led_tx.send(led::LED_ERROR);
        wait_forever();
    };

    let pre_upgrade = || {
        eprintln!("{INSTRUCTIONS}");
        wait_for_confirmation();
    };

    if let Err(error) = upgrade_bmc(rootfs, bootloader, pre_upgrade, led_tx.clone()) {
        eprintln!("[-] Installation error:\n{error}");
        let _ = led_tx.send(led::LED_ERROR);
    } else {
        eprintln!("[+] DONE: Please remove the microSD card and reset the BMC.");
    }

    wait_forever()
}
