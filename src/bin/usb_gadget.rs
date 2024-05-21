#![feature(cursor_remaining)]
use std::sync::mpsc::Sender;
use std::{io, sync::mpsc};

use anyhow::{bail, Context};
use bmc_installer::turing_pi::board_info::BoardInfo;
use bmc_installer::turing_pi::get_ubi_volumes;
use bmc_installer::util::ReceiverReader;
use bmc_installer::{
    format,
    nand::mtd::MtdNand,
    turing_pi::{led, setup_initramfs, wait_forever},
    ubi,
};
use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;
use usb_gadget::{
    default_udc,
    function::custom::{
        Custom, Endpoint, EndpointDirection, EndpointReceiver, EndpointSender, Interface,
    },
    Class, Config, Gadget, Id, OsDescriptor, RegGadget, Strings, WebUsb,
};

fn setup_gadget() -> anyhow::Result<(EndpointSender, EndpointReceiver, RegGadget)> {
    usb_gadget::remove_all().context("cannot remove all gadgets")?;
    let serial_number = String::from_utf8(std::fs::read("/proc/device-tree/serial-number")?)?;

    let (mut ep1_rx, ep1_dir) = EndpointDirection::host_to_device();
    let (mut ep2_tx, ep2_dir) = EndpointDirection::device_to_host();

    let (custom, handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::vendor_specific(1, 2), "custom interface")
                .with_endpoint(Endpoint::bulk(ep1_dir))
                .with_endpoint(Endpoint::bulk(ep2_dir)),
        )
        .build();
    let udc = default_udc().context("cannot get UDC")?;
    let reg = Gadget::new(
        Class::new(255, 255, 3),
        Id::new(6, 0x11),
        Strings::new("Turing Machines", "USB Install gadget", serial_number),
    )
    .with_config(Config::new("config").with_function(handle))
    .with_os_descriptor(OsDescriptor::microsoft())
    .with_web_usb(WebUsb::new(0xf1, "http://webusb.org"))
    .bind(&udc)
    .context("cannot bind to UDC")?;

    println!(
        "Custom function at {}",
        custom.status().unwrap().path().unwrap().display()
    );

    let ep1_control = ep1_rx.control().unwrap();
    println!("ep1 unclaimed: {:?}", ep1_control.unclaimed_fifo());
    println!("ep1 real address: {}", ep1_control.real_address().unwrap());
    println!("ep1 descriptor: {:?}", ep1_control.descriptor().unwrap());
    println!();

    let ep2_control = ep2_tx.control().unwrap();
    println!("ep2 unclaimed: {:?}", ep2_control.unclaimed_fifo());
    println!("ep2 real address: {}", ep2_control.real_address().unwrap());
    println!("ep2 descriptor: {:?}", ep2_control.descriptor().unwrap());
    println!();

    Ok((ep2_tx, ep1_rx, reg))
}

#[derive(Debug)]
enum InstallPart {
    Bootloader = 0,
    Rootfs = 1,
    EEPROM = 2,
}

impl TryFrom<u8> for InstallPart {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(InstallPart::Bootloader),
            1 => Ok(InstallPart::Rootfs),
            2 => Ok(InstallPart::EEPROM),
            _ => bail!("unknown install part `{}`", value),
        }
    }
}

#[derive(Error, Debug)]
pub enum InstallError {
    #[error("Empty buffer on fetch of bulk endpoint")]
    EmptyBuffer,
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl InstallError {
    pub fn error_code(&self) -> u32 {
        match self {
            InstallError::EmptyBuffer => 0,
            InstallError::Anyhow(_) => 1,
            InstallError::Io(_) => 2,
        }
    }
}

/// poor man's protocol, each install part to write is sent as a contiguous byte stream with a
/// prepended header. This header states what part to write and the length of this byte stream.
fn wait_for_command(
    rx_ep: &mut EndpointReceiver,
) -> Result<(Option<InstallPart>, u64), InstallError> {
    let mut cmd = rx_ep.recv_and_fetch(BytesMut::with_capacity(9))?;
    println!("{:?}", cmd);

    let install_part: Option<InstallPart> = cmd.get_u8().try_into().ok();
    let size = cmd.get_u64();
    if cmd.has_remaining() {
        println!("Warning: dropping {} bytes", cmd.remaining());
    }

    println!("InstallPart:{:?}, size:{}", install_part, size);
    Ok((install_part, size))
}

fn handle_bootloader(
    mut reader: impl io::Read,
    status_printer: mpsc::Sender<&'static str>,
) -> anyhow::Result<()> {
    status_printer.send("Puring boot0 code")?;
    let mut nand_boot = MtdNand::open_named("boot")?;
    let purged = format::purge_boot0(&mut nand_boot)?;
    if purged {
        println!("Legacy Allwinner boot code has been found and erased");
    }

    status_printer.send("Updating bootloader")?;
    format::raw::write_raw_image(&mut nand_boot, &mut reader, false)
}

fn handle_rootfs(
    mut reader: impl io::Read,
    size: u64,
    status_printer: mpsc::Sender<&'static str>,
) -> anyhow::Result<()> {
    let mut nand_ubi = MtdNand::open_named("ubi")?;

    status_printer.send("Analyzing UBI partition")?;
    let mut ebt = ubi::scan_blocks(&mut nand_ubi)?;

    status_printer.send("Formatting UBI partition")?;
    ubi::format(&mut nand_ubi, &mut ebt)?;

    status_printer.send("Writing rootfs")?;
    let mut ubi_volumes = get_ubi_volumes(&mut reader, size);
    ubi::write_volumes(&mut nand_ubi, &mut ebt, ubi_volumes.split_off(0))
}

fn handle_eeprom(
    reader: impl io::Read,
    status_printer: mpsc::Sender<&'static str>,
) -> anyhow::Result<()> {
    status_printer.send("updating EEPROM")?;
    let board_info = BoardInfo::from_reader(reader)?;
    Ok(board_info.write_back()?)
}

fn run_status_printer() -> Sender<&'static str> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(task) = receiver.recv() {
            println!("* {}", task);
        }
    });
    sender
}

fn send_response(tx: &mut EndpointSender, error: Option<InstallError>) -> io::Result<()> {
    if let Some(e) = error {
        let mut bytes = BytesMut::with_capacity(4);
        bytes.put_u32(e.error_code());
        tx.send(bytes.into())?;
    } else {
        tx.send(vec![0].into())?;
    }

    Ok(())
}

fn run_usb_gadget(status_printer: mpsc::Sender<&'static str>) -> Result<(), InstallError> {
    setup_initramfs()?;
    usb_gadget::remove_all().context("cannot remove all gadgets")?;
    let serial_number =
        String::from_utf8(std::fs::read("/proc/device-tree/serial-number")?).unwrap_or_default();

    let (mut ep1_rx, ep1_dir) = EndpointDirection::host_to_device();
    let (mut ep2_tx, ep2_dir) = EndpointDirection::device_to_host();

    let (custom, handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::vendor_specific(1, 2), "custom interface")
                .with_endpoint(Endpoint::bulk(ep1_dir))
                .with_endpoint(Endpoint::bulk(ep2_dir)),
        )
        .build();
    let udc = default_udc().context("cannot get UDC")?;
    let reg = Gadget::new(
        Class::new(255, 255, 3),
        Id::new(6, 0x11),
        Strings::new("Turing Machines", "USB Install gadget", serial_number),
    )
    .with_config(Config::new("config").with_function(handle))
    .with_os_descriptor(OsDescriptor::microsoft())
    .with_web_usb(WebUsb::new(0xf1, "http://webusb.org"))
    .bind(&udc)
    .context("cannot bind to UDC")?;

    println!("starting loop");
    loop {
        let (cmd, size) = wait_for_command(&mut ep1_rx)?;
        if cmd.is_none() {
            println!("end of program signaled");
            return Ok(send_response(&mut ep2_tx, None)?);
        }

        let receiver_reader = ReceiverReader::new(&mut ep1_rx, None);

        let res = match cmd.expect("cmd is is_none tested") {
            InstallPart::Rootfs => handle_rootfs(receiver_reader, size, status_printer.clone()),
            InstallPart::Bootloader => handle_bootloader(receiver_reader, status_printer.clone()),
            InstallPart::EEPROM => handle_eeprom(receiver_reader, status_printer.clone()),
        };

        if let Err(e) = send_response(&mut ep2_tx, res.err().map(InstallError::from)) {
            return Err(e.into());
        }
    }
}

fn main() -> ! {
    // Set up the LED blinking thread, in order to indicate further init errors
    let led_tx = led::led_blink_thread();
    let status_printer = run_status_printer();

    if let Err(error) = run_usb_gadget(status_printer) {
        eprintln!("[-] Error setting up USB interface:\n{:#}", error);
        let _ = led_tx.send(led::LED_ERROR);
        wait_forever();
    };

    wait_forever()
}
