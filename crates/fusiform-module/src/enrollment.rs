//! The engram backup enrollment descriptor, written beside the store at startup.
//!
//! Engram, the fleet's backup owner, discovers what to capture by walking the
//! module data directories and reading `engram-catalog.json` from each one. A
//! module directory without that file is reported as not enrolled and its store
//! is never captured. The descriptor used to be copied into place by hand, so a
//! fresh machine never had one and backed nothing up; the module now writes it
//! itself, every time it opens its store.
//!
//! # Why the write is this careful
//!
//! The descriptor's entry is class `portable`. Engram treats an unreadable or
//! malformed portable descriptor as an error in the whole fleet capture run, not
//! just in this module's entry, so a truncated file here would stop backups for
//! every module on the machine. The write therefore goes to a temporary file,
//! is flushed to disk, and is renamed over the target in one step, and the
//! result is read back and compared with the bytes this binary carries.
//!
//! # Why failure does not stop the module
//!
//! A missing descriptor costs this module its backups; it does not affect a
//! single catalog read. Refusing to serve over it would turn a backup gap into
//! an outage. Failure is instead loud: a stderr line naming the path and the
//! error, and health reports Degraded until a restart writes the file.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::signals::Signals;

/// The file name engram looks for in each module directory.
pub const CATALOG_FILE_NAME: &str = "engram-catalog.json";

/// The database file name the descriptor's entry points at, relative to the
/// directory both files live in.
pub const STORE_FILE_NAME: &str = "store.db";

/// The descriptor exactly as the repository holds it.
///
/// Embedded rather than built from a struct, so the bytes engram reads are the
/// bytes that were reviewed in `data/engram-catalog.json`, with no serializer in
/// between that could reorder or reformat them.
pub const CATALOG_BYTES: &[u8] = include_bytes!("../data/engram-catalog.json");

/// The temporary name the descriptor is written under before the rename.
///
/// A fixed name rather than a unique one: only one fusiform holds the store's
/// writer lease at a time, so only one process writes here, and a fixed name
/// means a temporary file left by a crash is removed by the next start instead
/// of accumulating. It starts with a dot and does not end in `.json`, so
/// nothing reading the directory mistakes it for a descriptor.
const TEMP_FILE_NAME: &str = ".engram-catalog.json.tmp";

/// What [`write_catalog`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogWrite {
    /// No descriptor existed; one was written.
    Created,
    /// A descriptor with exactly these bytes already existed. Only its mode was
    /// set, to 0600.
    Unchanged,
    /// A descriptor with different bytes existed and was replaced. `old_len` is
    /// `None` when the old file could not be read to measure it.
    Replaced { old_len: Option<u64>, new_len: u64 },
}

/// A failed descriptor write, naming the file and the step that failed.
#[derive(Debug)]
pub struct CatalogWriteError {
    pub path: PathBuf,
    pub step: &'static str,
    pub source: io::Error,
}

impl std::fmt::Display for CatalogWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.step, self.path.display(), self.source)
    }
}

impl std::error::Error for CatalogWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn step_err(step: &'static str, path: &Path) -> impl FnOnce(io::Error) -> CatalogWriteError {
    let path = path.to_path_buf();
    move |source| CatalogWriteError { path, step, source }
}

/// Write the embedded descriptor as `engram-catalog.json` in `dir`.
///
/// `dir` is the directory holding the store, so the descriptor's relative entry
/// path `store.db` resolves to the database actually in use.
///
/// When a descriptor with identical bytes is already there, its mode is set to
/// 0600 and nothing else is touched: rewriting it would change its modification
/// time on every start for no reason. Otherwise the bytes go to a temporary file
/// in the same directory, which is flushed to disk, set to 0600, renamed over
/// the target, and read back and compared. Any failure before the rename removes
/// the temporary file, so a failed write never leaves a partial descriptor.
pub fn write_catalog(dir: &Path) -> Result<CatalogWrite, CatalogWriteError> {
    let target = dir.join(CATALOG_FILE_NAME);

    // `None`: nothing to replace. `Some(Some(len))`: replacing a file of that
    // many bytes. `Some(None)`: replacing a file that could not be read.
    let replacing = match fs::read(&target) {
        Ok(existing) if existing == CATALOG_BYTES => {
            // Identical bytes say nothing about the mode. A descriptor placed by
            // hand or restored from a backup can match and still be readable by
            // every user on the machine, and it is a precise map of which files
            // here are worth taking, so it gets the same 0600 as the store.
            set_private(&target).map_err(step_err("could not set the mode of", &target))?;
            return Ok(CatalogWrite::Unchanged);
        }
        // A live descriptor that differs from this binary's is OVERWRITTEN.
        //
        // When the descriptor was installed by hand, a differing live file meant
        // someone had edited it, and the installer refused rather than destroy
        // that edit. Now that the module writes the descriptor itself on every
        // start, the repository's bytes are the authority: an edit to the live
        // file is not reviewed anywhere, and keeping a stale descriptor risks
        // backing up the wrong thing. To change the descriptor, change the
        // repository file. The caller logs the old and new sizes so the
        // replacement is visible.
        Ok(existing) => Some(Some(existing.len() as u64)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        // Present but unreadable. Replaced too: the rename below does not need
        // to read the old file, and a descriptor this process cannot read is
        // one engram may not be able to read either.
        Err(_) => Some(None),
    };

    let temp = dir.join(TEMP_FILE_NAME);
    if let Err(source) = write_temp(&temp) {
        let _ = fs::remove_file(&temp);
        return Err(source);
    }
    if let Err(source) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(step_err(
            "could not rename the temporary descriptor over",
            &target,
        )(source));
    }

    // Flush the directory entry too, so the rename itself survives a crash.
    // Best effort: some platforms cannot open a directory for syncing, and the
    // file's own bytes are already on disk.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }

    // Read back and compare against the EMBEDDED bytes, not against what was
    // just written: comparing the file with itself would pass on a truncated
    // write, because the truncation is what would be compared.
    let written = fs::read(&target).map_err(step_err("could not read back", &target))?;
    if written != CATALOG_BYTES {
        // Removed rather than left in place. A missing descriptor costs this
        // module its backups; a wrong one can fail engram's capture of every
        // module on the machine.
        let _ = fs::remove_file(&target);
        return Err(CatalogWriteError {
            path: target,
            step: "read-back did not match the embedded descriptor, so removed",
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "read {} bytes, expected {}",
                    written.len(),
                    CATALOG_BYTES.len()
                ),
            ),
        });
    }

    Ok(match replacing {
        None => CatalogWrite::Created,
        Some(old_len) => CatalogWrite::Replaced {
            old_len,
            new_len: CATALOG_BYTES.len() as u64,
        },
    })
}

/// Create the temporary file fresh, write the descriptor, flush it to disk and
/// set it to 0600. The caller removes the file if this fails.
fn write_temp(temp: &Path) -> Result<(), CatalogWriteError> {
    // A leftover from a crashed start is removed first, so the create below can
    // insist on a new file rather than writing through whatever is there.
    match fs::remove_file(temp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(step_err("could not remove the stale temporary file", temp)(
                e,
            ))
        }
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Private from creation, so the descriptor is never readable by other
        // users even for the moment before the explicit mode change below.
        options.mode(0o600);
    }
    let mut file = options
        .open(temp)
        .map_err(step_err("could not create the temporary descriptor", temp))?;
    file.write_all(CATALOG_BYTES)
        .map_err(step_err("could not write the temporary descriptor", temp))?;
    file.sync_all()
        .map_err(step_err("could not flush the temporary descriptor", temp))?;
    // Set explicitly as well: the creation mode above is only an upper bound
    // filtered through the umask, and the result has to be exactly 0600.
    set_private(temp).map_err(step_err("could not set the mode of", temp))?;
    Ok(())
}

#[cfg(unix)]
fn set_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Write the descriptor beside the store the module opened, logging what
/// happened and flagging a failure for health.
///
/// `store_path` is the database file the store actually opened. The directory
/// comes from it rather than from the environment, because a path re-derived
/// from environment variables can differ from the one the supervisor handed
/// over, and a descriptor beside the wrong database protects nothing.
///
/// Never returns an error: a backup gap must not stop the module serving. A
/// failure is reported on stderr and through [`Signals::catalog_write_failed`],
/// which makes health Degraded.
pub fn write_beside_store(store_path: &Path, signals: &Signals) {
    // The descriptor's entry names `store.db`, so it only describes this store
    // when the store has that name. Writing it beside a differently named
    // database would enroll a file that does not exist.
    if store_path.file_name() != Some(std::ffi::OsStr::new(STORE_FILE_NAME)) {
        signals.catalog_write_failed();
        eprintln!(
            "fusiform: did not write the backup enrollment descriptor: the store \
             at {} is not named {STORE_FILE_NAME}, which is the file the \
             descriptor enrolls, so engram will not back this store up",
            store_path.display()
        );
        return;
    }
    let dir = match store_path.parent() {
        Some(p) if p.as_os_str().is_empty() => Path::new("."),
        Some(p) => p,
        None => Path::new("."),
    };
    let target = dir.join(CATALOG_FILE_NAME);

    match write_catalog(dir) {
        Ok(CatalogWrite::Unchanged) => {}
        Ok(CatalogWrite::Created) => {
            eprintln!(
                "fusiform: wrote the backup enrollment descriptor {}",
                target.display()
            )
        }
        Ok(CatalogWrite::Replaced { old_len, new_len }) => {
            let old = old_len.map_or_else(|| "unreadable".to_string(), |n| format!("{n} bytes"));
            eprintln!(
                "fusiform: replaced the backup enrollment descriptor {} because it \
                 differed from this build's ({old} -> {new_len} bytes)",
                target.display()
            )
        }
        Err(e) => {
            signals.catalog_write_failed();
            eprintln!(
                "fusiform: could not write the backup enrollment descriptor {}, so \
                 engram will not back up this module's store; serving is \
                 unaffected: {e}",
                target.display()
            );
        }
    }
}
