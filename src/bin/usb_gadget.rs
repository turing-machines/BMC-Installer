use anyhow::Context;
use bmc_installer::turing_pi::{led, setup_initramfs, upgrade_bmc, wait_forever};
use retry::{delay::Fixed, retry};
use std::fs;

const ROOTFS_PATH: &str = "/rootfs.erofs";
const BOOTLOADER_PATH: &str = "/u-boot-sunxi-with-spl.bin";

fn main() -> ! {
    // Set up the LED blinking thread, in order to indicate further init errors
    let led_tx = led::led_blink_thread();

    let result = setup_initramfs().and_then(|_| {
        // Locate the rootfs and bootloader to be written
        let bootloader = retry(Fixed::from_millis(100).take(10), || {
            fs::File::open(BOOTLOADER_PATH)
        })
        .context(BOOTLOADER_PATH)?;
        let rootfs = retry(Fixed::from_millis(100).take(10), || {
            fs::File::open(ROOTFS_PATH)
        })
        .context(ROOTFS_PATH)?;
        Ok((bootloader, rootfs))
    });

    let Ok((bootloader, rootfs)) = result else {
        eprintln!(
            "[-] The installer could not initialize properly:\n{:#}",
            result.unwrap_err()
        );
        let _ = led_tx.send(led::LED_ERROR);
        wait_forever();
    };

    if let Err(error) = upgrade_bmc(rootfs, bootloader, || (), led_tx.clone()) {
        eprintln!("[-] Installation error:\n{error}");
        let _ = led_tx.send(led::LED_ERROR);
    } else {
        eprintln!("[+] DONE: Reset BMC");
    }

    wait_forever()
}
