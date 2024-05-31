use bmc_installer::turing_pi::{led, read_from_sdcard, upgrade_bmc};

fn main() -> anyhow::Result<()> {
    let led_tx = led::led_blink_thread();
    let (bootloader, rootfs) = read_from_sdcard()?;
    upgrade_bmc(rootfs, bootloader, || (), led_tx)
}
