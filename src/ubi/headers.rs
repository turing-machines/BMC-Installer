//! This module contains the code necessary to read, write, and manipulate EC/VID headers, with
//! CRC verification/computation.

use crc::{Crc, CRC_32_JAMCRC};
pub use deku::{DekuContainerRead, DekuContainerWrite};
use income::{EcHdr, VidHdr, VtblRecord, UBI_EC_HDR_MAGIC, UBI_VID_HDR_MAGIC};

pub const UBI_CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_JAMCRC);
const UBI_VERSION: u8 = 1;

/// A trait missing from the `income` crate: implements parsing UBI headers from byteslices, with
/// magic and CRC verification.
pub trait ParseHeader<'a>: Sized + DekuContainerRead<'a> + ComputeCrc {
    fn get_magic() -> &'static [u8];
    fn get_hdr_magic(&self) -> &[u8];
    fn get_hdr_version(&self) -> u8;

    fn parse(buf: &'a [u8]) -> Option<Self> {
        let (_, header) = Self::from_bytes((buf, 0)).ok()?;

        if (header.get_hdr_magic(), header.get_hdr_version()) != (Self::get_magic(), UBI_VERSION) {
            return None;
        }

        if !header.check_crc() {
            return None;
        }

        Some(header)
    }
}

impl ParseHeader<'_> for EcHdr {
    fn get_magic() -> &'static [u8] {
        UBI_EC_HDR_MAGIC
    }
    fn get_hdr_magic(&self) -> &[u8] {
        &self.magic
    }
    fn get_hdr_version(&self) -> u8 {
        self.version
    }
}

impl ParseHeader<'_> for VidHdr {
    fn get_magic() -> &'static [u8] {
        UBI_VID_HDR_MAGIC
    }
    fn get_hdr_magic(&self) -> &[u8] {
        &self.magic
    }
    fn get_hdr_version(&self) -> u8 {
        self.version
    }
}

/// Another trait missing from `income` to compute the correct CRC for some Vid/Ec header
pub trait ComputeCrc: DekuContainerWrite {
    fn compute_crc(&self) -> u32 {
        let header_bytes = self.to_bytes().unwrap();
        let header_len = header_bytes.len() - std::mem::size_of::<u32>();
        UBI_CRC.checksum(&header_bytes[..header_len])
    }

    fn check_crc(&self) -> bool {
        self.get_crc() == self.compute_crc()
    }

    fn fix_crc(&mut self) {
        self.set_crc(self.compute_crc())
    }

    fn get_crc(&self) -> u32;
    fn set_crc(&mut self, crc: u32);
}

impl ComputeCrc for EcHdr {
    fn get_crc(&self) -> u32 {
        self.hdr_crc
    }
    fn set_crc(&mut self, crc: u32) {
        self.hdr_crc = crc;
    }
}
impl ComputeCrc for VidHdr {
    fn get_crc(&self) -> u32 {
        self.hdr_crc
    }
    fn set_crc(&mut self, crc: u32) {
        self.hdr_crc = crc;
    }
}
impl ComputeCrc for VtblRecord {
    fn get_crc(&self) -> u32 {
        self.crc
    }
    fn set_crc(&mut self, crc: u32) {
        self.crc = crc;
    }
}

/// This represents the specific fields we care about in an EC header
///
/// This is meant to be more ergonomic to work with than EcHdr, which represents the raw data
#[derive(Debug, Default, Eq, PartialEq, Copy, Clone)]
pub struct Ec {
    pub ec: u64,
    pub vid_hdr_offset: u32,
    pub data_offset: u32,
    pub image_seq: u32,
}

impl Ec {
    /// Change the erase counter of this EC header
    pub fn ec(mut self, ec: u64) -> Self {
        self.ec = ec;
        self
    }

    /// Increment the erase counter of this EC header
    pub fn inc_ec(mut self) -> Self {
        self.ec += 1;
        self
    }

    /// Convert from a byte slice
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        EcHdr::parse(bytes).map(|x| x.into())
    }

    /// Write into a byte slice
    pub fn encode(self, out_bytes: &mut [u8]) -> anyhow::Result<()> {
        let bytes = EcHdr::from(self).to_bytes()?;
        let out_bytes = out_bytes
            .get_mut(..bytes.len())
            .ok_or(anyhow::anyhow!("out_bytes too small"))?;
        out_bytes.copy_from_slice(&bytes);
        Ok(())
    }
}

impl From<EcHdr> for Ec {
    fn from(value: EcHdr) -> Self {
        let EcHdr {
            ec,
            vid_hdr_offset,
            data_offset,
            image_seq,
            ..
        } = value;

        Self {
            ec,
            vid_hdr_offset,
            data_offset,
            image_seq,
        }
    }
}

impl From<Ec> for EcHdr {
    fn from(value: Ec) -> EcHdr {
        let Ec {
            ec,
            vid_hdr_offset,
            data_offset,
            image_seq,
        } = value;

        let mut target = Self {
            magic: UBI_EC_HDR_MAGIC.try_into().unwrap(),
            version: UBI_VERSION,

            ec,
            vid_hdr_offset,
            data_offset,
            image_seq,

            hdr_crc: Default::default(),
            padding1: Default::default(),
            padding2: Default::default(),
        };

        target.fix_crc();
        target
    }
}

/// These represent UBI volume types
#[derive(Debug, Default, Eq, PartialEq, Copy, Clone)]
pub enum VolType {
    /// A volume that may be read and written in random order
    #[default]
    Dynamic,

    /// A volume that is read-only after it is initially written, except for whole-volume updates
    Static,
}

impl From<VolType> for u8 {
    fn from(value: VolType) -> Self {
        match value {
            VolType::Dynamic => 1,
            VolType::Static => 2,
        }
    }
}

impl TryFrom<u8> for VolType {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Dynamic),
            2 => Ok(Self::Static),
            _ => Err(()),
        }
    }
}

/// This represents the specific fields we care about in a VID header
#[derive(Debug, Default, Eq, PartialEq, Copy, Clone)]
pub struct Vid {
    /// The type of volume.
    pub vol_type: VolType,

    /// Whether this PEB was written as a copy of another, for wear-leveling purposes.
    pub copy_flag: bool,

    /// For internal volumes, flags indicating how UBI should handle the volume.
    pub compat: u8,

    /// The ID of the volume, and entry in the volume table.
    pub vol_id: u32,

    /// The offset of the LEB within this volume.
    pub lnum: u32,

    /// For `Static` volumes and copied LEBs, the number of bytes written at the same time as the
    /// VID header, which are thus included in `data_crc`; otherwise 0.
    pub data_size: u32,

    /// The number of LEBs used by this volume, or 0 if this volume is `Dynamic`
    pub used_ebs: u32,

    /// The number of bytes unused at the end of the PEB, to cut the LEB down to a multiple of the
    /// requested volume alignment size.
    pub data_pad: u32,

    /// The CRC of the first `data_size` bytes of the LEB, or 0 when unused.
    pub data_crc: u32,

    /// A unique counter greater than any other VID header written, for resolving `vol_id:lnum`
    /// collisions.
    pub sqnum: u64,
}

impl Vid {
    /// Change the sequence number for this `Vid`
    pub fn sqnum(mut self, sqnum: u64) -> Self {
        self.sqnum = sqnum;
        self
    }

    /// Convert from a byte slice
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        VidHdr::parse(bytes).and_then(|x| x.try_into().ok())
    }

    /// Write into a byte slice
    pub fn encode(self, out_bytes: &mut [u8]) -> anyhow::Result<()> {
        let bytes = VidHdr::from(self).to_bytes()?;
        let out_bytes = out_bytes
            .get_mut(..bytes.len())
            .ok_or(anyhow::anyhow!("out_bytes too small"))?;
        out_bytes.copy_from_slice(&bytes);
        Ok(())
    }
}

impl TryFrom<VidHdr> for Vid {
    type Error = ();

    fn try_from(value: VidHdr) -> Result<Self, Self::Error> {
        let VidHdr {
            vol_type,
            copy_flag,
            compat,
            vol_id,
            lnum,
            data_size,
            used_ebs,
            data_pad,
            data_crc,
            sqnum,
            ..
        } = value;

        let vol_type = vol_type.try_into()?;
        let copy_flag = copy_flag != 0;

        Ok(Self {
            vol_type,
            copy_flag,
            compat,
            vol_id,
            lnum,
            data_size,
            used_ebs,
            data_pad,
            data_crc,
            sqnum,
        })
    }
}

impl From<Vid> for VidHdr {
    fn from(value: Vid) -> VidHdr {
        let Vid {
            vol_type,
            copy_flag,
            compat,
            vol_id,
            lnum,
            data_size,
            used_ebs,
            data_pad,
            data_crc,
            sqnum,
        } = value;

        let vol_type = vol_type.into();
        let copy_flag = copy_flag.into();

        let mut target = Self {
            magic: UBI_VID_HDR_MAGIC.try_into().unwrap(),
            version: UBI_VERSION,

            vol_type,
            copy_flag,
            compat,
            vol_id,
            lnum,
            data_size,
            used_ebs,
            data_pad,
            data_crc,
            sqnum,

            hdr_crc: Default::default(),
            padding1: Default::default(),
            padding2: Default::default(),
            padding3: Default::default(),
        };

        target.fix_crc();
        target
    }
}

/// This represents the specific fields we care about in a volume table record
#[derive(Debug, Default, Eq, PartialEq, Clone)]
pub struct VolTableRecord {
    /// The total number of PEBs allocated to this volume.
    pub reserved_pebs: u32,

    /// All LEBs in this volume will be a multiple of this size.
    pub alignment: u32,

    /// The number of bytes reserved from the end of each PEB to ensure alignment.
    pub data_pad: u32,

    /// The type of volume.
    pub vol_type: VolType,

    /// Set to `true` during a whole-volume update, so that if interrupted, it's possible to detect
    /// that the volume is corrupt.
    pub upd_marker: bool,

    /// The name of the volume. This code supports any UTF-8 string, but as other UBI implementors
    /// might assume only ASCII, it's best to stick to that.
    pub name: String,

    /// Any flags set on this volume.
    pub flags: u8,
}

impl VolTableRecord {
    /// Convert from a byte slice
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (_, vtblrec) = VtblRecord::from_bytes((bytes, 0)).ok()?;
        if !vtblrec.check_crc() {
            return None;
        }
        vtblrec.try_into().ok()
    }

    /// Write into a Vec<u8>
    pub fn into_bytes(self) -> Vec<u8> {
        VtblRecord::from(self).to_bytes().unwrap()
    }

    /// Represent an empty entry in the volume table
    pub fn none_into_bytes() -> Vec<u8> {
        let mut record = VtblRecord {
            reserved_pebs: Default::default(),
            alignment: Default::default(),
            data_pad: Default::default(),
            vol_type: Default::default(),
            upd_marker: Default::default(),
            name: std::array::from_fn(|_| 0u8),
            name_len: Default::default(),
            flags: Default::default(),
            crc: Default::default(),
            padding: Default::default(),
        };
        record.fix_crc();
        record.to_bytes().unwrap()
    }
}

pub trait OptionIntoBytes {
    fn into_bytes(self) -> Vec<u8>;
}

impl OptionIntoBytes for Option<VolTableRecord> {
    fn into_bytes(self) -> Vec<u8> {
        match self {
            Some(x) => x.into_bytes(),
            None => VolTableRecord::none_into_bytes(),
        }
    }
}

impl TryFrom<VtblRecord> for VolTableRecord {
    type Error = ();

    fn try_from(value: VtblRecord) -> Result<Self, Self::Error> {
        let VtblRecord {
            reserved_pebs,
            alignment,
            data_pad,
            vol_type,
            upd_marker,
            name,
            name_len,
            flags,
            ..
        } = value;

        let vol_type = vol_type.try_into()?;
        let upd_marker = upd_marker != 0;
        let name = std::str::from_utf8(&name[..name_len as usize])
            .map_err(|_| ())?
            .to_string();

        Ok(Self {
            reserved_pebs,
            alignment,
            data_pad,
            vol_type,
            upd_marker,
            name,
            flags,
        })
    }
}

impl From<VolTableRecord> for VtblRecord {
    fn from(value: VolTableRecord) -> VtblRecord {
        let VolTableRecord {
            reserved_pebs,
            alignment,
            data_pad,
            vol_type,
            upd_marker,
            name,
            flags,
        } = value;

        let vol_type = vol_type.into();
        let upd_marker = upd_marker.into();
        let name_len = name.len() as _;

        let name_bytes = name.as_bytes();
        let mut name = std::array::from_fn(|_| 0u8);
        name[..name_bytes.len()].copy_from_slice(name_bytes);

        let mut target = Self {
            reserved_pebs,
            alignment,
            data_pad,
            vol_type,
            upd_marker,
            name,
            name_len,
            flags,

            crc: Default::default(),
            padding: Default::default(),
        };

        target.fix_crc();
        target
    }
}

#[test]
fn test_encode() -> anyhow::Result<()> {
    let ec = Ec::default();
    let vid = Vid::default();
    let vtbl = VolTableRecord {
        alignment: 1024,
        name: "example".to_string(),
        ..Default::default()
    };

    let mut buf = vec![0u8; 1024];

    ec.encode(&mut buf)?;
    assert_eq!(Ec::decode(&buf), Some(ec));

    vid.encode(&mut buf)?;
    assert_eq!(Vid::decode(&buf), Some(vid));

    let vec = vtbl.clone().into_bytes();
    assert_eq!(VolTableRecord::decode(&vec), Some(vtbl));

    Ok(())
}
