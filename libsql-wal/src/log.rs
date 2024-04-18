use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, IoSlice, Write};
use std::mem::size_of;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use fst::{map::Map, MapBuilder};
use parking_lot::{Mutex, RwLock};
use zerocopy::byteorder::little_endian::{U32, U64};
use zerocopy::{AsBytes, FromZeroes};

use crate::error::Result;
use crate::file::FileExt;
use crate::transaction::{merge_savepoints, Transaction, WriteTransaction};

pub struct Log {
    path: PathBuf,
    index: LogIndex,
    header: Mutex<LogHeader>,
    file: File,
    /// Read lock count on this Log. Each begin_read increments the count of readers on the current
    /// lock
    pub read_locks: Arc<AtomicU64>,
    pub sealed: AtomicBool,
}

impl Drop for Log {
    fn drop(&mut self) {
        // todo: if reader is 0 and log is sealed, register for compaction.
    }
}

#[derive(Default)]
struct LogIndex {
    start_frame_no: u64,
    index: RwLock<BTreeMap<u32, Vec<u32>>>,
}

impl LogIndex {
    fn locate(&self, page_no: u32, max_frame_no: u64) -> Option<u32> {
        let index = self.index.read();
        let offsets = index.get(&page_no)?;
        offsets
            .iter()
            .rev()
            .find(|fno| self.start_frame_no + **fno as u64 <= max_frame_no)
            .copied()
    }

    #[tracing::instrument(skip_all)]
    fn merge_all<W: Write>(&self, writer: W) -> Result<()> {
        let index = self.index.read();
        let mut builder = MapBuilder::new(writer)?;
        for (key, entries) in index.iter() {
            let offset = *entries.last().unwrap();
            builder.insert(key.to_be_bytes(), offset as u64)?;
        }

        builder.finish()?;
        Ok(())
    }
}

#[repr(C)]
#[derive(Debug, zerocopy::AsBytes, zerocopy::FromBytes, zerocopy::FromZeroes, Clone, Copy)]
pub struct LogHeader {
    start_frame_no: U64,
    pub last_commited_frame_no: U64,
    pub db_size: U32,
    /// byte offset of the index. If 0, then the index wasn't written, and must be recovered.
    /// If non-0, the log is sealed, and must not be written to anymore
    index_offset: U64,
    index_size: U64,
}

impl LogHeader {
    fn is_empty(&self) -> bool {
        self.last_commited_frame_no.get() == 0
    }

    fn count_committed(&self) -> usize {
        self.last_commited_frame_no
            .get()
            .checked_sub(self.start_frame_no.get() - 1)
            .unwrap_or(0) as usize
    }

    fn last_committed(&self) -> u64 {
        // either the current log is empty, and the start frame_no is the last frame_no commited on
        // the previous log (start_frame_no - 1), or it's the last committed frame_no from this
        // log.
        if self.is_empty() {
            self.start_frame_no.get() - 1
        } else {
            self.last_commited_frame_no.get()
        }
    }

    pub(crate) fn next_frame_no(&self) -> NonZeroU64 {
        if self.is_empty() {
            NonZeroU64::new(self.start_frame_no.get()).unwrap()
        } else {
            NonZeroU64::new(self.last_commited_frame_no.get() + 1).unwrap()
        }
    }
}

/// split the index entry value into it's components: (frame_no, offset)
pub fn index_entry_split(k: u64) -> (u32, u32) {
    let offset = (k & u32::MAX as u64) as u32;
    let frame_no = (k >> 32) as u32;
    (frame_no, offset)
}

#[repr(C)]
#[derive(Debug, zerocopy::AsBytes, zerocopy::FromBytes, zerocopy::FromZeroes)]
struct FrameHeader {
    page_no: U32,
    size_after: U32,
}

#[repr(C)]
#[derive(Debug, zerocopy::AsBytes, zerocopy::FromBytes, zerocopy::FromZeroes)]
struct Frame {
    header: FrameHeader,
    data: [u8; 4096],
}

fn byte_offset(offset: u32) -> u64 {
    (size_of::<LogHeader>() + (offset as usize) * size_of::<Frame>()) as u64
}

impl Log {
    /// Create a new log from the given path and metadata. The file pointed to by path must not
    /// exist.
    pub fn create(path: &Path, start_frame_no: NonZeroU64, db_size: u32) -> Result<Self> {
        let log_file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(path)?;

        let header = LogHeader {
            start_frame_no: start_frame_no.get().into(),
            last_commited_frame_no: 0.into(),
            db_size: db_size.into(),
            index_offset: 0.into(),
            index_size: 0.into(),
        };

        log_file.write_all_at(header.as_bytes(), 0)?;

        Ok(Self {
            path: path.to_path_buf(),
            index: LogIndex::default(),
            header: Mutex::new(header),
            file: log_file,
            read_locks: Arc::new(AtomicU64::new(0)),
            sealed: AtomicBool::default(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.count_committed() == 0
    }
    /// Returns the db size and the last commited frame_no
    #[tracing::instrument(skip_all)]
    pub fn begin_read_infos(&self) -> (u64, u32) {
        let header = self.header.lock();
        (header.last_committed(), header.db_size.get())
    }

    pub fn last_committed(&self) -> u64 {
        self.header.lock().last_committed()
    }

    pub fn next_frame_no(&self) -> NonZeroU64 {
        self.header.lock().next_frame_no()
    }

    pub fn count_committed(&self) -> usize {
        self.header.lock().count_committed()
    }

    pub fn db_size(&self) -> u32 {
        self.header.lock().db_size.get()
    }

    #[tracing::instrument(skip(self, pages, tx))]
    pub fn insert_pages<'a>(
        &self,
        pages: impl Iterator<Item = (u32, &'a [u8])>,
        size_after: Option<u32>,
        tx: &mut WriteTransaction,
    ) -> Result<()> {
        assert!(!self.sealed.load(Ordering::SeqCst));
        tx.enter(move |tx| {
            let mut pages = pages.peekable();
            // let mut commit_frame_written = false;
            let current_savepoint = tx.savepoints.last_mut().expect("no savepoints initialized");
            while let Some((page_no, page)) = pages.next() {
                tracing::trace!(page_no, "inserting page");
                // match current_savepoint
                //     .index
                //     .as_ref()
                //     .and_then(|i| i.get(&page_no.to_be_bytes()))
                // {
                //     Some(x) => {
                //         let header = FrameHeader {
                //             page_no: page_no.into(),
                //             // set the size_after if it's the last frame in a commit batch
                //             size_after: 0.into(),
                //         };
                //         // there is already an occurence of this page in the current transaction, replace it
                //         let (fno_offset, offset) = index_entry_split(x);
                //         let fno = self.header.lock().start_frame_no.get() + fno_offset as u64;
                //         let fno_bytes = &fno.to_be_bytes()[..];
                //         let slices = &[
                //             IoSlice::new(header.as_bytes()),
                //             IoSlice::new(&page[..4096 - 8]),
                //             // store the replication index in big endian as per SQLite convention,
                //             // at the end of the page
                //             IoSlice::new(fno_bytes),
                //         ];
                //
                //         self.file
                //             .write_at_vectored(slices, byte_offset(offset) as u64)?;
                //     }
                //     None => {
                let size_after = if let Some(size) = size_after {
                    pages.peek().is_none().then_some(size).unwrap_or(0)
                } else {
                    0
                };

                // commit_frame_written = size_after != 0;

                let header = FrameHeader {
                    page_no: page_no.into(),
                    size_after: size_after.into(),
                };
                let frame_no = tx.next_frame_no;
                let frame_no_bytes = frame_no.to_be_bytes();
                let slices = &[
                    IoSlice::new(header.as_bytes()),
                    IoSlice::new(&page[..4096 - 8]),
                    // store the replication index in big endian as per SQLite convention,
                    // at the end of the page
                    IoSlice::new(&frame_no_bytes),
                ];
                tx.next_frame_no += 1;
                let offset = tx.next_offset;
                tx.next_offset += 1;
                self.file.write_at_vectored(slices, byte_offset(offset))?;
                current_savepoint.index.insert(page_no, offset);
            }
            // }
            // }

            if let Some(size_after) = size_after {
                if tx.savepoints.len() == 1
                    && tx
                        .savepoints
                        .last()
                        .expect("missing savepoint")
                        .index
                        .is_empty()
                {
                    // nothing to do
                } else {
                    // let indexes = tx.savepoints.iter().map(|i| i.index);
                    // let merged = merge_indexes(indexes)?;
                    if tx.savepoints.iter().any(|s| !s.index.is_empty()) {
                        let last_frame_no = tx.next_frame_no - 1;
                        let mut header = { *self.header.lock() };
                        header.last_commited_frame_no = last_frame_no.into();
                        header.db_size = size_after.into();

                        // if !commit_frame_written {
                        //     // need to patch the last frame header
                        //     self.patch_frame_size_after(tx.next_offset - 1, size_after)?;
                        // }

                        self.file.write_all_at(header.as_bytes(), 0)?;
                        // self.file.sync_data().unwrap();
                        let savepoints = tx.savepoints.iter().rev().map(|s| &s.index);
                        merge_savepoints(savepoints, &mut self.index.index.write());
                        // set the header last, so that a transaction does not witness a write before
                        // it's actually committed.
                        *self.header.lock() = header;
                    }
                }

                tx.is_commited = true;
            }

            Ok(())
        })
    }

    /// return the offset of the frame for page_no, with frame_no no larger that max_frame_no, if
    /// it exists
    pub fn find_frame(&self, page_no: u32, tx: &Transaction) -> Option<u32> {
        // TODO: ensure that we are looking in the same log as the passed transaction
        // this is a write transaction, check the transient index for request page
        if let Transaction::Write(ref tx) = tx {
            if let Some(offset) = tx.find_frame_offset(page_no) {
                return Some(offset);
            }
        }

        // not a write tx, or page is not in write tx, look into the log
        self.index.locate(page_no, tx.max_frame_no)
    }

    /// reads the page conainted in frame at offset into buf
    #[tracing::instrument(skip(self, buf))]
    pub fn read_page_offset(&self, offset: u32, buf: &mut [u8]) -> Result<()> {
        tracing::trace!("read page");
        debug_assert_eq!(buf.len(), 4096);
        self.file.read_exact_at(buf, page_offset(offset))?;

        Ok(())
    }

    #[allow(dead_code)]
    fn frame_header_at(&self, offset: u32) -> Result<FrameHeader> {
        let mut header = FrameHeader::new_zeroed();
        self.file
            .read_exact_at(header.as_bytes_mut(), byte_offset(offset))?;
        Ok(header)
    }

    #[tracing::instrument(skip_all)]
    pub fn seal(&self) -> Result<SealedLog> {
        assert!(
            self.sealed
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok(),
            "attempt to seal an already sealed log"
        );
        let mut header = self.header.lock();
        let index_offset = header.count_committed() as u32;
        let index_byte_offset = byte_offset(index_offset);
        let mut cursor = self.file.cursor(index_byte_offset);
        let mut writer = BufWriter::new(&mut cursor);
        self.index.merge_all(&mut writer)?;
        writer.into_inner().unwrap();
        header.index_offset = index_byte_offset.into();
        header.index_size = cursor.count().into();
        self.file.write_all_at(header.as_bytes(), 0)?;

        tracing::debug!("log sealed");

        Ok(SealedLog::open(
            self.file.try_clone()?,
            self.path.clone(),
            self.read_locks.clone(),
        )?
        .expect("log is not empty"))
    }
}

fn page_offset(offset: u32) -> u64 {
    byte_offset(offset) + size_of::<FrameHeader>() as u64
}

/// an immutable, sealed, memory mapped log file.
pub struct SealedLog {
    pub read_locks: Arc<AtomicU64>,
    header: LogHeader,
    file: File,
    index: Map<Vec<u8>>,
    path: PathBuf,
    checkpointed: AtomicBool,
}

impl SealedLog {
    pub fn open(file: File, path: PathBuf, read_locks: Arc<AtomicU64>) -> Result<Option<Self>> {
        let mut header: LogHeader = LogHeader::new_zeroed();
        file.read_exact_at(header.as_bytes_mut(), 0)?;

        let index_offset = header.index_offset.get();
        let index_len = header.index_size.get();
        if index_offset == 0 {
            return Self::recover(file, header);
        }

        let mut slice = vec![0; index_len as usize];
        file.read_exact_at(&mut slice, index_offset)?;
        let index = Map::new(slice)?;
        Ok(Some(Self {
            file,
            path,
            read_locks,
            checkpointed: false.into(),
            index,
            header,
        }))
    }

    fn recover(_file: File, header: LogHeader) -> Result<Option<Self>> {
        if header.last_commited_frame_no.get() == 0 {
            return Ok(None);
        }
        todo!();
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn header(&self) -> &LogHeader {
        &self.header
    }

    // TODO: building the Map on each call could be costly (maybe?), find a way to cache it and
    // return a ref. It will likely require some unsafe, since the index references the mmaped log,
    // owned by self.
    pub fn index(&self) -> &Map<Vec<u8>> {
        &self.index
    }

    pub fn read_offset(&self, offset: u32, buf: &mut [u8]) -> Result<()> {
        let page_offset = page_offset(offset) as usize;
        self.file.read_exact_at(buf, page_offset as _)?;

        Ok(())
    }

    pub fn read_page(&self, page_no: u32, max_frame_no: u64, buf: &mut [u8]) -> Result<bool> {
        if self.header().last_commited_frame_no.get() > max_frame_no {
            return Ok(false);
        }

        let index = self.index();
        if let Some(value) = index.get(page_no.to_be_bytes()) {
            let (_f, offset) = index_entry_split(value);
            self.read_offset(offset, buf)?;

            return Ok(true);
        }

        Ok(false)
    }

    pub(crate) fn checkpointed(&self) {
        self.checkpointed.store(true, Ordering::SeqCst);
    }
}

impl Drop for SealedLog {
    fn drop(&mut self) {
        if self.checkpointed.load(Ordering::SeqCst) {
            // todo: recycle?;
            if let Err(e) = std::fs::remove_file(self.path()) {
                tracing::error!("failed to remove log file: {e}");
            }
        }
    }
}
