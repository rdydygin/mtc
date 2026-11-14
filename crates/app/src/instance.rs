//! A profile has one writer. Keep the locked file open until the process exits.
//! Never unlink the file: waiters must continue to lock the same inode.

use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

pub fn acquire(directory: &Path) -> io::Result<Option<File>> {
    std::fs::create_dir_all(directory)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(directory.join("profile.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(err)) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_contend_and_release_without_removing_the_file() {
        let directory =
            std::env::temp_dir().join(format!("mtc-profile-lock-{}", std::process::id()));
        let first = acquire(&directory).unwrap().unwrap();
        assert!(acquire(&directory).unwrap().is_none());
        drop(first);
        let second = acquire(&directory).unwrap().unwrap();
        assert!(acquire(&directory).unwrap().is_none());
        drop(second);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
