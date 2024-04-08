use std::ops::{Deref, DerefMut};
use std::sync::{Arc, atomic::Ordering};
use std::time::Instant;

use fst::Streamer;
use fst::map::{Map, OpBuilder};

use crate::log::{Log, index_entry_split};
use crate::shared_wal::WalLock;

pub enum Transaction {
    Write(WriteTransaction),
    Read(ReadTransaction),
}

impl Transaction {
    pub fn as_write_mut(&mut self) -> Option<&mut WriteTransaction> {
        if let Self::Write(ref mut v) = self {
            Some(v)
        } else {
            None
        }
    }

    pub(crate) fn commit(&mut self) {
        match self {
            Transaction::Write(tx) => {
                tx.is_commited = true;
            },
            Transaction::Read(_) => (),
        }
    }
}

impl Deref for Transaction {
    type Target = ReadTransaction;

    fn deref(&self) -> &Self::Target {
        match self {
            Transaction::Write(tx) => &tx,
            Transaction::Read(tx) => &tx,
        }
    }
}

impl DerefMut for Transaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Transaction::Write(ref mut tx) => tx,
            Transaction::Read(ref mut tx) => tx,
        }
    }
}

pub struct ReadTransaction {
    /// Max frame number that this transaction can read
    pub max_frame_no: u64,
    pub db_size: u32,
    /// The log to which we have a read lock
    pub log: Arc<Log>,
    pub created_at: Instant,
    pub conn_id: u64,
    /// number of pages read by this transaction. This is used to determine whether a write lock
    /// will be re-acquired.
    pub pages_read: usize,
}

impl Clone for ReadTransaction {
    fn clone(&self) -> Self {
        self.log.read_locks.fetch_add(1, Ordering::SeqCst);
        Self { max_frame_no: self.max_frame_no, log: self.log.clone(),  db_size: self.db_size, created_at: self.created_at, conn_id: self.conn_id, pages_read: self.pages_read }
    }
}

impl Drop for ReadTransaction {
    fn drop(&mut self) {
        // FIXME: if the count drops to 0, register for compaction.
        self.log.read_locks.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct Savepoint {
    pub next_offset: u32,
    pub next_frame_no: u64,
    pub index: Option<Map<Vec<u8>>>,
}

pub struct WriteTransaction {
    pub id: u64,
    /// id of the transaction currently holding the lock
    pub wal_lock: Arc<WalLock>,
    pub savepoints: Vec<Savepoint>,
    pub next_frame_no: u64,
    pub next_offset: u32,
    pub is_commited: bool,
    pub read_tx: ReadTransaction,
}

impl WriteTransaction {
    /// enter the lock critical section
    pub fn enter<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        if self.is_commited {
            tracing::error!("transaction already commited");
            todo!("txn has already been commited");
        }

        let wal_lock = self.wal_lock.clone();
        let g = wal_lock.tx_id.lock();
        match *g {
            // we still hold the lock, we can proceed
            Some(id) if self.id == id => {
                f(self)
            },
            // Somebody took the lock from us
            Some(_) => todo!("lock stolen"),
            None => todo!("not a transaction"),
        }
    }

    pub fn savepoint(&mut self) -> usize {
        let savepoint_id = self.savepoints.len();
        self.savepoints.push(Savepoint { next_offset: self.next_offset, next_frame_no: self.next_frame_no, index: None });
        savepoint_id
    }

    pub fn reset(&mut self, savepoint_id: usize) {
        if savepoint_id >= self.savepoints.len() {
            panic!("savepoint doesn't exist");
        }

        self.savepoints.drain(savepoint_id + 1..).count();
        self.next_frame_no = self.savepoints.last().unwrap().next_frame_no;
        self.next_offset = self.savepoints.last().unwrap().next_offset;
    }

    /// Returns an iterator over the current transaction index key/values
    pub fn index_iter(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        let iter = self.savepoints.iter().filter_map(|s| s.index.as_ref());
        let mut union = iter.collect::<OpBuilder>().union();
        std::iter::from_fn(move || {
            match union.next() {
                Some((key, vals)) => {
                    let key = u32::from_be_bytes(key.try_into().unwrap());
                    let val = vals.iter().max_by_key(|i| i.index).unwrap().value;
                    Some((key, val))
                },
                None => None,
            }
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn downgrade(self) -> ReadTransaction {
        tracing::trace!("downgrading write transaction");
        let Self { id, wal_lock, read_tx, .. } = self;
        let mut lock = wal_lock.tx_id.lock();
        match *lock {
            Some(lock_id) if lock_id == id => {
                lock.take();
            }
            _ => (),
        }

        if let Some(id) = *wal_lock.reserved.lock() {
            tracing::trace!("tx already reserved by {id}");
            return read_tx;
        }

        loop {
            match wal_lock.waiters.steal() {
                crossbeam::deque::Steal::Empty => {
                    tracing::trace!("no connection waiting");
                    break
                },
                crossbeam::deque::Steal::Success((unparker, id)) => {
                    tracing::trace!("waking up {id}");
                    wal_lock.reserved.lock().replace(id);
                    unparker.unpark();
                    break
                },
                crossbeam::deque::Steal::Retry => (),
            }
        }

        tracing::debug!(id=self.id, "lock released");

        read_tx
    }

    pub fn is_commited(&self) -> bool {
        self.is_commited
    }

    pub(crate) fn find_frame(&self, page_no: u32) -> Option<(u32, u32)> {
        let iter = self.savepoints.iter().rev().filter_map(|s| s.index.as_ref());
        for index in iter {
            if let Some(val) = index.get(page_no.to_be_bytes()) {
                return Some(index_entry_split(val))
            }
        }

        None
    }
}

impl Deref for WriteTransaction {
    type Target = ReadTransaction;

    fn deref(&self) -> &Self::Target {
        &self.read_tx
    }
}

impl DerefMut for WriteTransaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.read_tx
    }
}
