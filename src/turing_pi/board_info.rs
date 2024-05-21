use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
};

use bytes::{Buf, BufMut, BytesMut};

const BOARDINFO_SIZE: usize = 42;

#[repr(C, packed)]
pub struct BoardInfo {
    reserved: u16,
    crc32: u32,
    hdr_version: u16,
    hw_version: u16,
    factory_date: u16,
    factory_serial: [u8; 8],
    product_name: [u8; 16],
    mac: [u8; 6],
}

impl BoardInfo {
    pub fn load() -> io::Result<Self> {
        let eeprom = Self::find_i2c_device()?;
        let file = OpenOptions::new().read(true).open(eeprom)?;
        Self::from_reader(file)
    }

    pub fn from_reader(mut reader: impl io::Read) -> io::Result<Self> {
        let mut bytes = BytesMut::with_capacity(BOARDINFO_SIZE);
        reader.read_exact(&mut bytes)?;
        Self::from_bytes(bytes)
    }

    pub fn from_bytes(mut bytes: BytesMut) -> io::Result<Self> {
        let reserved = bytes.get_u16();
        let crc32 = bytes.get_u32();
        let hdr_version = bytes.get_u16();
        let hw_version = bytes.get_u16();
        let factory_date = bytes.get_u16();
        let mut factory_serial = [0u8; 8];
        bytes.copy_to_slice(&mut factory_serial);
        let mut product_name = [0u8; 16];
        bytes.copy_to_slice(&mut product_name);
        let mut mac = [0u8; 6];
        bytes.copy_to_slice(&mut mac);

        Ok(BoardInfo {
            reserved,
            crc32,
            hdr_version,
            hw_version,
            factory_date,
            factory_serial,
            product_name,
            mac,
        })
    }

    fn find_i2c_device() -> io::Result<PathBuf> {
        for entry in fs::read_dir("/sys/bus/i2c/devices/")? {
            let eeprom = entry?.path().join("eeprom");
            if eeprom.exists() {
                return Ok(eeprom);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "missing eeprom i2c device in /sys",
        ))
    }

    pub fn write_back(&self) -> io::Result<()> {
        let eeprom = Self::find_i2c_device()?;
        let mut file = OpenOptions::new().write(true).open(eeprom)?;

        let mut bytes = BytesMut::with_capacity(BOARDINFO_SIZE);
        bytes.put_u16(self.reserved);
        bytes.put_u32(self.crc32);
        bytes.put_u16(self.hdr_version);
        bytes.put_u16(self.hw_version);
        bytes.put_u16(self.factory_date);
        bytes.put_slice(&self.factory_serial);
        bytes.put_slice(&self.product_name);
        bytes.put_slice(&self.mac);

        file.write_all(&bytes)
    }
}
