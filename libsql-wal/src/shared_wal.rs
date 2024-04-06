use std::collections::VecDeque;
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use crossbeam::deque::Injector;
use crossbeam::sync::Unparker;
use fst::Streamer;
use fst::map::OpBuilder;
use libsql_sys::ffi::Sqlite3DbHeader;
use libsql_sys::wal::PageHeaders;
use parking_lot::{Mutex, RwLock};
use zerocopy::FromBytes;

use crate::error::Error;
use crate::file::FileExt;
use crate::log::{Log, index_entry_split};
use crate::log::SealedLog;
use crate::name::NamespaceName;
use crate::registry::WalRegistry;
use crate::transaction::Transaction;
use crate::transaction::{ReadTransaction, Savepoint, WriteTransaction};

pub struct SharedWal {
    pub current: ArcSwap<Log>,
    pub segments: RwLock<VecDeque<SealedLog>>,
    /// Current transaction id
    pub tx_id: Arc<Mutex<Option<u64>>>,
    pub next_tx_id: AtomicU64,
    pub db_file: File,
    pub waiters: Arc<Injector<Unparker>>,
    pub namespace: NamespaceName,
    pub registry: Arc<WalRegistry>,
}

impl SharedWal {
    pub fn db_size(&self) -> u32 {
        self.current.load().db_size()
    }

    #[tracing::instrument(skip_all)]
    pub fn begin_read(&self) -> ReadTransaction {
        // FIXME: this is not enough to just increment the counter, we must make sure that the log
        // is not sealed. If the log is sealed, retry with the current log
        loop {
            let current = self.current.load();
            // FIXME: This function comes up a lot more than in should in profiling. I suspect that
            // this is caused by those expensive loads here
            current.read_locks.fetch_add(1, Ordering::SeqCst);
            if current.sealed.load(Ordering::SeqCst) {
                continue;
            }
            let (max_frame_no, db_size) = current.begin_read_infos();
            return ReadTransaction {
                max_frame_no,
                log: current.clone(),
                db_size,
                created_at: Instant::now()
            };
        }
    }

    pub fn upgrade(&self, tx: &mut Transaction) -> Result<(), Error> {
        match tx {
            Transaction::Write(_) => todo!("already in a write transaction"),
            Transaction::Read(read_tx) => {
                loop {
                    let mut lock = self.tx_id.lock();
                    match *lock {
                        Some(id) => {
                            // FIXME this is not ver fair, always enqueue to the queue before acquiring
                            // lock
                            tracing::trace!(
                                "txn currently held by {id}, registering to wait queue"
                            );
                            let parker = crossbeam::sync::Parker::new();
                            let unpaker = parker.unparker().clone();
                            self.waiters.push(unpaker);
                            drop(lock);
                            parker.park();
                        }
                        None => {
                            let id = self.next_tx_id.fetch_add(1, Ordering::Relaxed);
                            // we read two fields in the header. There is no risk that a transaction commit in
                            // between the two reads because this would require that:
                            // 1) there would be a running txn
                            // 2) that transaction held the lock to tx_id (be in a transaction critical section)
                            let current = self.current.load();
                            let last_commited = current.last_commited();
                            if read_tx.max_frame_no != last_commited {
                                return Err(Error::BusySnapshot);
                            }
                            let next_offset = current.frames_in_log() as u32;
                            *lock = Some(id);
                            *tx = Transaction::Write(WriteTransaction {
                                id,
                                lock: self.tx_id.clone(),
                                savepoints: vec![Savepoint {
                                    next_offset,
                                    next_frame_no: last_commited + 1,
                                    index: None,
                                }],
                                next_frame_no: last_commited + 1,
                                next_offset,
                                is_commited: false,
                                read_tx: read_tx.clone(),
                                waiters: self.waiters.clone(),
                            });
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    pub fn read_frame(&self, tx: &Transaction, page_no: u32, buffer: &mut [u8]) {
        match tx.log.find_frame(page_no, tx) {
            Some((_, offset)) => tx.log.read_page_offset(offset, buffer),
            None => {
                // locate in segments
                if !self.read_from_segments(page_no, tx.max_frame_no, buffer) {
                    // read from db_file
                    self.db_file
                        .read_exact_at(buffer, (page_no as u64 - 1) * 4096)
                        .unwrap();
                }
            }
        }

        if page_no == 1 {
            let header = Sqlite3DbHeader::read_from_prefix(&buffer).unwrap();
            tracing::info!(db_size = header.db_size.get(), "read page 1");
        }

        let frame_no = u64::from_be_bytes(buffer[4096 - 8..].try_into().unwrap());
        tracing::trace!(frame_no, tx = tx.max_frame_no, "read page");
        assert!(frame_no <= tx.max_frame_no);
    }

    fn read_from_segments(&self, page_no: u32, max_frame_no: u64, buf: &mut [u8]) -> bool {
        let segs = self.segments.read();
        let mut prev_seg = u64::MAX;
        for (i, seg) in segs.iter().rev().enumerate() {
            let last = seg.header().last_commited_frame_no.get();
            assert!(prev_seg > last);
            prev_seg = last;
            if seg.read_page(page_no, max_frame_no, buf) {
                tracing::trace!("found {page_no} in segment {i}");
                return true;
            }
        }

        false
    }

    #[tracing::instrument(skip_all, fields(tx_id = tx.id))]
    pub fn insert_frames(
        &self,
        tx: &mut WriteTransaction,
        pages: &mut PageHeaders,
        size_after: u32,
    ) {
        let current = self.current.load();
        current.insert_pages(pages.iter(), (size_after != 0).then_some(size_after), tx);

        // TODO: use config for max log size
        if tx.is_commited() && current.len() > 1000 {
            self.registry.swap_current(self, tx);
        }

        // TODO: remove, stupid strategy for tests
        // ok, we still hold a write txn
        if self.segments.read().len() > 10 {
            self.checkpoint()
        }
    }

    pub fn checkpoint(&self) {
        let mut segs = self.segments.upgradable_read();
        let indexes = segs.iter().take_while(|s| s.read_locks.load(Ordering::SeqCst) == 0).map(|s| s.index()).collect::<Vec<_>>();

        // nothing to checkpoint rn
        if indexes.is_empty() {
            return
        }

        dbg!(indexes.len());

        let mut union = indexes.iter().collect::<OpBuilder>().union();
        while let Some((k, v)) = union.next() {
            let page_no = u32::from_be_bytes(k.try_into().unwrap());
            let v = v.iter().max_by_key(|i| i.index).unwrap();
            let seg = &segs[v.index];
            let (_, offset) = index_entry_split(v.value);
            self.db_file.write_all_at(seg.read_offset(offset), (page_no as u64 - 1) * 4096).unwrap();
        }

        self.db_file.sync_all().unwrap();

        let seg_count = indexes.len();

        drop(union);
        drop(indexes);

        let paths = segs.with_upgraded(|segs| {
            segs.drain(..seg_count).map(|s| s.into_path()).collect::<Vec<_>>()
        });

        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
    }
}
