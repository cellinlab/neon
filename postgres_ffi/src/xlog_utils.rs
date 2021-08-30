//
// This file contains common utilities for dealing with PostgreSQL WAL files and
// LSNs.
//
// Many of these functions have been copied from PostgreSQL, and rewritten in
// Rust. That's why they don't follow the usual Rust naming conventions, they
// have been named the same as the corresponding PostgreSQL functions instead.
//

use crate::pg_constants;
use crate::CheckPoint;
use crate::ControlFileData;
use crate::FullTransactionId;
use crate::XLogLongPageHeaderData;
use crate::XLogPageHeaderData;
use crate::XLogRecord;
use crate::XLOG_PAGE_MAGIC;

use byteorder::{ByteOrder, LittleEndian};
use bytes::{Buf, Bytes};
use bytes::{BufMut, BytesMut};
use crc32c::*;
use log::*;
use std::cmp::min;
use std::fs::{self, File};
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const XLOG_FNAME_LEN: usize = 24;
pub const XLOG_BLCKSZ: usize = 8192;
pub const XLP_FIRST_IS_CONTRECORD: u16 = 0x0001;
pub const XLP_REM_LEN_OFFS: usize = 2 + 2 + 4 + 8;
pub const XLOG_RECORD_CRC_OFFS: usize = 4 + 4 + 8 + 1 + 1 + 2;
pub const MAX_SEND_SIZE: usize = XLOG_BLCKSZ * 16;

pub const XLOG_SIZE_OF_XLOG_SHORT_PHD: usize = std::mem::size_of::<XLogPageHeaderData>();
pub const XLOG_SIZE_OF_XLOG_LONG_PHD: usize = std::mem::size_of::<XLogLongPageHeaderData>();
pub const XLOG_SIZE_OF_XLOG_RECORD: usize = std::mem::size_of::<XLogRecord>();
pub const SIZE_OF_XLOG_RECORD_DATA_HEADER_SHORT: usize = 1 * 2;

pub type XLogRecPtr = u64;
pub type TimeLineID = u32;
pub type TimestampTz = i64;
pub type XLogSegNo = u64;

const XID_CHECKPOINT_INTERVAL: u32 = 1024;

#[allow(non_snake_case)]
pub fn XLogSegmentsPerXLogId(wal_segsz_bytes: usize) -> XLogSegNo {
    (0x100000000u64 / wal_segsz_bytes as u64) as XLogSegNo
}

#[allow(non_snake_case)]
pub fn XLogSegNoOffsetToRecPtr(
    segno: XLogSegNo,
    offset: u32,
    wal_segsz_bytes: usize,
) -> XLogRecPtr {
    segno * (wal_segsz_bytes as u64) + (offset as u64)
}

#[allow(non_snake_case)]
pub fn XLogFileName(tli: TimeLineID, logSegNo: XLogSegNo, wal_segsz_bytes: usize) -> String {
    return format!(
        "{:>08X}{:>08X}{:>08X}",
        tli,
        logSegNo / XLogSegmentsPerXLogId(wal_segsz_bytes),
        logSegNo % XLogSegmentsPerXLogId(wal_segsz_bytes)
    );
}

#[allow(non_snake_case)]
pub fn XLogFromFileName(fname: &str, wal_seg_size: usize) -> (XLogSegNo, TimeLineID) {
    let tli = u32::from_str_radix(&fname[0..8], 16).unwrap();
    let log = u32::from_str_radix(&fname[8..16], 16).unwrap() as XLogSegNo;
    let seg = u32::from_str_radix(&fname[16..24], 16).unwrap() as XLogSegNo;
    (log * XLogSegmentsPerXLogId(wal_seg_size) + seg, tli)
}

#[allow(non_snake_case)]
pub fn IsXLogFileName(fname: &str) -> bool {
    return fname.len() == XLOG_FNAME_LEN && fname.chars().all(|c| c.is_ascii_hexdigit());
}

#[allow(non_snake_case)]
pub fn IsPartialXLogFileName(fname: &str) -> bool {
    fname.ends_with(".partial") && IsXLogFileName(&fname[0..fname.len() - 8])
}

pub fn get_current_timestamp() -> TimestampTz {
    const UNIX_EPOCH_JDATE: u64 = 2440588; /* == date2j(1970, 1, 1) */
    const POSTGRES_EPOCH_JDATE: u64 = 2451545; /* == date2j(2000, 1, 1) */
    const SECS_PER_DAY: u64 = 86400;
    const USECS_PER_SEC: u64 = 1000000;
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(n) => {
            ((n.as_secs() - ((POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * SECS_PER_DAY))
                * USECS_PER_SEC
                + n.subsec_micros() as u64) as i64
        }
        Err(_) => panic!("SystemTime before UNIX EPOCH!"),
    }
}

fn find_end_of_wal_segment(
    data_dir: &Path,
    segno: XLogSegNo,
    tli: TimeLineID,
    wal_seg_size: usize,
    is_partial: bool,
    rec_offs: &mut usize,
    rec_hdr: &mut [u8; XLOG_SIZE_OF_XLOG_RECORD],
    crc: &mut u32,
    check_contrec: bool,
) -> u32 {
    let mut offs: usize = 0;
    let mut contlen: usize = 0;
    let mut buf = [0u8; XLOG_BLCKSZ];
    let file_name = XLogFileName(tli, segno, wal_seg_size);
    let mut last_valid_rec_pos: usize = 0;
    let file_path = data_dir.join(if is_partial {
        file_name.clone() + ".partial"
    } else {
        file_name
    });
    let mut file = File::open(&file_path).unwrap();

    while offs < wal_seg_size {
        if offs % XLOG_BLCKSZ == 0 {
            if let Ok(bytes_read) = file.read(&mut buf) {
                if bytes_read != buf.len() {
                    break;
                }
            } else {
                break;
            }
            let xlp_magic = LittleEndian::read_u16(&buf[0..2]);
            let xlp_info = LittleEndian::read_u16(&buf[2..4]);
            let xlp_rem_len = LittleEndian::read_u32(&buf[XLP_REM_LEN_OFFS..XLP_REM_LEN_OFFS + 4]);
            if xlp_magic != XLOG_PAGE_MAGIC as u16 {
                info!("Invalid WAL file {:?} magic {}", &file_path, xlp_magic);
                break;
            }
            if offs == 0 {
                offs = XLOG_SIZE_OF_XLOG_LONG_PHD;
                if (xlp_info & XLP_FIRST_IS_CONTRECORD) != 0 {
                    if check_contrec {
                        let xl_tot_len = LittleEndian::read_u32(&rec_hdr[0..4]) as usize;
                        contlen = xlp_rem_len as usize;
                        if *rec_offs + contlen < xl_tot_len
                            || (*rec_offs + contlen != xl_tot_len
                                && contlen != XLOG_BLCKSZ - XLOG_SIZE_OF_XLOG_LONG_PHD)
                        {
                            info!(
                                "Corrupted continuation record: offs={}, contlen={}, xl_tot_len={}",
                                *rec_offs, contlen, xl_tot_len
                            );
                            return 0;
                        }
                    } else {
                        offs += ((xlp_rem_len + 7) & !7) as usize;
                    }
                } else if *rec_offs != 0 {
                    // There is incompleted page at previous segment but no cont record:
                    // it means that current segment is not valid and we have to return back.
                    info!("CONTRECORD flag is missed in page header");
                    return 0;
                }
            } else {
                offs += XLOG_SIZE_OF_XLOG_SHORT_PHD;
            }
        } else if contlen == 0 {
            let page_offs = offs % XLOG_BLCKSZ;
            let xl_tot_len = LittleEndian::read_u32(&buf[page_offs..page_offs + 4]) as usize;
            if xl_tot_len == 0 {
                break;
            }
            offs += 4;
            *rec_offs = 4;
            contlen = xl_tot_len - 4;
            rec_hdr[0..4].copy_from_slice(&buf[page_offs..page_offs + 4]);
        } else {
            let page_offs = offs % XLOG_BLCKSZ;
            // we're continuing a record, possibly from previous page.
            let pageleft = XLOG_BLCKSZ - page_offs;

            // read the rest of the record, or as much as fits on this page.
            let n = min(contlen, pageleft);
            let mut hdr_len: usize = 0;
            if *rec_offs < XLOG_SIZE_OF_XLOG_RECORD {
                // copy header
                hdr_len = min(XLOG_SIZE_OF_XLOG_RECORD - *rec_offs, n);
                rec_hdr[*rec_offs..*rec_offs + hdr_len]
                    .copy_from_slice(&buf[page_offs..page_offs + hdr_len]);
            }
            *crc = crc32c_append(*crc, &buf[page_offs + hdr_len..page_offs + n]);
            *rec_offs += n;
            offs += n;
            contlen -= n;

            if contlen == 0 {
                *crc = crc32c_append(*crc, &rec_hdr[0..XLOG_RECORD_CRC_OFFS]);
                offs = (offs + 7) & !7; // pad on 8 bytes boundary */
                let wal_crc = LittleEndian::read_u32(
                    &rec_hdr[XLOG_RECORD_CRC_OFFS..XLOG_RECORD_CRC_OFFS + 4],
                );
                if *crc == wal_crc {
                    last_valid_rec_pos = offs;
                    // Reset rec_offs and crc for start of new record
                    *rec_offs = 0;
                    *crc = 0;
                } else {
                    info!(
                        "CRC mismatch {} vs {} at offset {} lsn {}",
                        *crc, wal_crc, offs, last_valid_rec_pos
                    );
                    break;
                }
            }
        }
    }
    last_valid_rec_pos as u32
}

///
/// Scan a directory that contains PostgreSQL WAL files, for the end of WAL.
///
pub fn find_end_of_wal(
    data_dir: &Path,
    wal_seg_size: usize,
    precise: bool,
) -> (XLogRecPtr, TimeLineID) {
    let mut high_segno: XLogSegNo = 0;
    let mut high_tli: TimeLineID = 0;
    let mut high_ispartial = false;

    for entry in fs::read_dir(data_dir).unwrap().flatten() {
        let ispartial: bool;
        let entry_name = entry.file_name();
        let fname = entry_name.to_str().unwrap();
        /*
         * Check if the filename looks like an xlog file, or a .partial file.
         */
        if IsXLogFileName(fname) {
            ispartial = false;
        } else if IsPartialXLogFileName(fname) {
            ispartial = true;
        } else {
            continue;
        }
        let (segno, tli) = XLogFromFileName(fname, wal_seg_size);
        if !ispartial && entry.metadata().unwrap().len() != wal_seg_size as u64 {
            continue;
        }
        if segno > high_segno
            || (segno == high_segno && tli > high_tli)
            || (segno == high_segno && tli == high_tli && high_ispartial && !ispartial)
        {
            high_segno = segno;
            high_tli = tli;
            high_ispartial = ispartial;
        }
    }
    if high_segno > 0 {
        let mut high_offs = 0;
        if precise {
            let mut crc: u32 = 0;
            let mut rec_offs: usize = 0;
            let mut rec_hdr = [0u8; XLOG_SIZE_OF_XLOG_RECORD];
            let wal_dir = data_dir.join("pg_wal");

            /*
             * To be able to calculate CRC of records crossing segment boundary,
             * we need to parse previous segments.
             * So first traverse segments in backward direction to locate record start
             * and then traverse forward, accumulating CRC.
             */
            let mut prev_segno = high_segno - 1;
            let mut prev_offs: u32 = 0;
            while prev_segno > 1 {
                // TOFO: first segment constains dummy checkpoint record at the beginning
                prev_offs = find_end_of_wal_segment(
                    data_dir,
                    prev_segno,
                    high_tli,
                    wal_seg_size,
                    false,
                    &mut rec_offs,
                    &mut rec_hdr,
                    &mut crc,
                    false,
                );
                if prev_offs != 0 {
                    break;
                }
                prev_segno -= 1;
            }
            if prev_offs != 0 {
                // found start of WAL record
                let first_segno = prev_segno;
                let first_offs = prev_offs;
                while prev_segno + 1 < high_segno {
                    // now traverse record in forward direction, accumulating CRC
                    prev_segno += 1;
                    prev_offs = find_end_of_wal_segment(
                        data_dir,
                        prev_segno,
                        high_tli,
                        wal_seg_size,
                        false,
                        &mut rec_offs,
                        &mut rec_hdr,
                        &mut crc,
                        true,
                    );
                    if prev_offs == 0 {
                        info!("Segment {} is corrupted", prev_segno,);
                        break;
                    }
                }
                if prev_offs != 0 {
                    high_offs = find_end_of_wal_segment(
                        data_dir,
                        high_segno,
                        high_tli,
                        wal_seg_size,
                        high_ispartial,
                        &mut rec_offs,
                        &mut rec_hdr,
                        &mut crc,
                        true,
                    );
                }
                if high_offs == 0 {
                    // If last segment contais no valid records, then return back
                    info!("Last WAL segment {} contains no valid record, truncate WAL till {} segment",
						  high_segno, first_segno);
                    // Remove last segments containing corrupted WAL record
                    for segno in first_segno + 1..high_segno {
                        let file_name = XLogFileName(high_tli, segno, wal_seg_size);
                        let file_path = wal_dir.join(file_name);
                        if let Err(e) = fs::remove_file(&file_path) {
                            info!("Failed to remove file {:?}: {}", &file_path, e);
                        }
                    }
                    let file_name = XLogFileName(high_tli, high_segno, wal_seg_size);
                    let file_path = if high_ispartial {
                        wal_dir.join(file_name.clone() + ".partial")
                    } else {
                        wal_dir.join(file_name.clone())
                    };
                    if let Err(e) = fs::remove_file(&file_path) {
                        info!("Failed to remove file {:?}: {}", &file_path, e);
                    }
                    high_ispartial = false; // previous segment should not be partial
                    high_segno = first_segno;
                    high_offs = first_offs;
                }
            } else {
                // failed to locate previous segment
                assert!(prev_segno <= 1);
                high_offs = find_end_of_wal_segment(
                    data_dir,
                    high_segno,
                    high_tli,
                    wal_seg_size,
                    high_ispartial,
                    &mut rec_offs,
                    &mut rec_hdr,
                    &mut crc,
                    false,
                );
            }

            // If last segment is not marked as partial, it means that next segment
            // was not written. Let's make this segment partial once again.
            if !high_ispartial {
                let file_name = XLogFileName(high_tli, high_segno, wal_seg_size);
                if let Err(e) = fs::rename(
                    wal_dir.join(file_name.clone()),
                    wal_dir.join(file_name.clone() + ".partial"),
                ) {
                    info!(
                        "Failed to rename {} to {}.partial: {}",
                        &file_name, &file_name, e
                    );
                }
            }
        } else {
            /*
             * Move the starting pointer to the start of the next segment, if the
             * highest one we saw was completed.
             */
            if !high_ispartial {
                high_segno += 1;
            }
        }
        let high_ptr = XLogSegNoOffsetToRecPtr(high_segno, high_offs, wal_seg_size);
        return (high_ptr, high_tli);
    }
    (0, 1) // First timeline is 1
}

pub fn main() {
    let mut data_dir = PathBuf::new();
    data_dir.push(".");
    let wal_seg_size = 16 * 1024 * 1024;
    let (wal_end, tli) = find_end_of_wal(&data_dir, wal_seg_size, true);
    println!(
        "wal_end={:>08X}{:>08X}, tli={}",
        (wal_end >> 32) as u32,
        wal_end as u32,
        tli
    );
}

impl XLogRecord {
    pub fn from_bytes(buf: &mut Bytes) -> XLogRecord {
        XLogRecord {
            xl_tot_len: buf.get_u32_le(),
            xl_xid: buf.get_u32_le(),
            xl_prev: buf.get_u64_le(),
            xl_info: buf.get_u8(),
            xl_rmid: buf.get_u8(),
            xl_crc: {
                buf.advance(2);
                buf.get_u32_le()
            },
        }
    }

    pub fn encode(&self) -> Bytes {
        let b: [u8; XLOG_SIZE_OF_XLOG_RECORD];
        b = unsafe { std::mem::transmute::<XLogRecord, [u8; XLOG_SIZE_OF_XLOG_RECORD]>(*self) };
        Bytes::copy_from_slice(&b[..])
    }

    // Is this record an XLOG_SWITCH record? They need some special processing,
    pub fn is_xlog_switch_record(&self) -> bool {
        self.xl_info == pg_constants::XLOG_SWITCH && self.xl_rmid == pg_constants::RM_XLOG_ID
    }
}

impl XLogPageHeaderData {
    pub fn from_bytes<B: Buf>(buf: &mut B) -> XLogPageHeaderData {
        let hdr: XLogPageHeaderData = XLogPageHeaderData {
            xlp_magic: buf.get_u16_le(),
            xlp_info: buf.get_u16_le(),
            xlp_tli: buf.get_u32_le(),
            xlp_pageaddr: buf.get_u64_le(),
            xlp_rem_len: buf.get_u32_le(),
        };
        buf.get_u32_le(); //padding
        hdr
    }
}

impl XLogLongPageHeaderData {
    pub fn from_bytes<B: Buf>(buf: &mut B) -> XLogLongPageHeaderData {
        XLogLongPageHeaderData {
            std: XLogPageHeaderData::from_bytes(buf),
            xlp_sysid: buf.get_u64_le(),
            xlp_seg_size: buf.get_u32_le(),
            xlp_xlog_blcksz: buf.get_u32_le(),
        }
    }

    pub fn encode(&self) -> Bytes {
        let b: [u8; XLOG_SIZE_OF_XLOG_LONG_PHD];
        b = unsafe {
            std::mem::transmute::<XLogLongPageHeaderData, [u8; XLOG_SIZE_OF_XLOG_LONG_PHD]>(*self)
        };
        Bytes::copy_from_slice(&b[..])
    }
}

pub const SIZEOF_CHECKPOINT: usize = std::mem::size_of::<CheckPoint>();

impl CheckPoint {
    pub fn encode(&self) -> Bytes {
        let b: [u8; SIZEOF_CHECKPOINT];
        b = unsafe { std::mem::transmute::<CheckPoint, [u8; SIZEOF_CHECKPOINT]>(*self) };
        Bytes::copy_from_slice(&b[..])
    }

    pub fn decode(buf: &[u8]) -> Result<CheckPoint, anyhow::Error> {
        let mut b = [0u8; SIZEOF_CHECKPOINT];
        b.copy_from_slice(&buf[0..SIZEOF_CHECKPOINT]);
        let checkpoint: CheckPoint;
        checkpoint = unsafe { std::mem::transmute::<[u8; SIZEOF_CHECKPOINT], CheckPoint>(b) };
        Ok(checkpoint)
    }

    // Update next XID based on provided new_xid and stored epoch.
    // Next XID should be greater than new_xid.
    // Also take in account 32-bit wrap-around.
    pub fn update_next_xid(&mut self, xid: u32) {
        let xid = xid.wrapping_add(XID_CHECKPOINT_INTERVAL - 1) & !(XID_CHECKPOINT_INTERVAL - 1);
        let full_xid = self.nextXid.value;
        let new_xid = std::cmp::max(xid + 1, pg_constants::FIRST_NORMAL_TRANSACTION_ID);
        let old_xid = full_xid as u32;
        if new_xid.wrapping_sub(old_xid) as i32 > 0 {
            let mut epoch = full_xid >> 32;
            if new_xid < old_xid {
                // wrap-around
                epoch += 1;
            }
            self.nextXid = FullTransactionId {
                value: (epoch << 32) | new_xid as u64,
            };
        }
    }
}

//
// Generate new WAL segment with single XLOG_CHECKPOINT_SHUTDOWN record.
// We need this segment to start compute node.
// In order to minimize changes in Postgres core, we prefer to
// provide WAL segment from which is can extract checkpoint record in standard way,
// rather then implement some alternative mechanism.
//
pub fn generate_wal_segment(pg_control: &ControlFileData) -> Bytes {
    let mut seg_buf = BytesMut::with_capacity(pg_constants::WAL_SEGMENT_SIZE as usize);

    let hdr = XLogLongPageHeaderData {
        std: {
            XLogPageHeaderData {
                xlp_magic: XLOG_PAGE_MAGIC as u16,
                xlp_info: pg_constants::XLP_LONG_HEADER,
                xlp_tli: 1, // FIXME: always use Postgres timeline 1
                xlp_pageaddr: pg_control.checkPoint - XLOG_SIZE_OF_XLOG_LONG_PHD as u64,
                xlp_rem_len: 0,
            }
        },
        xlp_sysid: pg_control.system_identifier,
        xlp_seg_size: pg_constants::WAL_SEGMENT_SIZE as u32,
        xlp_xlog_blcksz: XLOG_BLCKSZ as u32,
    };

    let hdr_bytes = hdr.encode();
    seg_buf.extend_from_slice(&hdr_bytes);

    let rec_hdr = XLogRecord {
        xl_tot_len: (XLOG_SIZE_OF_XLOG_RECORD
            + SIZE_OF_XLOG_RECORD_DATA_HEADER_SHORT
            + SIZEOF_CHECKPOINT) as u32,
        xl_xid: 0, //0 is for InvalidTransactionId
        xl_prev: 0,
        xl_info: pg_constants::XLOG_CHECKPOINT_SHUTDOWN,
        xl_rmid: pg_constants::RM_XLOG_ID,
        xl_crc: 0,
    };

    let mut rec_shord_hdr_bytes = BytesMut::new();
    rec_shord_hdr_bytes.put_u8(pg_constants::XLR_BLOCK_ID_DATA_SHORT);
    rec_shord_hdr_bytes.put_u8(SIZEOF_CHECKPOINT as u8);

    let rec_bytes = rec_hdr.encode();
    let checkpoint_bytes = pg_control.checkPointCopy.encode();

    //calculate record checksum
    let mut crc = 0;
    crc = crc32c_append(crc, &rec_shord_hdr_bytes[..]);
    crc = crc32c_append(crc, &checkpoint_bytes[..]);
    crc = crc32c_append(crc, &rec_bytes[0..XLOG_RECORD_CRC_OFFS]);

    seg_buf.extend_from_slice(&rec_bytes[0..XLOG_RECORD_CRC_OFFS]);
    seg_buf.put_u32_le(crc);
    seg_buf.extend_from_slice(&rec_shord_hdr_bytes);
    seg_buf.extend_from_slice(&checkpoint_bytes);

    //zero out the rest of the file
    seg_buf.resize(pg_constants::WAL_SEGMENT_SIZE, 0);
    seg_buf.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use std::{env, process::Command, str::FromStr};
    use zenith_utils::lsn::Lsn;

    // Run find_end_of_wal against file in test_wal dir
    // Ensure that it finds last record correctly
    #[test]
    pub fn test_find_end_of_wal() {
        // 1. Run initdb to generate some WAL
        let top_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let data_dir = top_path.join("test_output/test_find_end_of_wal");
        let initdb_path = top_path.join("tmp_install/bin/initdb");
        let lib_path = top_path.join("tmp_install/lib");
        if data_dir.exists() {
            fs::remove_dir_all(&data_dir).unwrap();
        }
        println!("Using initdb from '{}'", initdb_path.display());
        println!("Data directory '{}'", data_dir.display());
        let initdb_output = Command::new(initdb_path)
            .args(&["-D", data_dir.to_str().unwrap()])
            .arg("--no-instructions")
            .arg("--no-sync")
            .env_clear()
            .env("LD_LIBRARY_PATH", &lib_path)
            .env("DYLD_LIBRARY_PATH", &lib_path)
            .output()
            .unwrap();
        assert!(initdb_output.status.success());

        // 2. Pick WAL generated by initdb
        let wal_dir = data_dir.join("pg_wal");
        let wal_seg_size = 16 * 1024 * 1024;

        // 3. Check end_of_wal on non-partial WAL segment (we treat it as fully populated)
        let (wal_end, tli) = find_end_of_wal(&wal_dir, wal_seg_size, true);
        let wal_end = Lsn(wal_end);
        println!("wal_end={}, tli={}", wal_end, tli);
        assert_eq!(wal_end, "0/1699D10".parse::<Lsn>().unwrap());

        // 4. Get the actual end of WAL by pg_waldump
        let waldump_path = top_path.join("tmp_install/bin/pg_waldump");
        let waldump_output = Command::new(waldump_path)
            .arg(wal_dir.join("000000010000000000000001"))
            .env_clear()
            .env("LD_LIBRARY_PATH", &lib_path)
            .env("DYLD_LIBRARY_PATH", &lib_path)
            .output()
            .unwrap();
        let waldump_output = std::str::from_utf8(&waldump_output.stderr).unwrap();
        println!("waldump_output = '{}'", &waldump_output);
        let re = Regex::new(r"invalid record length at (.+):").unwrap();
        let caps = re.captures(&waldump_output).unwrap();
        let waldump_wal_end = Lsn::from_str(caps.get(1).unwrap().as_str()).unwrap();

        // 5. Rename file to partial to actually find last valid lsn
        fs::rename(
            wal_dir.join("000000010000000000000001"),
            wal_dir.join("000000010000000000000001.partial"),
        )
        .unwrap();
        let (wal_end, tli) = find_end_of_wal(&wal_dir, wal_seg_size, true);
        let wal_end = Lsn(wal_end);
        println!("wal_end={}, tli={}", wal_end, tli);
        assert_eq!(wal_end, waldump_wal_end);
    }
}
