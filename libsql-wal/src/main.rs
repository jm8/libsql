use std::{path::Path, time::Instant};
use std::sync::Arc;

use libsql_sys::rusqlite::{OpenFlags, self};
use libsql_wal::registry::WalRegistry;
use libsql_wal::wal::LibsqlWalManager;

use tracing::Level;
use tracing_subscriber::{EnvFilter, fmt::{self, format::FmtSpan}, prelude::*};

fn enable_libsql_logging() {
    use std::ffi::c_int;
    use std::sync::Once;
    static ONCE: Once = Once::new();

    #[tracing::instrument(skip_all)]
    fn libsql_log(code: c_int, msg: &str) {
        tracing::error!("sqlite error {code}: {msg}");
    }

    ONCE.call_once(|| unsafe {
        rusqlite::trace::config_log(Some(libsql_log)).unwrap();
    });
}

fn main() {
    tracing_subscriber::registry()
        // .with(fmt::layer())
        .with(fmt::layer()
            .with_span_events(FmtSpan::CLOSE))
        .with(EnvFilter::from_default_env())
        .init();

    enable_libsql_logging();

    let path = std::env::args().nth(1).unwrap();
    let path = <str as AsRef<Path>>::as_ref(path.as_str());
    std::fs::create_dir_all(&path).unwrap();
    let registry = Arc::new(WalRegistry::new(path.join("wals")));
    let wal_manager = LibsqlWalManager {
        registry: registry.clone(),
        namespace: "test".into(),
        next_conn_id: Default::default(),
    };

    let db_path: Arc<Path> = path.join("data").into();
    let conn = libsql_sys::Connection::open(db_path.clone(), OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_READ_WRITE, wal_manager.clone(), 100000, None).unwrap();
  
    let _ = conn.execute("CREATE TABLE t1(a INTEGER PRIMARY KEY, b BLOB(16), c BLOB(16), d BLOB(400));", ());
    let _ = conn.execute("CREATE INDEX i1 ON t1(b);", ());
    let _ = conn.execute("CREATE INDEX i2 ON t1(c);", ());

    let mut handles = Vec::new();
    for w in 0..50 {
        let handle = std::thread::spawn({
            let wal_manager = wal_manager.clone();
            let db_path = db_path.clone();
            move || {
                let span = tracing::span!(Level::TRACE, "conn", w);
                let _enter = span.enter();
                let mut conn = libsql_sys::Connection::open(db_path, OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_READ_WRITE, wal_manager, 100000, None).unwrap();
                for _i in 0..10_000 {
                    let before = Instant::now();
                    let tx = conn.transaction().unwrap();
                    tx.execute("REPLACE INTO t1 VALUES(abs(random() % 5000000), randomblob(16), randomblob(16), randomblob(400));", ()).unwrap();
                    tx.execute("REPLACE INTO t1 VALUES(abs(random() % 5000000), randomblob(16), randomblob(16), randomblob(400));", ()).unwrap();
                    tx.execute("REPLACE INTO t1 VALUES(abs(random() % 5000000), randomblob(16), randomblob(16), randomblob(400));", ()).unwrap();
                    tx.commit().unwrap();
                    println!("time: {:?}", before.elapsed().as_micros());
                }
            }
        });
    
        handles.push(handle);
    }
    
    // let before = Instant::now();
    for handle in handles {
        handle.join().unwrap();
    }

    conn.query_row("select count(0) from t1", (), |r| {
        dbg!(r);
        Ok(())
    }).unwrap();

    drop(conn);

    registry.shutdown();

    // let lines = std::io::stdin().lines();
    // for line in lines {
    //     let line = line.unwrap();
    //     if line.trim() == "quit" {
    //         break;
    //     }
    //     let mut stmt = conn.prepare(&line).unwrap();
    //     let mut rows = stmt.query(()).unwrap();
    //     while let Ok(Some(row)) = rows.next() {
    //         dbg!(row);
    //     }
    // }
}
