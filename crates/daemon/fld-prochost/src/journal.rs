//! Synchronous, single-owner journal. Callers must run filesystem work off runtime workers.
//!
//! Version 1 uses SHA-256 protected segment/watermark headers and bounded postcard
//! records, with an inverted length and a commit trailer. A durable tail manifest
//! distinguishes an interrupted final append from truncation of committed data.
//! Complete invalid frames are corruption. Checkpoint publication (file sync,
//! rename, directory sync) always precedes reclamation.
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fl_model::{LogCursor, TerminalRecord, TerminalRecordKind};
use fl_prochost_proto::{MAX_OUTPUT_BYTES, MAX_READ_BYTES, MAX_READ_RECORDS, MIN_QUOTA_BYTES};
use sha2::{Digest, Sha256};

const SEGMENT_MAGIC: &[u8; 8] = b"FLJSEG01";
const ACK_MAGIC: &[u8; 8] = b"FLJACK01";
const TAIL_MAGIC: &[u8; 8] = b"FLJEND01";
const COMMIT: &[u8; 8] = b"FLJCMT01";
const HEADER_BYTES: u64 = 56;
const FRAME_HEADER: usize = 40;
const FRAME_OVERHEAD: usize = FRAME_HEADER + COMMIT.len();
const MAX_PAYLOAD: usize = MAX_OUTPUT_BYTES + 64;
const HOT_BYTES: usize = 2 * 1024 * 1024;
const HOT_RECORDS: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal I/O: {0}")]
    Io(#[from] io::Error),
    #[error("journal codec: {0}")]
    Codec(#[from] postcard::Error),
    #[error("corrupt journal: {0}")]
    Corrupt(String),
    #[error("journal quota exhausted ({retained_bytes} retained, {quota_bytes} quota)")]
    Backpressured {
        retained_bytes: u64,
        quota_bytes: u64,
    },
    #[error("journal cursor expired; earliest is {earliest:?}")]
    CursorExpired { earliest: LogCursor },
    #[error("journal cursor record and byte positions do not match")]
    CursorMismatch,
    #[error("journal quota must be at least 128 KiB")]
    InvalidQuota,
}

type Result<T, E = JournalError> = std::result::Result<T, E>;

#[derive(Debug)]
struct Segment {
    path: PathBuf,
    start: LogCursor,
    end: LogCursor,
    len: u64,
}

#[derive(Debug)]
struct DirtyAppend {
    path: PathBuf,
    // None means a new, not-yet-published segment.
    original_len: Option<u64>,
    extra: u64,
}

#[derive(Debug)]
pub struct Journal {
    dir: PathBuf,
    quota: u64,
    ack: LogCursor,
    tail: LogCursor,
    segments: VecDeque<Segment>,
    hot: VecDeque<TerminalRecord>,
    hot_bytes: usize,
    dirty: Option<DirtyAppend>,
    scratch_bytes: u64,
}

impl Journal {
    pub fn open(dir: &Path, quota_bytes: u64) -> Result<Self> {
        if quota_bytes < MIN_QUOTA_BYTES {
            return Err(JournalError::InvalidQuota);
        }
        private_dir(dir)?;
        let mut paths = Vec::new();
        let mut temporary = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| corrupt("non-UTF8 journal filename"))?;
            if !entry.file_type()?.is_file() {
                return Err(corrupt("journal entry is not a regular file"));
            }
            if name == "ack" || name == "tail" {
                continue;
            }
            if name == "ack.tmp" || name == "tail.tmp" || name == "compact.tmp" {
                temporary.push(entry.path());
            } else if let Some(id) = segment_id(name) {
                paths.push((id, entry.path()));
            } else {
                return Err(corrupt(format!("unrecognized journal file {name}")));
            }
        }
        paths.sort_unstable_by_key(|(id, _)| *id);
        for path in temporary {
            fs::remove_file(path)?;
        }
        sync_dir(dir)?;
        let ack_path = dir.join("ack");
        let ack = if ack_path.try_exists()? {
            read_header(&mut open_private(&ack_path, false)?, ACK_MAGIC, true)?
        } else {
            if !paths.is_empty() {
                return Err(corrupt("segments exist without a durable checkpoint"));
            }
            let ack = LogCursor::default();
            publish_ack(dir, ack)?;
            ack
        };
        let tail_path = dir.join("tail");
        let committed_tail = if tail_path.try_exists()? {
            read_header(&mut open_private(&tail_path, false)?, TAIL_MAGIC, true)?
        } else {
            if !paths.is_empty() || ack != LogCursor::default() {
                return Err(corrupt("journal is missing its committed tail manifest"));
            }
            publish_cursor(dir, "tail", TAIL_MAGIC, ack)?;
            ack
        };
        if ack.next_record > committed_tail.next_record
            || (ack.next_record == committed_tail.next_record && ack != committed_tail)
        {
            return Err(corrupt("checkpoint exceeds committed tail"));
        }
        let mut journal = Self {
            dir: dir.to_owned(),
            quota: quota_bytes,
            ack,
            tail: ack,
            segments: VecDeque::new(),
            hot: VecDeque::new(),
            hot_bytes: 0,
            dirty: None,
            scratch_bytes: 0,
        };
        let mut previous = None;
        let mut saw_ack = false;
        let mut saw_tail = false;
        let count = paths.len();
        for (index, (id, path)) in paths.into_iter().enumerate() {
            let final_segment = index + 1 == count;
            let mut file = open_private(&path, true)?;
            let file_len = file.metadata()?.len();
            if file_len < HEADER_BYTES {
                if !final_segment
                    || previous.unwrap_or(ack).next_record < committed_tail.next_record
                {
                    return Err(corrupt("incomplete committed segment header"));
                }
                fs::remove_file(&path)?;
                sync_dir(dir)?;
                continue;
            }
            let start = read_header(&mut file, SEGMENT_MAGIC, false)?;
            if start.next_record < id {
                return Err(corrupt("segment identity exceeds its starting cursor"));
            }
            if let Some(end) = previous {
                if start != end {
                    return Err(corrupt("gap or overlap between journal segments"));
                }
            } else if start.next_record > ack.next_record {
                return Err(corrupt("required journal prefix is missing"));
            }
            let mut end = start;
            saw_ack |= check_ack_boundary(ack, end)?;
            saw_tail |= check_ack_boundary(committed_tail, end)?;
            let mut valid_len = HEADER_BYTES;
            loop {
                let allow_incomplete =
                    final_segment && end.next_record >= committed_tail.next_record;
                match read_frame(&mut file, file_len, allow_incomplete)? {
                    Frame::Record(record) => {
                        if record.at != end {
                            return Err(corrupt("record sequence or byte cursor is discontinuous"));
                        }
                        end = record_end(&record)?;
                        saw_ack |= check_ack_boundary(ack, end)?;
                        saw_tail |= check_ack_boundary(committed_tail, end)?;
                        valid_len = file.stream_position()?;
                        if record.at.next_record >= ack.next_record {
                            journal.cache(record);
                        }
                    }
                    Frame::End => break,
                    Frame::Incomplete => {
                        file.set_len(valid_len)?;
                        file.sync_all()?;
                        break;
                    }
                }
            }
            journal.segments.push_back(Segment {
                path,
                start,
                end,
                len: valid_len,
            });
            previous = Some(end);
        }
        if let Some(end) = previous {
            if !saw_ack || !saw_tail || end.next_record < committed_tail.next_record {
                return Err(corrupt(
                    "durable checkpoint or committed tail is not a journal boundary",
                ));
            }
            journal.tail = end;
        }
        if journal.tail.next_record < committed_tail.next_record {
            return Err(corrupt("committed journal suffix is missing"));
        }
        if journal.tail != committed_tail {
            publish_cursor(dir, "tail", TAIL_MAGIC, journal.tail)?;
        }
        journal.reclaim()?;
        Ok(journal)
    }

    pub fn append(&mut self, kind: &TerminalRecordKind) -> Result<TerminalRecord> {
        self.prepare_write()?;
        validate_kind(kind)?;
        let record = TerminalRecord {
            at: self.tail,
            kind: kind.clone(),
        };
        let end = record_end(&record)?;
        let payload = postcard::to_allocvec(&record)?;
        if payload.len() > MAX_PAYLOAD {
            return Err(corrupt("encoded record exceeds framing limit"));
        }
        let frame_len = (payload.len() + FRAME_OVERHEAD) as u64;
        let target = (self.quota / 4).min(1024 * 1024);
        let rotate = self
            .segments
            .back()
            .is_none_or(|segment| segment.len > HEADER_BYTES && segment.len + frame_len > target);
        let extra = frame_len + if rotate { HEADER_BYTES } else { 0 };
        if self.retained_bytes().saturating_add(extra) > self.quota {
            return Err(JournalError::Backpressured {
                retained_bytes: self.retained_bytes(),
                quota_bytes: self.quota,
            });
        }
        let (path, original_len) = if rotate {
            (
                self.dir
                    .join(format!("segment-{:020}.log", self.tail.next_record)),
                None,
            )
        } else {
            let segment = self
                .segments
                .back()
                .expect("nonrotating append has a segment");
            (segment.path.clone(), Some(segment.len))
        };
        // Remember the rollback position before touching disk. A failed append is
        // never exposed and must be removed before any retry can assign its cursor.
        self.dirty = Some(DirtyAppend {
            path: path.clone(),
            original_len,
            extra: 0,
        });
        let written = (|| -> Result<()> {
            let mut file = if let Some(len) = original_len {
                let mut file = open_private(&path, true)?;
                if file.metadata()?.len() != len {
                    return Err(corrupt("active segment changed outside its owner"));
                }
                file.seek(SeekFrom::Start(len))?;
                file
            } else {
                let mut file = create_private(&path, true)?;
                write_header(&mut file, SEGMENT_MAGIC, self.tail)?;
                file
            };
            write_frame(&mut file, &payload)?;
            file.sync_all()?;
            if rotate {
                sync_dir(&self.dir)?;
            }
            publish_cursor(&self.dir, "tail", TAIL_MAGIC, end)?;
            Ok(())
        })();
        if let Err(error) = written {
            if let Some(dirty) = self.dirty.as_mut() {
                dirty.extra = fs::symlink_metadata(&path)
                    .map(|meta| meta.len().saturating_sub(original_len.unwrap_or(0)))
                    .unwrap_or(extra);
            }
            // Best effort here, mandatory on the next append. Preserve the real
            // write/fsync error rather than disguising an ENOSPC/EFBIG as quota.
            let _ = self.clean_dirty();
            self.account_scratch();
            return Err(error);
        }
        self.dirty = None;
        if rotate {
            self.segments.push_back(Segment {
                path,
                start: self.tail,
                end,
                len: HEADER_BYTES + frame_len,
            });
        } else {
            let segment = self.segments.back_mut().expect("active segment exists");
            segment.end = end;
            segment.len += frame_len;
        }
        self.tail = end;
        self.cache(record.clone());
        Ok(record)
    }

    pub fn read(&mut self, from: LogCursor, max_bytes: usize) -> Result<Vec<TerminalRecord>> {
        self.validate_cursor(from)?;
        let budget = max_bytes.min(MAX_READ_BYTES);
        if budget == 0 || from == self.tail {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        let mut bytes = 0;
        if let Some(index) = self.hot.iter().position(|record| record.at == from) {
            for record in self.hot.iter().skip(index) {
                if !fits_batch(record, budget, &mut bytes, records.len()) {
                    break;
                }
                records.push(record.clone());
            }
            return Ok(records);
        }
        'segments: for segment in &self.segments {
            if segment.end.next_record <= from.next_record {
                continue;
            }
            let mut file = open_private(&segment.path, false)?;
            file.seek(SeekFrom::Start(HEADER_BYTES))?;
            let mut expected = segment.start;
            while let Frame::Record(record) = read_frame(&mut file, segment.len, false)? {
                if record.at != expected {
                    return Err(corrupt("record sequence changed during read"));
                }
                expected = record_end(&record)?;
                if record.at.next_record < from.next_record {
                    continue;
                }
                if !fits_batch(&record, budget, &mut bytes, records.len()) {
                    break 'segments;
                }
                records.push(record);
            }
        }
        Ok(records)
    }

    pub fn acknowledge(&mut self, through: LogCursor) -> Result<()> {
        self.validate_cursor(through)?;
        self.prepare_write()?;
        if through != self.ack {
            if let Err(error) = publish_ack(&self.dir, through) {
                self.account_scratch();
                return Err(error);
            }
            self.ack = through;
            while self
                .hot
                .front()
                .is_some_and(|record| record.at.next_record < through.next_record)
            {
                let record = self.hot.pop_front().expect("front exists");
                self.hot_bytes -= hot_cost(&record);
            }
        }
        // Repeated ack retries reclamation after a prior unlink/compaction error.
        let result = self.reclaim();
        if result.is_err() {
            self.account_scratch();
        }
        result
    }

    pub fn set_quota(&mut self, bytes: u64) -> Result<()> {
        if bytes < MIN_QUOTA_BYTES {
            return Err(JournalError::InvalidQuota);
        }
        self.quota = bytes;
        Ok(())
    }

    pub fn tail(&self) -> LogCursor {
        self.tail
    }
    pub fn earliest(&self) -> LogCursor {
        self.ack
    }
    pub fn retained_bytes(&self) -> u64 {
        2 * HEADER_BYTES
            + self.segments.iter().map(|segment| segment.len).sum::<u64>()
            + self.dirty.as_ref().map_or(0, |dirty| dirty.extra)
            + self.scratch_bytes
    }
    pub fn quota_bytes(&self) -> u64 {
        self.quota
    }

    fn account_scratch(&mut self) {
        self.scratch_bytes = ["ack.tmp", "tail.tmp", "compact.tmp"]
            .iter()
            .filter_map(|name| fs::symlink_metadata(self.dir.join(name)).ok())
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len())
            .sum();
    }

    fn prepare_write(&mut self) -> Result<()> {
        let result = (|| -> Result<()> {
            self.clean_dirty()?;
            if self.scratch_bytes != 0 {
                for name in ["ack.tmp", "tail.tmp", "compact.tmp"] {
                    match fs::remove_file(self.dir.join(name)) {
                        Ok(()) => (),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                        Err(error) => return Err(error.into()),
                    }
                }
                sync_dir(&self.dir)?;
                self.scratch_bytes = 0;
            }
            Ok(())
        })();
        if result.is_err() {
            self.account_scratch();
        }
        result
    }

    fn validate_cursor(&self, cursor: LogCursor) -> Result<()> {
        if cursor.next_record < self.ack.next_record {
            return Err(JournalError::CursorExpired { earliest: self.ack });
        }
        if cursor.next_record > self.tail.next_record {
            return Err(JournalError::CursorMismatch);
        }
        if cursor.next_record == self.tail.next_record {
            return if cursor == self.tail {
                Ok(())
            } else {
                Err(JournalError::CursorMismatch)
            };
        }
        if cursor.next_record == self.ack.next_record {
            return if cursor == self.ack {
                Ok(())
            } else {
                Err(JournalError::CursorMismatch)
            };
        }
        if let Some(record) = self
            .hot
            .iter()
            .find(|record| record.at.next_record == cursor.next_record)
        {
            return if record.at == cursor {
                Ok(())
            } else {
                Err(JournalError::CursorMismatch)
            };
        }
        for segment in &self.segments {
            if cursor.next_record < segment.start.next_record
                || cursor.next_record >= segment.end.next_record
            {
                continue;
            }
            let mut file = open_private(&segment.path, false)?;
            file.seek(SeekFrom::Start(HEADER_BYTES))?;
            let mut expected = segment.start;
            while let Frame::Record(record) = read_frame(&mut file, segment.len, false)? {
                if record.at != expected {
                    return Err(corrupt("record sequence changed during cursor lookup"));
                }
                expected = record_end(&record)?;
                if record.at.next_record == cursor.next_record {
                    return if record.at == cursor {
                        Ok(())
                    } else {
                        Err(JournalError::CursorMismatch)
                    };
                }
            }
        }
        Err(corrupt("required cursor boundary is missing"))
    }

    fn cache(&mut self, record: TerminalRecord) {
        self.hot_bytes += hot_cost(&record);
        self.hot.push_back(record);
        while self.hot_bytes > HOT_BYTES || self.hot.len() > HOT_RECORDS {
            let record = self.hot.pop_front().expect("overfull cache is nonempty");
            self.hot_bytes -= hot_cost(&record);
        }
    }

    fn clean_dirty(&mut self) -> Result<()> {
        let Some(dirty) = &self.dirty else {
            return Ok(());
        };
        match fs::remove_file(self.dir.join("tail.tmp")) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let published = read_header(
            &mut open_private(&self.dir.join("tail"), false)?,
            TAIL_MAGIC,
            true,
        )?;
        if published != self.tail {
            // A rename may have succeeded before directory fsync failed. Restore
            // the old durable manifest before removing its unexposed record.
            publish_cursor(&self.dir, "tail", TAIL_MAGIC, self.tail)?;
        }
        if let Some(len) = dirty.original_len {
            let file = open_private(&dirty.path, true)?;
            if file.metadata()?.len() < len {
                return Err(corrupt("committed active segment was truncated"));
            }
            file.set_len(len)?;
            file.sync_all()?;
        } else {
            match fs::remove_file(&dirty.path) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
            sync_dir(&self.dir)?;
        }
        self.dirty = None;
        Ok(())
    }

    fn reclaim(&mut self) -> Result<()> {
        // Also retry a prior directory-sync failure after unlink or replacement,
        // including when that operation already updated our in-memory segments.
        sync_dir(&self.dir)?;
        while self
            .segments
            .front()
            .is_some_and(|segment| segment.end.next_record <= self.ack.next_record)
        {
            let segment = self.segments.front().expect("front exists");
            fs::remove_file(&segment.path)?;
            self.segments.pop_front();
            sync_dir(&self.dir)?;
        }
        let Some(segment) = self.segments.front() else {
            return Ok(());
        };
        if segment.start == self.ack {
            return Ok(());
        }
        // Compact even the active segment: otherwise a partially acknowledged
        // small segment can permanently block a 64-KiB append at minimum quota.
        // The old suffix remains authoritative until the replacement is synced.
        let mut source = open_private(&segment.path, false)?;
        source.seek(SeekFrom::Start(HEADER_BYTES))?;
        let offset = loop {
            let before = source.stream_position()?;
            match read_frame(&mut source, segment.len, false)? {
                Frame::Record(record) if record.at == self.ack => break before,
                Frame::Record(_) => (),
                Frame::End | Frame::Incomplete => {
                    return Err(corrupt("checkpoint missing from compacted segment"));
                }
            }
        };
        let new_len = HEADER_BYTES + segment.len - offset;
        let replacement = self.dir.join("compact.tmp");
        let compacted = (|| -> Result<()> {
            let mut target = create_private(&replacement, false)?;
            write_header(&mut target, SEGMENT_MAGIC, self.ack)?;
            source.seek(SeekFrom::Start(offset))?;
            let suffix_len = segment.len - offset;
            let copied = io::copy(&mut source.take(suffix_len), &mut target)?;
            if copied != suffix_len {
                return Err(corrupt("segment shortened during compaction"));
            }
            target.sync_all()?;
            fs::rename(&replacement, &segment.path)?;
            Ok(())
        })();
        if let Err(error) = compacted {
            let _ = fs::remove_file(&replacement);
            return Err(error);
        }
        // Rename has happened even if the subsequent directory fsync fails.
        let segment = self.segments.front_mut().expect("compacted segment exists");
        segment.start = self.ack;
        segment.len = new_len;
        sync_dir(&self.dir)?;
        Ok(())
    }
}

fn corrupt(message: impl Into<String>) -> JournalError {
    JournalError::Corrupt(message.into())
}

fn validate_kind(kind: &TerminalRecordKind) -> Result<()> {
    match kind {
        TerminalRecordKind::Output { bytes, .. } if bytes.len() > MAX_OUTPUT_BYTES => {
            Err(corrupt("output record exceeds 64 KiB"))
        }
        TerminalRecordKind::Resize(size) => size
            .validate()
            .map(|_| ())
            .map_err(|_| corrupt("invalid resize record")),
        _ => Ok(()),
    }
}

fn record_end(record: &TerminalRecord) -> Result<LogCursor> {
    record
        .end()
        .ok_or_else(|| corrupt("journal cursor overflow"))
}

fn output_len(record: &TerminalRecord) -> usize {
    match &record.kind {
        TerminalRecordKind::Output { bytes, .. } => bytes.len(),
        _ => 0,
    }
}

fn hot_cost(record: &TerminalRecord) -> usize {
    std::mem::size_of::<TerminalRecord>() + output_len(record)
}

fn fits_batch(record: &TerminalRecord, budget: usize, used: &mut usize, count: usize) -> bool {
    let len = output_len(record);
    if count >= MAX_READ_RECORDS || len > budget - *used {
        return false;
    }
    *used += len;
    true
}

fn check_ack_boundary(ack: LogCursor, cursor: LogCursor) -> Result<bool> {
    if cursor.next_record == ack.next_record {
        if cursor != ack {
            return Err(corrupt(
                "checkpoint byte cursor does not match record boundary",
            ));
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

fn segment_id(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("segment-")?.strip_suffix(".log")?;
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn private_dir(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && (!meta.is_dir() || meta.file_type().is_symlink())
    {
        return Err(corrupt("journal directory is not a real directory"));
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    if let Some(parent) = path.parent() {
        sync_dir(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })?;
    }
    Ok(())
}

fn file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
}

fn open_private(path: &Path, write: bool) -> Result<File> {
    let file = file_options().read(true).write(write).open(path)?;
    if !file.metadata()?.is_file() {
        return Err(corrupt("journal file is not regular"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn create_private(path: &Path, exclusive: bool) -> Result<File> {
    let mut options = file_options();
    options.read(true).write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all().map_err(Into::into)
}

fn write_header(file: &mut File, magic: &[u8; 8], cursor: LogCursor) -> Result<()> {
    let mut header = [0_u8; HEADER_BYTES as usize];
    header[..8].copy_from_slice(magic);
    header[8..16].copy_from_slice(&cursor.next_record.to_le_bytes());
    header[16..24].copy_from_slice(&cursor.next_byte.to_le_bytes());
    let hash = Sha256::digest(&header[..24]);
    header[24..].copy_from_slice(&hash);
    file.write_all(&header)?;
    Ok(())
}

fn read_header(file: &mut File, magic: &[u8; 8], exact: bool) -> Result<LogCursor> {
    if file.metadata()?.len() < HEADER_BYTES || (exact && file.metadata()?.len() != HEADER_BYTES) {
        return Err(corrupt("invalid journal header length"));
    }
    let mut header = [0_u8; HEADER_BYTES as usize];
    file.read_exact(&mut header)?;
    if &header[..8] != magic || Sha256::digest(&header[..24])[..] != header[24..] {
        return Err(corrupt("journal header checksum or version mismatch"));
    }
    Ok(LogCursor {
        next_record: u64::from_le_bytes(header[8..16].try_into().expect("fixed header field")),
        next_byte: u64::from_le_bytes(header[16..24].try_into().expect("fixed header field")),
    })
}

fn publish_ack(dir: &Path, cursor: LogCursor) -> Result<()> {
    publish_cursor(dir, "ack", ACK_MAGIC, cursor)
}

fn publish_cursor(dir: &Path, name: &str, magic: &[u8; 8], cursor: LogCursor) -> Result<()> {
    let temporary = dir.join(format!("{name}.tmp"));
    let result = (|| -> Result<()> {
        let mut file = create_private(&temporary, false)?;
        write_header(&mut file, magic, cursor)?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join(name))?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn write_frame(file: &mut File, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| corrupt("frame length overflow"))?;
    file.write_all(&len.to_le_bytes())?;
    file.write_all(&(!len).to_le_bytes())?;
    file.write_all(&Sha256::digest(payload))?;
    file.write_all(payload)?;
    file.write_all(COMMIT)?;
    Ok(())
}

enum Frame {
    Record(TerminalRecord),
    End,
    Incomplete,
}

fn read_frame(file: &mut File, file_len: u64, allow_incomplete: bool) -> Result<Frame> {
    let position = file.stream_position()?;
    if position == file_len {
        return Ok(Frame::End);
    }
    if position > file_len {
        return Err(corrupt("frame extends beyond segment"));
    }
    let remaining = file_len - position;
    if remaining < FRAME_HEADER as u64 {
        return if allow_incomplete {
            Ok(Frame::Incomplete)
        } else {
            Err(corrupt("short frame header"))
        };
    }
    let mut header = [0_u8; FRAME_HEADER];
    file.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header[..4].try_into().expect("fixed frame field"));
    let inverse = u32::from_le_bytes(header[4..8].try_into().expect("fixed frame field"));
    if inverse != !len || len == 0 || len as usize > MAX_PAYLOAD {
        return Err(corrupt("invalid frame length"));
    }
    if remaining < len as u64 + FRAME_OVERHEAD as u64 {
        return if allow_incomplete {
            Ok(Frame::Incomplete)
        } else {
            Err(corrupt("short committed interior frame"))
        };
    }
    let mut payload = vec![0_u8; len as usize];
    file.read_exact(&mut payload)?;
    let mut trailer = [0_u8; 8];
    file.read_exact(&mut trailer)?;
    if &trailer != COMMIT || Sha256::digest(&payload)[..] != header[8..] {
        return Err(corrupt("record checksum or commit trailer mismatch"));
    }
    // The payload bound is checked before allocation. Postcard bounds its byte
    // slice before Bytes' deserializer allocates; trailing/schema data is rejected.
    let (record, rest): (TerminalRecord, _) = postcard::take_from_bytes(&payload)
        .map_err(|error| corrupt(format!("committed record schema: {error}")))?;
    if !rest.is_empty() {
        return Err(corrupt("trailing bytes in committed record"));
    }
    validate_kind(&record.kind)?;
    record_end(&record)?;
    Ok(Frame::Record(record))
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
