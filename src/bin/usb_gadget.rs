use anyhow::Context;
use bmc_installer::bin_shared::*;
use std::str::from_utf8;
use usb_gadget::{
    function::msd::Lun, function::msd::Msd, udcs, Class, Config, Gadget, Id, RegGadget, Strings,
};

const MTD_PARTITION_0: &str = "/dev/mtdblock0";
const MTD_PARTITION_1: &str = "/dev/mtdblock1";

fn setup_gadget() -> anyhow::Result<RegGadget> {
    let udcs = udcs()?;
    let udc = udcs.first().context("could not find UDC")?;

    let serial_number = std::fs::read("/proc/device-tree/serial-number")?;
    let product = std::fs::read("/proc/device-tree/model")?;
    let manufacturer = "Turing Machine";

    let strings = Strings::new(
        manufacturer,
        from_utf8(&product)?.trim(),
        from_utf8(&serial_number)?.trim(),
    );

    // TODO: get turing machine's specific ID
    let id = Id::new(0x18d1, 0002);
    let class = Class::new(0x8, 0x1, 0x0);

    let mut builder = Msd::builder();
    builder.add_lun(Lun::new(MTD_PARTITION_0)?);
    builder.add_lun(Lun::new(MTD_PARTITION_1)?);
    let (_, handle) = builder.build();
    let config = Config::new("Mass storage USB config").with_function(handle);

    Ok(Gadget::new(class, id, strings)
        .with_config(config)
        .bind(&udc)?)
}

fn main() -> ! {
    eprintln!("{BANNER}");

    // Do all initialization before showing the instructions and prompting for confirmation
    if let Err(err) = setup_initramfs() {
        eprintln!("[-] The installer could not initialize properly:\n{err}");
        wait_forever();
    }

    let _ = setup_gadget().unwrap();
    loop {}
}
