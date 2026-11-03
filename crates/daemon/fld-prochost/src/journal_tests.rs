use super::*;
use bytes::Bytes;
use fl_model::{ExitStatus, Size};
use tempfile::TempDir;

fn output(len: usize, byte: u8) -> TerminalRecordKind {
    TerminalRecordKind::Output {
        bytes: Bytes::from(vec![byte; len]),
        echo: false,
    }
}

fn journal(quota: u64) -> (TempDir, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), quota).unwrap();
    (dir, journal)
}

fn disk_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum()
}

fn final_path(journal: &Journal) -> PathBuf {
    journal.segments.back().unwrap().path.clone()
}

fn corrupt_byte(path: &Path, offset: u64) {
    let mut file = open_private(path, true).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    byte[0] ^= 0x80;
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn ordered_controls_share_byte_offsets_but_not_record_boundaries() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    let kinds = [
        output(3, b'a'),
        TerminalRecordKind::Resize(Size::new(100, 40)),
        TerminalRecordKind::Echo(true),
        TerminalRecordKind::SyncFlush,
        TerminalRecordKind::Exit(ExitStatus {
            code: Some(0),
            signal: None,
        }),
        output(2, b'b'),
    ];
    let records: Vec<_> = kinds.iter().map(|kind| log.append(kind).unwrap()).collect();
    assert_eq!(
        log.tail(),
        LogCursor {
            next_record: 6,
            next_byte: 5
        }
    );
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.at.next_record, index as u64);
        assert_eq!(
            log.read(record.at, MAX_READ_BYTES).unwrap(),
            records[index..]
        );
    }
    assert_eq!(records[1].at.next_byte, 3);
    assert_eq!(records[4].at.next_byte, 3);
    assert!(log.read(log.tail(), MAX_READ_BYTES).unwrap().is_empty());
    drop(log);
    let mut log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(
        log.read(LogCursor::default(), MAX_READ_BYTES).unwrap(),
        records
    );
    assert_eq!(log.retained_bytes(), disk_bytes(dir.path()));
}

#[test]
fn cursor_pairs_are_validated_for_reads_and_checkpoints() {
    let (_dir, mut log) = journal(MIN_QUOTA_BYTES);
    let first = log.append(&output(5, b'x')).unwrap();
    let control = log.append(&TerminalRecordKind::SyncFlush).unwrap();
    log.append(&output(4, b'y')).unwrap();
    for cursor in [
        LogCursor {
            next_record: 0,
            next_byte: 1,
        },
        LogCursor {
            next_record: 1,
            next_byte: 4,
        },
        LogCursor {
            next_record: 2,
            next_byte: 9,
        },
        LogCursor {
            next_record: 3,
            next_byte: 8,
        },
        LogCursor {
            next_record: 4,
            next_byte: 9,
        },
    ] {
        assert!(matches!(
            log.read(cursor, MAX_READ_BYTES),
            Err(JournalError::CursorMismatch)
        ));
        assert!(matches!(
            log.acknowledge(cursor),
            Err(JournalError::CursorMismatch)
        ));
    }
    log.acknowledge(control.at).unwrap();
    assert_eq!(log.earliest(), first.end().unwrap());
    assert!(matches!(
        log.read(first.at, MAX_READ_BYTES),
        Err(JournalError::CursorExpired { .. })
    ));
    assert!(matches!(
        log.acknowledge(first.at),
        Err(JournalError::CursorExpired { .. })
    ));
    assert!(matches!(
        log.acknowledge(LogCursor {
            next_record: 1,
            next_byte: 0
        }),
        Err(JournalError::CursorMismatch)
    ));
    log.acknowledge(control.at).unwrap();
}

#[test]
fn minimum_quota_checkpoint_reclaims_active_tail_and_resumes() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    let large = output(MAX_OUTPUT_BYTES, b'x');
    let first = log.append(&large).unwrap();
    assert!(matches!(
        log.append(&large),
        Err(JournalError::Backpressured { .. })
    ));
    assert_eq!(log.tail(), first.end().unwrap());
    let tail = log.tail();
    log.acknowledge(tail).unwrap();
    assert!(log.segments.is_empty());
    assert_eq!(log.retained_bytes(), 2 * HEADER_BYTES);
    assert_eq!(log.retained_bytes(), disk_bytes(dir.path()));
    let second = log.append(&large).unwrap();
    assert_eq!(second.at, tail);
    log.acknowledge(log.tail()).unwrap();
    drop(log);
    let mut log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.earliest(), log.tail());
    assert!(log.segments.is_empty());
    log.append(&large).unwrap();
}

#[test]
fn partially_acked_segment_is_compacted_before_maximum_output() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    let mut checkpoint = LogCursor::default();
    // With an extra 40-KiB suffix, unreclaimed bytes would prevent a maximum
    // append even though the required suffix plus that append fits the quota.
    for index in 0..3 {
        let record = log.append(&output(8 * 1024, index)).unwrap();
        if index == 1 {
            checkpoint = record.end().unwrap();
        }
    }
    log.append(&output(40 * 1024, b'y')).unwrap();
    assert!(matches!(
        log.append(&output(MAX_OUTPUT_BYTES, b'z')),
        Err(JournalError::Backpressured { .. })
    ));
    let before = log.retained_bytes();
    log.acknowledge(checkpoint).unwrap();
    assert!(log.retained_bytes() < before - 16 * 1024);
    assert_eq!(log.segments.front().unwrap().start, checkpoint);
    log.append(&output(MAX_OUTPUT_BYTES, b'z')).unwrap();
    assert_eq!(log.retained_bytes(), disk_bytes(dir.path()));
    let expected = log.read(checkpoint, MAX_READ_BYTES).unwrap();
    drop(log);
    let mut recovered = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(recovered.earliest(), checkpoint);
    assert_eq!(
        recovered.read(checkpoint, MAX_READ_BYTES).unwrap(),
        expected
    );
}

#[test]
fn read_limits_never_split_records_and_control_memory_is_bounded() {
    let (_dir, mut log) = journal(4 * 1024 * 1024);
    log.append(&output(10, b'x')).unwrap();
    assert!(log.read(LogCursor::default(), 9).unwrap().is_empty());
    for _ in 0..HOT_RECORDS + 10 {
        log.append(&TerminalRecordKind::SyncFlush).unwrap();
    }
    assert_eq!(log.hot.len(), HOT_RECORDS);
    assert!(log.hot_bytes <= HOT_BYTES);
    let records = log.read(LogCursor::default(), usize::MAX).unwrap();
    assert_eq!(records.len(), MAX_READ_RECORDS);
    assert_eq!(records[0].at, LogCursor::default());
    assert!(log.read(LogCursor::default(), 0).unwrap().is_empty());
}

#[test]
fn hot_ring_evicts_bytes_but_disk_keeps_contiguous_required_suffix() {
    let (dir, mut log) = journal(5 * 1024 * 1024);
    let mut records = Vec::new();
    for byte in 0..40 {
        records.push(log.append(&output(MAX_OUTPUT_BYTES, byte)).unwrap());
    }
    assert!(log.hot_bytes <= HOT_BYTES);
    assert!(log.hot.front().unwrap().at.next_record > 0);
    let mut cursor = LogCursor::default();
    let mut read = Vec::new();
    while cursor != log.tail() {
        let batch = log.read(cursor, usize::MAX).unwrap();
        assert!(!batch.is_empty());
        assert!(batch.iter().map(output_len).sum::<usize>() <= MAX_READ_BYTES);
        cursor = batch.last().unwrap().end().unwrap();
        read.extend(batch);
    }
    assert_eq!(read, records);
    let checkpoint = records[11].at;
    log.acknowledge(checkpoint).unwrap();
    drop(log);
    let mut log = Journal::open(dir.path(), 5 * 1024 * 1024).unwrap();
    let mut cursor = checkpoint;
    for expected in records[11..].chunks(8) {
        let actual = log.read(cursor, MAX_READ_BYTES).unwrap();
        assert_eq!(actual, expected);
        cursor = actual.last().unwrap().end().unwrap();
    }
    assert_eq!(cursor, log.tail());
}


#[test]
fn interrupted_final_appends_are_removed_without_losing_committed_records() {
    for cut in [
        1,
        7,
        FRAME_HEADER - 1,
        FRAME_HEADER,
        FRAME_HEADER + 2,
        FRAME_HEADER + 12,
    ] {
        let (dir, mut log) = journal(MIN_QUOTA_BYTES);
        let record = log.append(&output(20, b'a')).unwrap();
        let path = final_path(&log);
        let committed_len = log.segments.back().unwrap().len;
        let pending = TerminalRecord {
            at: log.tail(),
            kind: output(20, b'b'),
        };
        let payload = postcard::to_allocvec(&pending).unwrap();
        let mut scratch = tempfile::tempfile().unwrap();
        write_frame(&mut scratch, &payload).unwrap();
        scratch.rewind().unwrap();
        let mut frame = Vec::new();
        scratch.read_to_end(&mut frame).unwrap();
        let mut file = open_private(&path, true).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(&frame[..cut]).unwrap();
        file.sync_all().unwrap();
        drop(log);
        let mut log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
        assert_eq!(log.tail(), record.end().unwrap());
        assert_eq!(fs::metadata(&path).unwrap().len(), committed_len);
        assert_eq!(log.read(record.at, MAX_READ_BYTES).unwrap(), [record]);
        assert_eq!(log.append(&pending.kind).unwrap(), pending);
    }
}

#[test]
fn incomplete_new_segment_header_is_safe_only_beyond_committed_tail() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    let record = log.append(&output(100, b'x')).unwrap();
    let incomplete = dir
        .path()
        .join(format!("segment-{:020}.log", log.tail().next_record));
    let mut file = create_private(&incomplete, true).unwrap();
    file.write_all(&SEGMENT_MAGIC[..4]).unwrap();
    file.sync_all().unwrap();
    drop(log);
    let log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.tail(), record.end().unwrap());
    assert!(!incomplete.exists());
}

#[test]
fn committed_truncation_checksum_length_schema_and_sequence_corruption_are_errors() {
    for corruption in 0..6 {
        let (dir, mut log) = journal(MIN_QUOTA_BYTES);
        let record = log.append(&output(20, b'x')).unwrap();
        let path = final_path(&log);
        let len = fs::metadata(&path).unwrap().len();
        match corruption {
            0 => {
                open_private(&path, true).unwrap().set_len(len - 1).unwrap();
            }
            1 => corrupt_byte(&path, HEADER_BYTES + FRAME_HEADER as u64 + 2),
            2 => corrupt_byte(&path, HEADER_BYTES),
            3 => {
                // Correct checksum/trailer does not make an unknown schema valid.
                let mut file = open_private(&path, true).unwrap();
                file.set_len(HEADER_BYTES).unwrap();
                file.seek(SeekFrom::Start(HEADER_BYTES)).unwrap();
                write_frame(&mut file, &[0xff, 0xff, 0xff, 0xff]).unwrap();
                file.sync_all().unwrap();
            }
            4 => {
                let invalid = TerminalRecord {
                    at: LogCursor {
                        next_record: record.at.next_record + 1,
                        next_byte: 0,
                    },
                    kind: record.kind,
                };
                let mut file = open_private(&path, true).unwrap();
                file.set_len(HEADER_BYTES).unwrap();
                file.seek(SeekFrom::Start(HEADER_BYTES)).unwrap();
                write_frame(&mut file, &postcard::to_allocvec(&invalid).unwrap()).unwrap();
                file.sync_all().unwrap();
            }
            5 => {
                fs::remove_file(&path).unwrap();
            }
            _ => unreachable!(),
        }
        drop(log);
        assert!(
            matches!(
                Journal::open(dir.path(), MIN_QUOTA_BYTES),
                Err(JournalError::Corrupt(_))
            ),
            "case {corruption}"
        );
    }
}

#[test]
fn missing_interior_segment_is_not_silently_skipped() {
    let (dir, mut log) = journal(1024 * 1024);
    for _ in 0..12 {
        log.append(&output(MAX_OUTPUT_BYTES, b'x')).unwrap();
    }
    assert!(log.segments.len() >= 3);
    fs::remove_file(&log.segments[1].path).unwrap();
    drop(log);
    assert!(matches!(
        Journal::open(dir.path(), 1024 * 1024),
        Err(JournalError::Corrupt(_))
    ));
}

#[test]
fn durable_ack_crash_windows_preserve_suffix_and_retry_reclamation() {
    for window in 0..4 {
        let (dir, mut log) = journal(MIN_QUOTA_BYTES);
        let mut records = Vec::new();
        for _ in 0..5 {
            records.push(log.append(&output(16 * 1024, b'x')).unwrap());
        }
        let checkpoint = records[3].at;
        // Exercise: temporary watermark only; durable watermark before any
        // deletion; durable watermark after prefix deletion; incomplete compaction.
        if window == 0 {
            let mut temporary = create_private(&dir.path().join("ack.tmp"), false).unwrap();
            write_header(&mut temporary, ACK_MAGIC, checkpoint).unwrap();
            temporary.sync_all().unwrap();
        } else {
            publish_ack(dir.path(), checkpoint).unwrap();
            if window >= 2 {
                fs::remove_file(&log.segments[0].path).unwrap();
            }
            if window == 3 {
                let mut temporary = create_private(&dir.path().join("compact.tmp"), false).unwrap();
                temporary.write_all(b"interrupted replacement").unwrap();
                temporary.sync_all().unwrap();
            }
        }
        drop(log);
        let mut log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
        let expected = if window == 0 {
            &records[..]
        } else {
            &records[3..]
        };
        assert_eq!(log.read(log.earliest(), MAX_READ_BYTES).unwrap(), expected);
        if window != 0 {
            assert_eq!(log.earliest(), checkpoint);
            assert!(
                log.segments
                    .iter()
                    .all(|segment| segment.start.next_record >= checkpoint.next_record)
            );
            log.acknowledge(checkpoint).unwrap();
        }
        assert!(!dir.path().join("ack.tmp").exists());
        assert!(!dir.path().join("compact.tmp").exists());
        assert_eq!(log.retained_bytes(), disk_bytes(dir.path()));
    }
}

#[test]
fn repeated_durable_ack_retries_deletion_after_io_failure() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    log.append(&output(1024, b'x')).unwrap();
    let tail = log.tail();
    let path = final_path(&log);
    let saved = dir.path().join("saved");
    fs::rename(&path, &saved).unwrap();
    // Removing a directory via remove_file fails on a real filesystem, after
    // checkpoint publication. Restore the segment and retry the same watermark.
    fs::create_dir(&path).unwrap();
    assert!(matches!(log.acknowledge(tail), Err(JournalError::Io(_))));
    assert_eq!(log.earliest(), tail);
    fs::remove_dir(&path).unwrap();
    fs::rename(saved, path).unwrap();
    log.acknowledge(tail).unwrap();
    assert!(log.segments.is_empty());
    drop(log);
    let log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.earliest(), tail);
    assert_eq!(log.tail(), tail);
    assert!(log.segments.is_empty());
}

#[test]
fn repeated_durable_ack_retries_compaction_after_io_failure() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    log.append(&output(1024, b'a')).unwrap();
    let second = log.append(&output(1024, b'b')).unwrap();
    fs::create_dir(dir.path().join("compact.tmp")).unwrap();
    assert!(matches!(
        log.acknowledge(second.at),
        Err(JournalError::Io(_))
    ));
    assert_eq!(log.earliest(), second.at);
    fs::remove_dir(dir.path().join("compact.tmp")).unwrap();
    log.acknowledge(second.at).unwrap();
    assert_eq!(log.segments.front().unwrap().start, second.at);
    assert_eq!(log.read(second.at, MAX_READ_BYTES).unwrap(), [second]);
}

#[test]
fn append_failure_keeps_cursor_and_retry_removes_partial_tail() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    log.append(&output(10, b'a')).unwrap();
    let before = log.tail();
    let len = log.segments.back().unwrap().len;
    // Force tail publication to fail after the record frame was written/synced.
    fs::create_dir(dir.path().join("tail.tmp")).unwrap();
    assert!(matches!(
        log.append(&output(10, b'b')),
        Err(JournalError::Io(_))
    ));
    assert_eq!(log.tail(), before);
    assert!(log.dirty.is_some());
    fs::remove_dir(dir.path().join("tail.tmp")).unwrap();
    let record = log.append(&output(10, b'b')).unwrap();
    assert_eq!(record.at, before);
    assert!(log.segments.back().unwrap().len > len);
    assert_eq!(log.read(before, MAX_READ_BYTES).unwrap(), [record]);
    drop(log);
    let log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.tail().next_record, 2);
}

#[test]
fn quota_changes_do_not_evict_or_hide_required_records() {
    let (dir, mut log) = journal(512 * 1024);
    let first = log.append(&output(MAX_OUTPUT_BYTES, b'a')).unwrap();
    log.append(&output(MAX_OUTPUT_BYTES, b'b')).unwrap();
    log.set_quota(MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.quota_bytes(), MIN_QUOTA_BYTES);
    assert!(matches!(
        log.append(&TerminalRecordKind::SyncFlush),
        Err(JournalError::Backpressured { .. })
    ));
    assert_eq!(log.read(first.at, MAX_READ_BYTES).unwrap().len(), 2);
    drop(log);
    let mut log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.read(first.at, MAX_READ_BYTES).unwrap().len(), 2);
    log.set_quota(512 * 1024).unwrap();
    log.append(&TerminalRecordKind::SyncFlush).unwrap();
    assert!(matches!(
        log.set_quota(MIN_QUOTA_BYTES - 1),
        Err(JournalError::InvalidQuota)
    ));
}

#[cfg(unix)]
#[test]
fn files_are_private_and_symlinks_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    log.append(&output(10, b'x')).unwrap();
    assert_eq!(
        fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for entry in fs::read_dir(dir.path()).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let path = final_path(&log);
    drop(log);
    fs::remove_file(&path).unwrap();
    let target = tempfile::NamedTempFile::new().unwrap();
    symlink(target.path(), &path).unwrap();
    assert!(matches!(
        Journal::open(dir.path(), MIN_QUOTA_BYTES),
        Err(JournalError::Corrupt(_))
    ));
    let outside = tempfile::tempdir().unwrap();
    let link = outside.path().join("linked-journal");
    symlink(dir.path(), &link).unwrap();
    assert!(matches!(
        Journal::open(&link, MIN_QUOTA_BYTES),
        Err(JournalError::Corrupt(_))
    ));
}

#[test]
fn crash_after_compaction_rename_keeps_only_the_contiguous_suffix() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    log.append(&output(1024, b'a')).unwrap();
    let retained = log.append(&output(1024, b'b')).unwrap();
    publish_ack(dir.path(), retained.at).unwrap();
    let path = final_path(&log);
    let mut source = open_private(&path, false).unwrap();
    source.seek(SeekFrom::Start(HEADER_BYTES)).unwrap();
    assert!(matches!(
        read_frame(&mut source, fs::metadata(&path).unwrap().len(), false).unwrap(),
        Frame::Record(_)
    ));
    let offset = source.stream_position().unwrap();
    let replacement = dir.path().join("compact.tmp");
    let mut target = create_private(&replacement, true).unwrap();
    write_header(&mut target, SEGMENT_MAGIC, retained.at).unwrap();
    io::copy(&mut source, &mut target).unwrap();
    target.sync_all().unwrap();
    fs::rename(&replacement, &path).unwrap();
    // Deliberately reopen at the rename/directory-sync window.
    drop(log);
    let mut log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.earliest(), retained.at);
    assert_eq!(log.read(retained.at, MAX_READ_BYTES).unwrap(), [retained]);
    assert!(offset > HEADER_BYTES);
    assert_eq!(log.retained_bytes(), disk_bytes(dir.path()));
}

#[test]
fn complete_synced_frame_before_tail_publication_is_recovered_once() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    log.append(&output(10, b'a')).unwrap();
    let pending = TerminalRecord {
        at: log.tail(),
        kind: output(10, b'b'),
    };
    let path = final_path(&log);
    let mut file = open_private(&path, true).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    write_frame(&mut file, &postcard::to_allocvec(&pending).unwrap()).unwrap();
    file.sync_all().unwrap();
    drop(log);
    let mut log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.tail(), pending.end().unwrap());
    assert_eq!(
        log.read(pending.at, MAX_READ_BYTES).unwrap(),
        std::slice::from_ref(&pending)
    );
    drop(log);
    let mut log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.read(pending.at, MAX_READ_BYTES).unwrap(), [pending]);
}

#[test]
fn corrupt_watermarks_and_incorrect_checkpoint_pairs_are_errors() {
    for name in ["ack", "tail"] {
        let (dir, mut log) = journal(MIN_QUOTA_BYTES);
        log.append(&output(10, b'a')).unwrap();
        corrupt_byte(&dir.path().join(name), 16);
        drop(log);
        assert!(matches!(
            Journal::open(dir.path(), MIN_QUOTA_BYTES),
            Err(JournalError::Corrupt(_))
        ));
    }
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    log.append(&output(10, b'a')).unwrap();
    publish_ack(
        dir.path(),
        LogCursor {
            next_record: 1,
            next_byte: 9,
        },
    )
    .unwrap();
    drop(log);
    assert!(matches!(
        Journal::open(dir.path(), MIN_QUOTA_BYTES),
        Err(JournalError::Corrupt(_))
    ));
}

#[test]
fn failed_tail_publication_is_rolled_back_before_same_cursor_retry() {
    let (dir, mut log) = journal(MIN_QUOTA_BYTES);
    log.append(&output(10, b'a')).unwrap();
    let previous = log.tail();
    let original_len = log.segments.back().unwrap().len;
    let pending = TerminalRecord {
        at: previous,
        kind: output(10, b'b'),
    };
    let path = final_path(&log);
    let mut file = open_private(&path, true).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    write_frame(&mut file, &postcard::to_allocvec(&pending).unwrap()).unwrap();
    file.sync_all().unwrap();
    // Reproduce the state after rename succeeded but directory fsync failed:
    // the durable manifest may name the unexposed frame, while the API still
    // owns the old tail and promises a retry of the same pending input.
    publish_cursor(dir.path(), "tail", TAIL_MAGIC, pending.end().unwrap()).unwrap();
    log.dirty = Some(DirtyAppend {
        path,
        original_len: Some(original_len),
        extra: file.metadata().unwrap().len() - original_len,
    });
    let actual = log.append(&pending.kind).unwrap();
    assert_eq!(actual, pending);
    assert_eq!(log.tail(), pending.end().unwrap());
    drop(log);
    let mut log = Journal::open(dir.path(), MIN_QUOTA_BYTES).unwrap();
    assert_eq!(log.read(previous, MAX_READ_BYTES).unwrap(), [pending]);
}
