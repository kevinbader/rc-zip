use super::{
    encoding::{self, Encoding},
    error::*,
    types::*,
};
use chrono::{
    offset::{LocalResult, TimeZone, Utc},
    DateTime,
};
use log::*;
#[macro_use]
mod nom_macros;

use hex_fmt::HexFmt;

use positioned_io::{Cursor, ReadAt};
use std::fmt;
use std::io::Read;

use nom::{
    bytes::complete::{tag, take},
    combinator::{cond, map, verify},
    error::ParseError,
    multi::length_data,
    number::complete::{le_u16, le_u32, le_u64, le_u8},
    sequence::{preceded, tuple},
    IResult, Offset,
};

// Reference code for zip handling:
// https://github.com/itchio/arkive/blob/master/zip/reader.go

#[repr(u16)]
enum ExtraHeaderID {
    /// Zip64 extended information
    Zip64 = 0x0001,
    /// NTFS
    NTFS = 0x000a,
    /// UNIX
    Unix = 0x000d,
    // Extended timestamp
    ExtTime = 0x5455,
    /// Info-ZIP Unix extension
    InfoZipUnix = 0x5855,
}

#[derive(Debug)]
/// 4.3.7 Local file header
struct LocalFileHeaderRecord {
    /// version needed to extract
    reader_version: u16,
    /// general purpose bit flag
    flags: u16,
    /// compression method
    method: u16,
    /// last mod file time
    modified_time: u16,
    /// last mod file date
    modified_date: u16,
    /// crc-32
    crc32: u32,
    /// compressed size
    compressed_size: u32,
    /// uncompressed size
    uncompressed_size: u32,
    // file name
    name: ZipString,
    // extra field
    extra: ZipBytes,
}

impl LocalFileHeaderRecord {
    /// Does not include filename size & data, extra size & data
    const LENGTH: usize = 30;
    const SIGNATURE: &'static str = "PK\x03\x04";
}

// 4.3.12 Central directory structure: File header
#[derive(Debug)]
struct DirectoryHeader {
    // version made by
    creator_version: u16,
    // version needed to extract
    reader_version: u16,
    // general purpose bit flag
    flags: u16,
    // compression method
    method: u16,
    // last mod file time
    modified_time: u16,
    // last mod file date
    modified_date: u16,
    // crc32
    crc32: u32,
    // compressed size
    compressed_size: u32,
    // uncompressed size
    uncompressed_size: u32,
    // disk number start
    disk_nbr_start: u16,
    // internal file attributes
    internal_attrs: u16,
    // external file attributes
    external_attrs: u32,
    // relative offset of local header
    header_offset: u32,

    // name
    name: ZipString,
    // extra
    extra: ZipBytes,
    // comment
    comment: ZipString,
}

impl DirectoryHeader {
    const SIGNATURE: &'static str = "PK\x01\x02";

    fn parse<'a, E: ParseError<&'a [u8]>>(i: &'a [u8]) -> IResult<&'a [u8], Self, E> {
        preceded(
            tag(Self::SIGNATURE),
            fields!({
                creator_version: le_u16,
                reader_version: le_u16,
                flags: le_u16,
                method: le_u16,
                modified_time: le_u16,
                modified_date: le_u16,
                crc32: le_u32,
                compressed_size: le_u32,
                uncompressed_size: le_u32,
                name_len: le_u16,
                extra_len: le_u16,
                comment_len: le_u16,
                disk_nbr_start: le_u16,
                internal_attrs: le_u16,
                external_attrs: le_u32,
                header_offset: le_u32,
            } chain {
                fields!({
                    name: zip_string(name_len),
                    extra: zip_bytes(extra_len),
                    comment: zip_string(comment_len),
                } map Self {
                    creator_version,
                    reader_version,
                    flags,
                    method,
                    modified_time,
                    modified_date,
                    crc32,
                    compressed_size,
                    uncompressed_size,
                    disk_nbr_start,
                    internal_attrs,
                    external_attrs,
                    header_offset,
                    name: name,
                    extra: extra,
                    comment: comment,
                })
            }),
        )(i)
    }
}

struct ExtraFieldRecord<'a> {
    tag: u16,
    payload: &'a [u8],
}

impl<'a> fmt::Debug for ExtraFieldRecord<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "tag 0x{:x}: {}", self.tag, HexFmt(self.payload))
    }
}

impl<'a> ExtraFieldRecord<'a> {
    fn parse(i: &'a [u8]) -> ZipParseResult<'a, Self> {
        fields!(Self {
            tag: le_u16,
            payload: length_data(le_u16),
        })(i)
    }
}

// Useful because zip64 extended information extra field has fixed order *but*
// optional fields. From the appnote:
//
// If one of the size or offset fields in the Local or Central directory record
// is too small to hold the required data, a Zip64 extended information record
// is created. The order of the fields in the zip64 extended information record
// is fixed, but the fields MUST only appear if the corresponding Local or
// Central directory record field is set to 0xFFFF or 0xFFFFFFFF.
struct ExtraFieldSettings {
    needs_uncompressed_size: bool,
    needs_compressed_size: bool,
    needs_header_offset: bool,
}

#[derive(Debug)]
enum ExtraField {
    Zip64(ExtraZip64Field),
    Timestamp(ExtraTimestampField),
    Unknown { tag: u16 },
}

impl ExtraField {
    fn parse<'a>(i: &'a [u8], settings: &ExtraFieldSettings) -> ZipParseResult<'a, Self> {
        use ExtraField as EF;

        let (remaining, rec) = ExtraFieldRecord::parse(i)?;
        debug!("Got extra field record: {:#?}", rec);

        let variant = match rec.tag {
            ExtraZip64Field::TAG => {
                if let Ok((_, tag)) = ExtraZip64Field::parse(rec.payload, settings) {
                    Some(EF::Zip64(tag))
                } else {
                    None
                }
            }
            ExtraTimestampField::TAG => {
                if let Ok((_, tag)) = ExtraTimestampField::parse(rec.payload) {
                    Some(EF::Timestamp(tag))
                } else {
                    None
                }
            }
            _ => None,
        }
        .unwrap_or(EF::Unknown { tag: rec.tag });

        Ok((remaining, variant))
    }
}

/// 4.5.3 -Zip64 Extended Information Extra Field (0x0001)
#[derive(Debug)]
struct ExtraZip64Field {
    uncompressed_size: Option<u64>,
    compressed_size: Option<u64>,
    header_offset: Option<u64>,
}

impl ExtraZip64Field {
    const TAG: u16 = 0x0001;

    fn parse<'a>(i: &'a [u8], settings: &ExtraFieldSettings) -> ZipParseResult<'a, Self> {
        // N.B: we ignore "disk start number"
        fields!(Self {
            uncompressed_size: cond(settings.needs_uncompressed_size, le_u64),
            compressed_size: cond(settings.needs_compressed_size, le_u64),
            header_offset: cond(settings.needs_header_offset, le_u64),
        })(i)
    }
}

/// Extended timestamp extra field
#[derive(Debug)]
struct ExtraTimestampField {
    seconds_since_epoch: u32,
}

impl ExtraTimestampField {
    const TAG: u16 = 0x5455;

    fn parse<'a>(i: &'a [u8]) -> ZipParseResult<'a, Self> {
        preceded(
            // note: no idea why there needs to be an u8 with the
            // lsb set - but that's what Go's archive/zip checks for
            // before using this as an extra timstamp field
            verify(le_u8, |x| x & 1 != 0),
            map(le_u32, |seconds_since_epoch| Self {
                seconds_since_epoch,
            }),
        )(i)
    }
}

fn convert_dos_date_and_time(dos_date: u16, dos_time: u16) -> Option<DateTime<Utc>> {
    // see https://docs.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-dosdatetimetofiletime
    let date = match {
        // bits 0-4: day of the month (1-31)
        let d = (dos_date & 0b1111_1) as u32;
        // bits 5-8: month (1 = january, 2 = february and so on)
        let m = ((dos_date >> 5) & 0b1111) as u32;
        // bits 9-15: year offset from 1980
        let y = ((dos_date >> 9) + 1980) as i32;
        Utc.ymd_opt(y, m, d)
    } {
        LocalResult::Single(date) => date,
        _ => return None,
    };

    // bits 0-4: second divided by 2
    let s = (dos_time & 0b1111_1) as u32 * 2;
    // bits 5-10: minute (0-59)
    let m = (dos_time >> 5 & 0b1111_11) as u32;
    // bits 11-15: hour (0-23 on a 24-hour clock)
    let h = (dos_time >> 11) as u32;
    date.and_hms_opt(h, m, s)
}

impl DirectoryHeader {
    fn is_non_utf8(&self) -> bool {
        let (valid1, require1) = encoding::detect_utf8(&self.name.0[..]);
        let (valid2, require2) = encoding::detect_utf8(&self.comment.0[..]);
        if !valid1 || !valid2 {
            // definitely not utf-8
            return true;
        }

        if !require1 && !require2 {
            // name and comment only use single-byte runes that overlap with UTF-8
            return false;
        }

        // Might be UTF-8, might be some other encoding; preserve existing flag.
        // Some ZIP writers use UTF-8 encoding without setting the UTF-8 flag.
        // Since it is impossible to always distinguish valid UTF-8 from some
        // other encoding (e.g., GBK or Shift-JIS), we trust the flag.
        self.flags & 0x800 == 0
    }

    fn as_stored_entry(&self, encoding: Encoding) -> Result<StoredEntry, Error> {
        let mut comment: Option<String> = None;
        if let Some(comment_field) = self.comment.clone().as_option() {
            comment = Some(encoding.decode(&comment_field.0)?);
        }

        let mut compressed_size = self.compressed_size as u64;
        let mut uncompressed_size = self.uncompressed_size as u64;
        let mut header_offset = self.header_offset as u64;
        let mut modified: Option<DateTime<Utc>> = None;

        let settings = ExtraFieldSettings {
            needs_compressed_size: self.uncompressed_size == !0u32,
            needs_uncompressed_size: self.compressed_size == !0u32,
            needs_header_offset: self.header_offset == !0u32,
        };

        let mut slice = &self.extra.0[..];
        while slice.len() > 0 {
            debug!("slice = {}", HexFmt(slice));
            match ExtraField::parse(&slice[..], &settings) {
                Ok((remaining, ef)) => {
                    debug!("extra field = {:#?}", ef);
                    match ef {
                        ExtraField::Zip64(z64) => {
                            if let Some(n) = z64.uncompressed_size {
                                uncompressed_size = n;
                            }
                            if let Some(n) = z64.compressed_size {
                                compressed_size = n;
                            }
                            if let Some(n) = z64.header_offset {
                                header_offset = n;
                            }
                        }
                        ExtraField::Timestamp(ts) => {
                            modified = Some(Utc.timestamp(ts.seconds_since_epoch as i64, 0));
                        }
                        _ => {}
                    };
                    slice = remaining;
                }
                Err(e) => {
                    debug!("extra field error: {:#?}", e);
                    return Err(FormatError::InvalidExtraField.into());
                }
            }
        }

        let modified = match modified {
            Some(m) => Some(m),
            None => convert_dos_date_and_time(self.modified_date, self.modified_time),
        };

        Ok(StoredEntry {
            entry: Entry {
                name: encoding.decode(&self.name.0)?,
                method: self.method.into(),
                comment,
                modified: modified.unwrap_or_else(|| zero_datetime()),
            },

            creator_version: self.creator_version,
            reader_version: self.reader_version,
            flags: self.flags,

            crc32: self.crc32,
            compressed_size,
            uncompressed_size,
            header_offset,

            external_attrs: self.external_attrs,
        })
    }
}

#[derive(Debug)]
/// 4.3.16  End of central directory record:
struct EndOfCentralDirectoryRecord {
    /// number of this disk
    disk_nbr: u16,
    /// number of the disk with the start of the central directory
    dir_disk_nbr: u16,
    /// total number of entries in the central directory on this disk
    dir_records_this_disk: u16,
    /// total number of entries in the central directory
    directory_records: u16,
    // size of the central directory
    directory_size: u32,
    /// offset of start of central directory with respect to the starting disk number
    directory_offset: u32,
    /// .ZIP file comment
    comment: ZipString,
}

impl EndOfCentralDirectoryRecord {
    /// does not include comment size & comment data
    const LENGTH: usize = 20;
    const SIGNATURE: &'static str = "PK\x05\x06";

    fn find_in_block(b: &[u8]) -> Option<Located<Self>> {
        for i in (0..(b.len() - Self::LENGTH + 1)).rev() {
            let slice = &b[i..];

            if let Ok((_, directory)) = Self::parse::<ZipParseError>(slice) {
                return Some(Located {
                    offset: i as u64,
                    inner: directory,
                });
            }
        }
        None
    }

    fn parse<'a, E: ParseError<&'a [u8]>>(i: &'a [u8]) -> IResult<&'a [u8], Self, E> {
        preceded(
            tag(Self::SIGNATURE),
            map(
                tuple((
                    le_u16,
                    le_u16,
                    le_u16,
                    le_u16,
                    le_u32,
                    le_u32,
                    length_data(le_u16),
                )),
                |(
                    disk_nbr,
                    dir_disk_nbr,
                    dir_records_this_disk,
                    directory_records,
                    directory_size,
                    directory_offset,
                    comment,
                )| Self {
                    disk_nbr,
                    dir_disk_nbr,
                    dir_records_this_disk,
                    directory_records,
                    directory_size,
                    directory_offset,
                    comment: comment.into(),
                },
            ),
        )(i)
    }
}

#[derive(Debug)]
/// 4.3.15 Zip64 end of central directory locator
struct EndOfCentralDirectory64Locator {
    /// number of the disk with the start of the zip64 end of central directory
    dir_disk_number: u32,
    /// relative offset of the zip64 end of central directory record
    directory_offset: u64,
    /// total number of disks
    total_disks: u32,
}

impl EndOfCentralDirectory64Locator {
    const LENGTH: usize = 20;
    const SIGNATURE: &'static str = "PK\x06\x07";

    fn parse<'a, E: ParseError<&'a [u8]>>(i: &'a [u8]) -> IResult<&'a [u8], Self, E> {
        preceded(
            tag(Self::SIGNATURE),
            fields!(Self {
                dir_disk_number: le_u32,
                directory_offset: le_u64,
                total_disks: le_u32,
            }),
        )(i)
    }
}

#[derive(Debug)]
/// 4.3.14  Zip64 end of central directory record
struct EndOfCentralDirectory64Record {
    /// size of zip64 end of central directory record
    record_size: u64,
    /// version made by
    creator_version: u16,
    /// version needed to extract
    reader_version: u16,
    /// number of this disk
    disk_nbr: u32,
    /// number of the disk with the start of the central directory
    dir_disk_nbr: u32,
    // total number of entries in the central directory on this disk
    dir_records_this_disk: u64,
    // total number of entries in the central directory
    directory_records: u64,
    // size of the central directory
    directory_size: u64,
    // offset of the start of central directory with respect to the
    // starting disk number
    directory_offset: u64,
}

impl EndOfCentralDirectory64Record {
    const LENGTH: usize = 56;
    const SIGNATURE: &'static str = "PK\x06\x06";

    fn parse<'a, E: ParseError<&'a [u8]>>(
        i: &'a [u8],
    ) -> IResult<&'a [u8], EndOfCentralDirectory64Record, E> {
        preceded(
            tag(Self::SIGNATURE),
            fields!(Self {
                record_size: le_u64,
                creator_version: le_u16,
                reader_version: le_u16,
                disk_nbr: le_u32,
                dir_disk_nbr: le_u32,
                dir_records_this_disk: le_u64,
                directory_records: le_u64,
                directory_size: le_u64,
                directory_offset: le_u64,
            }),
        )(i)
    }
}

#[derive(Debug)]
struct Located<T> {
    offset: u64,
    inner: T,
}

impl<T> std::ops::Deref for Located<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> std::ops::DerefMut for Located<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[derive(Debug)]
/// Coalesces zip and zip64 "end of central directory" record info
struct EndOfCentralDirectory {
    dir: Located<EndOfCentralDirectoryRecord>,
    dir64: Option<Located<EndOfCentralDirectory64Record>>,
    global_offset: i64,
}

impl EndOfCentralDirectory {
    fn new(
        size: u64,
        dir: Located<EndOfCentralDirectoryRecord>,
        dir64: Option<Located<EndOfCentralDirectory64Record>>,
    ) -> Result<Self, Error> {
        let mut res = Self {
            dir,
            dir64,
            global_offset: 0,
        };

        //
        // Pure .zip files look like this:
        // ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
        //                     <------directory_size----->
        // [ Data 1 ][ Data 2 ][    Central directory    ][ ??? ]
        // ^                   ^                          ^
        // 0                   directory_offset           directory_end_offset
        // ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
        //
        // But there exist some valid zip archives with padding at the beginning, like so:
        // ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
        // <--global_offset->                    <------directory_size----->
        // [    Padding     ][ Data 1 ][ Data 2 ][    Central directory    ][ ??? ]
        // ^                 ^                   ^                         ^
        // 0                 global_offset       computed_directory_offset directory_end_offset
        // ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
        //
        // (e.g. https://www.icculus.org/mojosetup/ installers are ELF binaries with a .zip file appended)
        //
        // `directory_end_offfset` is found by scanning the file (so it accounts for padding), but
        // `directory_offset` is found by reading a data structure (so it does not account for padding).
        // If we just trusted `directory_offset`, we'd be reading the central directory at the wrong place:
        // ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
        //                                       <------directory_size----->
        // [    Padding     ][ Data 1 ][ Data 2 ][    Central directory    ][ ??? ]
        // ^                   ^                                           ^
        // 0                   directory_offset - woops!                   directory_end_offset
        // ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

        let computed_directory_offset = res.located_directory_offset() - res.directory_size();

        // did we find a valid offset?
        if (0..size).contains(&computed_directory_offset) {
            // that's different from the recorded one?
            if computed_directory_offset != res.directory_offset() {
                // then assume the whole file is offset
                res.global_offset =
                    computed_directory_offset as i64 - res.directory_offset() as i64;
                res.set_directory_offset(computed_directory_offset);
            }
        }

        // make sure directory_offset points to somewhere in our file
        debug!(
            "directory offset = {}, valid range = 0..{}",
            res.directory_offset(),
            size
        );
        if !(0..size).contains(&res.directory_offset()) {
            return Err(FormatError::DirectoryOffsetPointsOutsideFile.into());
        }

        Ok(res)
    }

    fn located_directory_offset(&self) -> u64 {
        match self.dir64.as_ref() {
            Some(d64) => d64.offset,
            None => self.dir.offset,
        }
    }

    fn directory_offset(&self) -> u64 {
        match self.dir64.as_ref() {
            Some(d64) => d64.directory_offset,
            None => self.dir.directory_offset as u64,
        }
    }

    fn directory_size(&self) -> u64 {
        match self.dir64.as_ref() {
            Some(d64) => d64.directory_size,
            None => self.dir.directory_size as u64,
        }
    }

    fn set_directory_offset(&mut self, offset: u64) {
        match self.dir64.as_mut() {
            Some(d64) => d64.directory_offset = offset,
            None => self.dir.directory_offset = offset as u32,
        };
    }

    fn directory_records(&self) -> u64 {
        match self.dir64.as_ref() {
            Some(d64) => d64.directory_records,
            None => self.dir.directory_records as u64,
        }
    }

    fn comment(&self) -> &ZipString {
        &self.dir.comment
    }
}

pub fn zip_string<'a, C, E>(count: C) -> impl Fn(&'a [u8]) -> IResult<&'a [u8], ZipString, E>
where
    C: nom::ToUsize,
    E: ParseError<&'a [u8]>,
{
    move |i: &'a [u8]| {
        map(take(count.to_usize()), |slice: &'a [u8]| {
            ZipString::from(slice)
        })(i)
    }
}

pub fn zip_bytes<'a, C, E>(count: C) -> impl Fn(&'a [u8]) -> IResult<&'a [u8], ZipBytes, E>
where
    C: nom::ToUsize,
    E: ParseError<&'a [u8]>,
{
    move |i: &'a [u8]| {
        map(take(count.to_usize()), |slice: &'a [u8]| {
            ZipBytes(slice.into())
        })(i)
    }
}

pub struct ArchiveReader {
    // Size of the entire zip file
    size: u64,
    state: ArchiveReaderState,

    buffer: Buffer,
}

#[derive(Debug)]
pub enum ArchiveReaderResult {
    /// should continue
    Continue,
    /// done reading
    Done(Archive),
}

enum ArchiveReaderState {
    /// Dummy state, used while transitioning because
    /// ownership rules are tough.
    Transitioning,
    ReadEocd {
        haystack_size: u64,
    },
    ReadEocd64Locator {
        eocdr: Located<EndOfCentralDirectoryRecord>,
    },
    ReadEocd64 {
        eocdr64_offset: u64,
        eocdr: Located<EndOfCentralDirectoryRecord>,
    },
    ReadCentralDirectory {
        eocd: EndOfCentralDirectory,
        directory_headers: Vec<DirectoryHeader>,
    },
    Done,
}

macro_rules! transition {
    ($state: expr => ($pattern: pat) $body: expr) => {
        $state = if let $pattern = std::mem::replace(&mut $state, S::Transitioning) {
            $body
        } else {
            unreachable!()
        };
    };
}

struct ReadOp {
    offset: u64,
    length: u64,
}

struct Buffer {
    buffer: circular::Buffer,
    read_bytes: u64,
}

impl Buffer {
    pub fn with_capacity(size: usize) -> Self {
        Self {
            buffer: circular::Buffer::with_capacity(size),
            read_bytes: 0,
        }
    }

    /// resets the buffer (so that data() returns an empty slice,
    /// and space() returns the full capacity), along with th e
    /// read bytes counter.
    fn reset(&mut self) {
        self.read_bytes = 0;
        self.buffer.reset();
    }

    /// returns the number of read bytes since the last reset
    fn read_bytes(&self) -> u64 {
        self.read_bytes
    }

    /// returns a slice with all the available data
    fn data(&self) -> &[u8] {
        self.buffer.data()
    }

    /// advances the position tracker
    ///
    /// if the position gets past the buffer's half,
    /// this will call `shift()` to move the remaining data
    /// to the beginning of the buffer
    fn consume(&mut self, count: usize) -> usize {
        self.buffer.consume(count)
    }

    /// fill that buffer from the given Read
    fn read(&mut self, rd: &mut Read) -> Result<usize, std::io::Error> {
        match rd.read(self.buffer.space()) {
            Ok(written) => {
                self.read_bytes += written as u64;
                self.buffer.fill(written);
                Ok(written)
            }
            Err(e) => Err(e),
        }
    }
}

impl ArchiveReader {
    pub fn new(size: u64) -> Self {
        let haystack_size: u64 = 65 * 1024;
        let haystack_size = if size < haystack_size {
            size
        } else {
            haystack_size
        };

        Self {
            size,
            state: ArchiveReaderState::ReadEocd { haystack_size },
            buffer: Buffer::with_capacity(128 * 1024), // 128KB buffer
        }
    }

    pub fn wants_read(&self) -> Option<u64> {
        match self.read_op() {
            Some(op) => self.read_op_state(op),
            None => None,
        }
    }

    fn read_op(&self) -> Option<ReadOp> {
        use ArchiveReaderState as S;
        match self.state {
            S::ReadEocd { haystack_size } => Some(ReadOp {
                offset: self.size - haystack_size,
                length: haystack_size,
            }),
            S::ReadEocd64Locator { ref eocdr } => {
                let length = EndOfCentralDirectory64Locator::LENGTH as u64;
                Some(ReadOp {
                    offset: eocdr.offset - length,
                    length,
                })
            }
            S::ReadEocd64 { eocdr64_offset, .. } => {
                let length = EndOfCentralDirectory64Record::LENGTH as u64;
                Some(ReadOp {
                    offset: eocdr64_offset,
                    length,
                })
            }
            S::ReadCentralDirectory { ref eocd, .. } => Some(ReadOp {
                offset: eocd.directory_offset(),
                length: eocd.directory_size(),
            }),
            S::Done { .. } => panic!("Called wants_read() on ArchiveReader in Done state"),
            S::Transitioning => unreachable!(),
        }
    }

    fn read_op_state(&self, op: ReadOp) -> Option<u64> {
        let read_bytes = self.buffer.read_bytes();
        if read_bytes < op.length {
            Some(op.offset + read_bytes)
        } else {
            None
        }
    }

    pub fn read(&mut self, rd: &mut Read) -> Result<usize, std::io::Error> {
        self.buffer.read(rd)
    }

    pub fn process(&mut self) -> Result<ArchiveReaderResult, Error> {
        use ArchiveReaderResult as R;
        use ArchiveReaderState as S;
        match self.state {
            S::ReadEocd { haystack_size } => {
                if self.buffer.read_bytes() < haystack_size {
                    return Ok(R::Continue);
                }

                match {
                    let haystack = &self.buffer.data()[..haystack_size as usize];
                    EndOfCentralDirectoryRecord::find_in_block(haystack)
                } {
                    None => Err(FormatError::DirectoryEndSignatureNotFound.into()),
                    Some(mut eocdr) => {
                        self.buffer.reset();
                        eocdr.offset += self.size - haystack_size;

                        if eocdr.offset < EndOfCentralDirectory64Locator::LENGTH as u64 {
                            // no room for an EOCD64 locator, definitely not a zip64 file
                            self.state = S::ReadCentralDirectory {
                                eocd: EndOfCentralDirectory::new(self.size, eocdr, None)?,
                                directory_headers: vec![],
                            };
                            Ok(R::Continue)
                        } else {
                            self.buffer.reset();
                            self.state = S::ReadEocd64Locator { eocdr };
                            Ok(R::Continue)
                        }
                    }
                }
            }
            S::ReadEocd64Locator { .. } => {
                match EndOfCentralDirectory64Locator::parse::<ZipParseError>(self.buffer.data()) {
                    Err(nom::Err::Incomplete(_)) => {
                        // need more data
                        Ok(R::Continue)
                    }
                    Err(nom::Err::Error(_)) | Err(nom::Err::Failure(_)) => {
                        // we don't have a zip64 end of central directory locator - that's ok!
                        self.buffer.reset();
                        transition!(self.state => (S::ReadEocd64Locator {eocdr}) {
                            S::ReadCentralDirectory {
                                eocd: EndOfCentralDirectory::new(self.size, eocdr, None)?,
                                directory_headers: vec![],
                            }
                        });
                        Ok(R::Continue)
                    }
                    Ok((_, locator)) => {
                        self.buffer.reset();
                        transition!(self.state => (S::ReadEocd64Locator {eocdr}) {
                            S::ReadEocd64 {
                                eocdr64_offset: locator.directory_offset,
                                eocdr,
                            }
                        });
                        Ok(R::Continue)
                    }
                }
            }
            S::ReadEocd64 { .. } => {
                match EndOfCentralDirectory64Record::parse::<ZipParseError>(self.buffer.data()) {
                    Err(nom::Err::Incomplete(_)) => {
                        // need more data
                        Ok(R::Continue)
                    }
                    Err(nom::Err::Error(_)) | Err(nom::Err::Failure(_)) => {
                        // at this point, we really expected to have a zip64 end
                        // of central directory record, so, we want to propagate
                        // that error.
                        Err(FormatError::Directory64EndRecordInvalid.into())
                    }
                    Ok((_, eocdr64)) => {
                        self.buffer.reset();
                        transition!(self.state => (S::ReadEocd64 { eocdr, eocdr64_offset }) {
                            S::ReadCentralDirectory {
                                eocd: EndOfCentralDirectory::new(self.size, eocdr, Some(Located {
                                    offset: eocdr64_offset,
                                    inner: eocdr64
                                }))?,
                                directory_headers: vec![],
                            }
                        });
                        Ok(R::Continue)
                    }
                }
            }
            S::ReadCentralDirectory {
                ref eocd,
                ref mut directory_headers,
            } => {
                match DirectoryHeader::parse::<ZipParseError>(self.buffer.data()) {
                    Err(nom::Err::Incomplete(_needed)) => {
                        // TODO: couldn't this happen when we have 0 bytes available?

                        // need more data
                        Ok(R::Continue)
                    }
                    Err(nom::Err::Error(_)) | Err(nom::Err::Failure(_)) => {
                        // this is the normal end condition when reading
                        // the central directory (due to 65536-entries non-zip64 files)
                        // let's just check a few numbers first.

                        // only compare 16 bits here
                        if (directory_headers.len() as u16) == (eocd.directory_records() as u16) {
                            let mut detector = chardet::UniversalDetector::new();
                            let mut all_utf8 = true;

                            {
                                let max_feed: usize = 4096;
                                let mut total_fed: usize = 0;
                                let mut feed = |slice: &[u8]| {
                                    detector.feed(slice);
                                    total_fed += slice.len();
                                    total_fed < max_feed
                                };

                                'recognize_encoding: for fh in
                                    directory_headers.iter().filter(|fh| fh.is_non_utf8())
                                {
                                    all_utf8 = false;
                                    if !feed(&fh.name.0) || !feed(&fh.comment.0) {
                                        break 'recognize_encoding;
                                    }
                                }
                            }

                            let encoding = {
                                if all_utf8 {
                                    Encoding::Utf8
                                } else {
                                    let (charset, confidence, _language) = detector.close();
                                    let label = chardet::charset2encoding(&charset);
                                    debug!(
                                        "Detected charset {} with confidence {}",
                                        label, confidence
                                    );

                                    match label {
                                        "SHIFT_JIS" => Encoding::ShiftJis,
                                        "utf-8" => Encoding::Utf8,
                                        _ => Encoding::Cp437,
                                    }
                                }
                            };

                            let entries: Result<Vec<StoredEntry>, Error> = directory_headers
                                .into_iter()
                                .map(|x| x.as_stored_entry(encoding))
                                .collect();
                            let entries = entries?;

                            let mut comment: Option<String> = None;
                            if !eocd.comment().0.is_empty() {
                                comment = Some(encoding.decode(&eocd.comment().0)?);
                            }

                            self.state = S::Done;
                            Ok(R::Done(Archive {
                                size: self.size,
                                comment,
                                entries,
                                encoding,
                            }))
                        } else {
                            // if we read the wrong number of directory entries,
                            // error out.
                            Err(FormatError::InvalidCentralRecord.into())
                        }
                    }
                    Ok((remaining, dh)) => {
                        let consumed = self.buffer.data().offset(remaining);
                        drop(remaining);
                        self.buffer.consume(consumed);
                        directory_headers.push(dh);
                        Ok(R::Continue)
                    }
                }
            }
            S::Done { .. } => panic!("Called process() on ArchiveReader in Done state"),
            S::Transitioning => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub struct Archive {
    size: u64,
    encoding: Encoding,
    entries: Vec<StoredEntry>,
    comment: Option<String>,
}

impl Archive {
    pub fn read(rd: &ReadAt, size: u64) -> Result<Self, Error> {
        let mut ar = ArchiveReader::new(size);
        loop {
            if let Some(offset) = ar.wants_read() {
                match ar.read(&mut Cursor::new_pos(&rd, offset)) {
                    Ok(read_bytes) => {
                        if read_bytes == 0 {
                            return Err(Error::IO(std::io::ErrorKind::UnexpectedEof.into()));
                        }
                    }
                    Err(err) => return Err(Error::IO(err)),
                }
            }

            match ar.process()? {
                ArchiveReaderResult::Done(archive) => return Ok(archive),
                ArchiveReaderResult::Continue => {}
            }
        }
    }

    /// Return a list of all files in this zip, read from the
    /// central directory.
    pub fn entries(&self) -> &[StoredEntry] {
        &self.entries[..]
    }

    /// Returns the detected character encoding for text fields
    /// (paths, comments) inside this ZIP file
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    pub fn comment(&self) -> Option<&String> {
        self.comment.as_ref()
    }

    pub fn by_name<N: AsRef<str>>(&self, name: N) -> Option<&StoredEntry> {
        self.entries.iter().find(|&x| x.name() == name.as_ref())
    }
}
