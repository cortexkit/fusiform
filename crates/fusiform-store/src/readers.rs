use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use cortexkit_store::StoreError;
use rusqlite::{Connection, OpenFlags};

thread_local! {
    // A snapshot is synchronous and cannot move between threads. The borrow
    // of the stack ends before invoking a callback, which can read again.
    static SNAPSHOTS: RefCell<Vec<(usize, Rc<Connection>)>> = const { RefCell::new(Vec::new()) };
}

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
        let result = if let Some(conn) = self.scoped_conn() {
            f(&conn)
        } else {
            f(self.checkout().conn())
        };
        result.map_err(|e| StoreError::Backend(e.to_string()))
    }

    pub(crate) fn with_snapshot<T>(&self, read: impl FnOnce() -> T) -> Result<T, StoreError> {
        if self.scoped_conn().is_some() {
            return Ok(read());
        }
        let reader = self.checkout();
        let tx = reader
            .conn()
            .unchecked_transaction()
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let scope = SnapshotScope::push(self.key(), Rc::clone(reader.conn()));
        let result = read();
        drop(scope);
        tx.rollback()
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(result)
    }

    fn key(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    fn scoped_conn(&self) -> Option<Rc<Connection>> {
        SNAPSHOTS.with_borrow(|scopes| {
            scopes
                .iter()
                .rev()
                .find(|(key, _)| *key == self.key())
                .map(|(_, conn)| Rc::clone(conn))
        })
    }

    fn checkout(&self) -> Reader<'_> {
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
        Reader {
            pool: self,
            conn: Some(Rc::new(conn)),
        }
    }
}

struct SnapshotScope;

impl SnapshotScope {
    fn push(key: usize, conn: Rc<Connection>) -> Self {
        SNAPSHOTS.with_borrow_mut(|scopes| scopes.push((key, conn)));
        Self
    }
}

impl Drop for SnapshotScope {
    fn drop(&mut self) {
        SNAPSHOTS.with_borrow_mut(|scopes| scopes.pop());
    }
}

/// Return the connection even if the callback errors or unwinds; otherwise a
/// failed read could permanently shrink the pool and strand waiting requests.
struct Reader<'a> {
    pool: &'a ReadPool,
    conn: Option<Rc<Connection>>,
}

impl Reader<'_> {
    fn conn(&self) -> &Rc<Connection> {
        self.conn.as_ref().expect("checked out connection")
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        let conn = Rc::into_inner(self.conn.take().expect("checked out connection"))
            .expect("snapshot scope already ended");
        self.pool
            .available
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(conn);
        self.pool.returned.notify_one();
    }
}
