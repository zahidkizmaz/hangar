//! One hangar at a time: commands that change state hold an exclusive
//! lock on `$XDG_STATE_HOME/hangar/lock`, outside `stateDir`, which
//! `destroy --state` deletes.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;

use crate::error::{Context, Result};

#[derive(Debug)]
pub(crate) struct Lock {
    _file: File,
}

/// Takes the lock, calling `waiting` once if another hangar holds it.
pub(crate) fn acquire(path: &Path, waiting: impl FnOnce()) -> Result<Lock> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).context(dir.display())?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .context(path.display())?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            waiting();
            file.lock().context(path.display())?;
        }
        Err(TryLockError::Error(error)) => {
            return Err(error).context(path.display());
        }
    }
    Ok(Lock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::acquire;
    use crate::testing::scratch_dir;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn a_held_lock_waits_once_until_it_is_released() {
        let path = scratch_dir("lock-held").join("hangar/lock");
        let held = acquire(&path, || panic!("waited")).unwrap();
        let (waited, notice) = mpsc::channel();
        let second = {
            let path = path.clone();
            thread::spawn(move || {
                acquire(&path, move || waited.send(()).unwrap()).map(drop)
            })
        };
        notice.recv().unwrap();
        drop(held);
        second.join().unwrap().unwrap();
        assert!(notice.try_recv().is_err());
    }
}
