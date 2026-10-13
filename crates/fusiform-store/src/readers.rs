use std::sync::{Condvar, Mutex};
use std::time::Duration;

use cortexkit_store::StoreError;
use rusqlite::{Connection, OpenFlags};

/// Four read-only connections let a slow catalog query overlap both the writer
/// and other reads without opening a connection per request. When all four are
/// busy, readers wait for a returned connection, never for the writer's mutex.
/// Each checkout is exclusive and covers only the callback, so completed reads
/// do not pin a WAL snapshot across requests.
pub(crate) struct ReadPool {
    available: Mutex<Vec<Connection>>,
    returned: Condvar,
}

impl ReadPool {
    pub(crate) fn open(path: &str) -> Result<Self, StoreError> {
        let mut connections = Vec::with_capacity(4);
        for _ in 0..4 {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;
            // Match the writer's timeout for transient SQLite locks. WAL readers
            // still read the last commit while a writer transaction is open.
            conn.busy_timeout(Duration::from_secs(5))
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            connections.push(conn);
        }
        Ok(Self {
            available: Mutex::new(connections),
            returned: Condvar::new(),
        })
    }

    pub(crate) fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, StoreError> {
        let mut available = self.available.lock().unwrap_or_else(|p| p.into_inner());
        let conn = loop {
            if let Some(conn) = available.pop() {
                break conn;
            }
            available = self
                .returned
                .wait(available)
                .unwrap_or_else(|p| p.into_inner());
        };
        drop(available);
        let reader = Reader {
            pool: self,
            conn: Some(conn),
        };
        f(reader.conn.as_ref().expect("checked out connection"))
            .map_err(|e| StoreError::Backend(e.to_string()))
    }
}

/// Return the connection even if the callback errors or unwinds; otherwise a
/// failed read could permanently shrink the pool and strand waiting requests.
struct Reader<'a> {
    pool: &'a ReadPool,
    conn: Option<Connection>,
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        let conn = self.conn.take().expect("checked out connection");
        self.pool
            .available
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(conn);
        self.pool.returned.notify_one();
    }
}
