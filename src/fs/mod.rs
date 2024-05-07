//! Drop-in replacement of [std::fs] with no operations block scheduling thread.

use std::io::{Read, Result, Seek, SeekFrom, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::time::{Duration, SystemTime};

use ignore_result::Ignore;

use crate::io::{poller, Operation, Request};
use crate::task;

/// Same as [std::fs::File] except that it does not block scheduling thread in blocking operations.
#[derive(Debug)]
pub struct File {
    fd: OwnedFd,
}

impl FromRawFd for File {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd: OwnedFd::from_raw_fd(fd) }
    }
}

impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        let mut file = unsafe { ManuallyDrop::new(std::fs::File::from_raw_fd(self.fd.as_raw_fd())) };
        file.seek(pos)
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let len = buf.len().min(i32::MAX as usize);
        let operation = Operation::Read { fd: self.fd.as_raw_fd(), buf: buf.as_mut_ptr(), len, offset: -1 };
        poller::request(operation).map(|n| n as usize)
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let len = buf.len().min(i32::MAX as usize);
        let operation = Operation::Write { fd: self.fd.as_raw_fd(), buf: buf.as_ptr(), len, offset: -1 };
        poller::request(operation).map(|n| n as usize)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl File {
    /// Same as [std::fs::File::open] except that it does not block scheduling thread.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<File> {
        OpenOptions::new().read(true).open(path.as_ref())
    }

    /// Same as [std::fs::File::create] except that it does not block scheduling thread.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<File> {
        OpenOptions::new().write(true).create(true).truncate(true).open(path.as_ref())
    }

    /// Same as [std::fs::File::create_new] except that it does not block scheduling thread.
    pub fn create_new<P: AsRef<Path>>(path: P) -> Result<File> {
        OpenOptions::new().write(true).create_new(true).open(path.as_ref())
    }

    /// Same as [std::fs::File::options].
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    /// Same as [std::fs::File::sync_all] except that it does not block scheduling thread.
    pub fn sync_all(&self) -> Result<()> {
        self.sync(false)
    }

    /// Same as [std::fs::File::sync_data] except that it does not block scheduling thread.
    pub fn sync_data(&self) -> Result<()> {
        self.sync(true)
    }

    /// Same as [std::fs::File::set_len] except that it does not block scheduling thread.
    pub fn set_len(&self, size: u64) -> Result<()> {
        let operation = Operation::Truncate { fd: self.fd.as_raw_fd(), size };
        let requester = poller::requester();
        let (session, waker) = task::session();
        requester.send(Request { result: waker, operation });
        let result = session.wait_uninterruptibly(|_| {});
        result.map(|_| {})
    }

    /// Same as [std::fs::File::metadata] except that it does not block scheduling thread.
    pub fn metadata(&self) -> Result<Metadata> {
        let path = [0u8; 1];
        let mut metadata = unsafe { std::mem::zeroed() };
        let operation = Operation::Stat {
            fd: self.fd.as_raw_fd(),
            path: path.as_ptr() as *const libc::c_char,
            metadata: &mut metadata as *mut Metadata,
        };
        poller::request(operation)?;
        Ok(metadata)
    }

    fn sync(&self, only_data: bool) -> Result<()> {
        let operation = Operation::Fsync { fd: self.fd.as_raw_fd(), only_data };
        poller::request(operation)?;
        Ok(())
    }
}

/// Same as [std::fs::OpenOptions] except that [OpenOptions::open] does not block scheduling
/// thread.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    mode: u32,
}

impl OpenOptions {
    /// Same as [std::fs::OpenOptions::new].
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            mode: 0o666,
        }
    }

    /// Same as [std::fs::OpenOptions::read].
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    /// Same as [std::fs::OpenOptions::write].
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    /// Same as [std::fs::OpenOptions::append].
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    /// Same as [std::fs::OpenOptions::truncate].
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    /// Same as [std::fs::OpenOptions::create].
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    /// Same as [std::fs::OpenOptions::create_new].
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    fn flags(&self) -> i32 {
        let mut flags = match (self.read, self.write, self.append) {
            (true, true, false) => libc::O_RDWR,
            (true, false, false) => libc::O_RDONLY,
            (false, true, false) => libc::O_WRONLY,
            (true, _, true) => libc::O_RDWR | libc::O_APPEND,
            (false, _, true) => libc::O_WRONLY | libc::O_APPEND,
            _ => 0,
        };
        if self.truncate {
            flags |= libc::O_TRUNC;
        }
        if self.create {
            flags |= libc::O_CREAT;
        }
        if self.create_new {
            flags |= libc::O_CREAT | libc::O_EXCL;
        }
        flags
    }

    /// Same as [std::fs::OpenOptions::open] except that it does not block scheduling thread.
    pub fn open<P: AsRef<Path>>(&self, path: P) -> Result<File> {
        let path = path.as_ref();
        let bytes = path.as_os_str().as_encoded_bytes();
        let mut buffer = Vec::with_capacity(bytes.len() + 1);
        buffer.write(bytes).ignore();
        let null = [0u8; 1];
        buffer.write(&null[..]).ignore();
        let operation =
            Operation::Open { path: buffer.as_ptr() as *const libc::c_char, flags: self.flags(), mode: self.mode };
        let fd = poller::request(operation)?;
        Ok(unsafe { File::from_raw_fd(fd as RawFd) })
    }
}

/// Same as [std::fs::FileType].
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileType(libc::mode_t);

impl FileType {
    /// Same as [std::fs::FileType::is_dir].
    pub fn is_dir(&self) -> bool {
        (self.0 & libc::S_IFMT) == libc::S_IFDIR
    }

    /// Same as [std::fs::FileType::is_file].
    pub fn is_file(&self) -> bool {
        (self.0 & libc::S_IFMT) == libc::S_IFREG
    }

    /// Same as [std::fs::FileType::is_symlink].
    pub fn is_symlink(&self) -> bool {
        (self.0 & libc::S_IFMT) == libc::S_IFLNK
    }
}

/// Same as [std::fs::Metadata].
#[derive(Clone)]
pub struct Metadata {
    #[cfg(target_os = "linux")]
    pub(crate) stat: libc::statx,
    #[cfg(not(target_os = "linux"))]
    pub(crate) stat: libc::stat,
}

impl Metadata {
    /// Same as [std::fs::Metadata::file_type].
    #[cfg(not(target_os = "linux"))]
    pub fn file_type(&self) -> FileType {
        FileType(self.stat.st_mode)
    }

    /// Same as [std::fs::Metadata::file_type].
    #[cfg(target_os = "linux")]
    pub fn file_type(&self) -> FileType {
        FileType(self.stat.stx_mode as libc::mode_t)
    }

    /// Same as [std::fs::Metadata::is_dir].
    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    /// Same as [std::fs::Metadata::is_file].
    pub fn is_file(&self) -> bool {
        self.file_type().is_file()
    }

    /// Same as [std::fs::Metadata::is_symlink].
    pub fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }

    /// Same as [std::fs::Metadata::len].
    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.stat.st_size as u64
    }

    /// Same as [std::fs::Metadata::modified].
    #[cfg(not(target_os = "linux"))]
    pub fn modified(&self) -> Result<SystemTime> {
        let d = Duration::new(self.stat.st_mtime as u64, self.stat.st_mtime_nsec as u32);
        Ok(SystemTime::UNIX_EPOCH + d)
    }

    /// Same as [std::fs::Metadata::accessed].
    #[cfg(not(target_os = "linux"))]
    pub fn accessed(&self) -> Result<SystemTime> {
        let d = Duration::new(self.stat.st_atime as u64, self.stat.st_atime_nsec as u32);
        Ok(SystemTime::UNIX_EPOCH + d)
    }

    /// Same as [std::fs::Metadata::created].
    #[cfg(not(target_os = "linux"))]
    pub fn created(&self) -> Result<SystemTime> {
        let d = Duration::new(self.stat.st_birthtime as u64, self.stat.st_birthtime_nsec as u32);
        Ok(SystemTime::UNIX_EPOCH + d)
    }

    /// Same as [std::fs::Metadata::len].
    #[cfg(target_os = "linux")]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.stat.stx_size
    }

    /// Same as [std::fs::Metadata::modified].
    #[cfg(target_os = "linux")]
    pub fn modified(&self) -> Result<SystemTime> {
        let d = Duration::new(self.stat.stx_mtime.tv_sec as u64, self.stat.stx_mtime.tv_nsec);
        Ok(SystemTime::UNIX_EPOCH + d)
    }

    /// Same as [std::fs::Metadata::accessed].
    #[cfg(target_os = "linux")]
    pub fn accessed(&self) -> Result<SystemTime> {
        let d = Duration::new(self.stat.stx_mtime.tv_sec as u64, self.stat.stx_mtime.tv_nsec);
        Ok(SystemTime::UNIX_EPOCH + d)
    }

    /// Same as [std::fs::Metadata::created].
    #[cfg(target_os = "linux")]
    pub fn created(&self) -> Result<SystemTime> {
        let d = Duration::new(self.stat.stx_btime.tv_sec as u64, self.stat.stx_btime.tv_nsec);
        Ok(SystemTime::UNIX_EPOCH + d)
    }
}

#[cfg(test)]
mod tests {
    use std::io::prelude::*;
    use std::io::ErrorKind;
    use std::path::PathBuf;

    use googletest::prelude::*;

    use super::File;
    use crate::coroutine;

    // Truncate file with content "file1\n".
    fn assert_create_file(path: PathBuf) {
        let mut file = File::create(&path).unwrap();
        file.write(b"file1\n").unwrap();
        file.sync_all().unwrap();
        let mut buf = [0u8; 6];
        let mut file = File::open(&path).unwrap();
        file.read_exact(&mut buf[..]).unwrap();
        assert_eq!(&buf[..], b"file1\n");
    }

    // Create file with content "file2\n".
    fn assert_create_new_file(path: PathBuf) {
        let mut file = File::create_new(&path).unwrap();
        file.write(b"file2\n").unwrap();
        file.sync_data().unwrap();
        let mut buf = [0u8; 6];
        let mut file = File::open(&path).unwrap();
        file.read_exact(&mut buf[..]).unwrap();
        assert_eq!(&buf[..], b"file2\n");
    }

    #[crate::test(crate = "crate")]
    fn cancel() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = File::create_new(dir.path().join("file1")).unwrap();
        // given: ongoing file write
        coroutine::spawn(move || {
            file.write(b"file1").unwrap();
        });

        // give io a chance to suspension
        coroutine::yield_now();

        // when: task exit
        // then: all good
    }

    #[crate::test(crate = "crate")]
    fn multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let path1 = dir.path().join("file1");
        let path2 = dir.path().join("file2");

        // when: create two files concurrent
        let coroutine1 = coroutine::spawn(move || {
            assert_create_file(path1);
        });
        let coroutine2 = coroutine::spawn(move || {
            assert_create_new_file(path2);
        });

        // then: succeed
        coroutine1.join().unwrap();
        coroutine2.join().unwrap();

        // given: existing file
        // then: File::create_new should fail
        let err = File::create_new(dir.path().join("file1")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);

        // given: existing file
        // then: File::create should truncate it
        let mut file1 = File::create(dir.path().join("file1")).unwrap();
        assert_that!(file1.metadata().unwrap().len(), eq(0));

        // given: file without read enabled
        // then: read should fail
        let mut buf = [0u8; 1];
        let err = file1.read(&mut buf[..]).unwrap_err();
        assert_that!(err.to_string(), contains_substring("Bad file descriptor"));
    }

    #[crate::test(crate = "crate")]
    fn metadata() {
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        let path1 = dir.path().join("file1");
        let now = SystemTime::now();
        assert_create_file(path1);

        // given: file with content "file1\n".
        let file1 = File::open(dir.path().join("file1")).unwrap();

        // then: check its metadata
        let metadata = file1.metadata().unwrap();
        assert_eq!(metadata.len(), 6);
        assert_eq!(metadata.is_file(), true);
        assert_that!(metadata.modified().unwrap(), ge(now.checked_sub(Duration::from_secs(10)).unwrap()));
    }

    #[crate::test(crate = "crate")]
    fn set_len() {
        use std::io::Write;

        // given file with content "file1\n".
        let dir = tempfile::tempdir().unwrap();
        let path1 = dir.path().join("file1");
        assert_create_file(path1);
        let mut file1 = File::options().read(true).write(true).open(dir.path().join("file1")).unwrap();

        // when: truncate length to 128
        file1.set_len(128).unwrap();

        // then: file content should be "file1\n" plus zeros.
        let mut buf = [0u8; 129];
        fastrand::shuffle(&mut buf[..]);
        let err = file1.read_exact(&mut buf[..]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        let mut expected = [0u8; 129];
        (&mut expected[..]).write(b"file1\n").unwrap();
        assert_eq!(buf, expected);

        // give: above
        // when: truncate length to 4
        file1.set_len(4).unwrap();

        // then: file content should be "file".
        let mut buf = [0u8; 5];
        let mut file1 = File::open(dir.path().join("file1")).unwrap();
        let err = file1.read_exact(&mut buf[..]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        assert_eq!(&buf[..4], b"file");
    }
}
