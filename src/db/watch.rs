use anyhow::{bail, Context, Result};
use std::{
    ffi::CString,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
};

pub(super) struct DatabaseWatcher {
    descriptor: OwnedFd,
    watch: i32,
}

impl DatabaseWatcher {
    pub(super) fn new(directory: &Path) -> Result<Self> {
        let path = CString::new(directory.as_os_str().as_bytes())?;
        let descriptor = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error()).context("inotify 初始化失败");
        }
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let mask = libc::IN_MODIFY
            | libc::IN_CLOSE_WRITE
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_ATTRIB
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_ONLYDIR;
        let watch = unsafe { libc::inotify_add_watch(descriptor.as_raw_fd(), path.as_ptr(), mask) };
        if watch < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("监听数据库目录失败: {}", directory.display()));
        }
        Ok(Self { descriptor, watch })
    }

    pub(super) fn wait_for_changes(&self) -> Result<bool> {
        let mut buffer = [0u8; 16384];
        let length = loop {
            let length = unsafe {
                libc::read(
                    self.descriptor.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if length >= 0 {
                break length as usize;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("读取数据库文件事件失败");
            }
        };
        anyhow::ensure!(length > 0, "数据库文件事件监听已关闭");
        let header_size = std::mem::size_of::<libc::inotify_event>();
        let mut position = 0;
        let mut changed = false;
        while position < length {
            anyhow::ensure!(length - position >= header_size, "数据库文件事件不完整");
            let event = unsafe {
                std::ptr::read_unaligned(
                    buffer.as_ptr().add(position).cast::<libc::inotify_event>(),
                )
            };
            let event_size = header_size + event.len as usize;
            anyhow::ensure!(event_size <= length - position, "数据库文件事件名称不完整");
            if event.mask & libc::IN_Q_OVERFLOW != 0 {
                changed = true;
            }
            if event.wd == self.watch {
                if event.mask
                    & (libc::IN_DELETE_SELF
                        | libc::IN_MOVE_SELF
                        | libc::IN_UNMOUNT
                        | libc::IN_IGNORED)
                    != 0
                {
                    bail!("数据库目录已移动或移除，需要重新监听");
                }
                if event.mask & libc::IN_ISDIR == 0 {
                    changed = true;
                }
            }
            position += event_size;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::OpenOptions, path::PathBuf, process::Command};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let mut random = [0u8; 8];
            getrandom::fill(&mut random).unwrap();
            let directory = std::env::temp_dir().join(format!(
                "mimicwx-database-watch-{}",
                u64::from_le_bytes(random)
            ));
            std::fs::create_dir(&directory).unwrap();
            Self(directory)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn wait(watcher: &DatabaseWatcher) -> Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: watcher.descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 2000) }, 1);
        watcher.wait_for_changes()
    }

    fn exclusive_lock_available(path: &Path) -> bool {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut lock = libc::flock {
            l_type: libc::F_WRLCK as _,
            l_whence: libc::SEEK_SET as _,
            l_start: 128,
            l_len: 1,
            l_pid: 0,
        };
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            unsafe {
                let descriptor = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
                if descriptor < 0 {
                    libc::_exit(2);
                }
                let status = libc::fcntl(descriptor, libc::F_SETLK, &mut lock);
                libc::_exit(if status == 0 { 0 } else { 1 });
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        let code = libc::WEXITSTATUS(status);
        assert!(code <= 1);
        code == 0
    }

    #[test]
    fn external_database_notifications_preserve_posix_locks() {
        let fixture = Fixture::new();
        let path = fixture.0.join("message_0.db-shm");
        std::fs::write(&path, [0u8; 32768]).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut lock = libc::flock {
            l_type: libc::F_RDLCK as _,
            l_whence: libc::SEEK_SET as _,
            l_start: 128,
            l_len: 1,
            l_pid: 0,
        };
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &mut lock) },
            0
        );
        assert!(!exclusive_lock_available(&path));
        let watcher = DatabaseWatcher::new(&fixture.0).unwrap();
        assert!(Command::new("/bin/sh")
            .args(["-c", "printf changed >> \"$1\"", "database-watch-test"])
            .arg(&path)
            .status()
            .unwrap()
            .success());
        assert!(wait(&watcher).unwrap());
        assert!(!exclusive_lock_available(&path));
        drop(file);
        assert!(exclusive_lock_available(&path));
    }

    #[test]
    fn observes_wal_creation_replacement_and_removal() {
        let fixture = Fixture::new();
        let watcher = DatabaseWatcher::new(&fixture.0).unwrap();
        let path = fixture.0.join("message_0.db-wal");
        std::fs::write(&path, b"first").unwrap();
        assert!(wait(&watcher).unwrap());
        let replacement = fixture.0.join("replacement");
        std::fs::write(&replacement, b"second").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert!(wait(&watcher).unwrap());
        std::fs::remove_file(path).unwrap();
        assert!(wait(&watcher).unwrap());
    }

    #[test]
    fn detects_directory_replacement_for_watch_recovery() {
        let fixture = Fixture::new();
        let watcher = DatabaseWatcher::new(&fixture.0).unwrap();
        std::fs::remove_dir(&fixture.0).unwrap();
        assert!(wait(&watcher).is_err());
        std::fs::create_dir(&fixture.0).unwrap();
        let replacement = DatabaseWatcher::new(&fixture.0).unwrap();
        std::fs::write(fixture.0.join("message_0.db-wal"), b"new").unwrap();
        assert!(wait(&replacement).unwrap());
    }
}
