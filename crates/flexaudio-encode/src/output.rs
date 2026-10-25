//! Explicit output ownership: every native close is attempted at most once.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};

pub(crate) trait Output:
    Write + Seek + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
{
    fn close(&mut self) -> io::Result<()>;
}

pub(crate) struct FileOutput {
    file: Option<File>,
}

impl FileOutput {
    pub(crate) fn new(file: File) -> Self {
        Self { file: Some(file) }
    }

    fn file(&mut self) -> io::Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "FLAC output is closed"))
    }
}

impl Write for FileOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file()?.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file()?.flush()
    }
}

impl Seek for FileOutput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file()?.seek(position)
    }
}

impl Output for FileOutput {
    fn close(&mut self) -> io::Result<()> {
        match self.file.take() {
            // Consume ownership before close: retrying an interrupted/failed close
            // could close an unrelated descriptor already reused by another thread.
            Some(file) => close_file(file),
            None => Ok(()),
        }
    }
}

#[cfg(unix)]
fn close_file(file: File) -> io::Result<()> {
    use std::os::fd::IntoRawFd;
    let descriptor = file.into_raw_fd();
    // SAFETY: Ownership was transferred from File; this is its sole close attempt.
    let result = unsafe { libc::close(descriptor) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn close_file(file: File) -> io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    let handle = file.into_raw_handle();
    // SAFETY: Ownership was transferred from File; this is its sole close attempt.
    let result = unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("checked file close requires a Unix or Windows target");

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn native_file_closes_once_and_rejects_later_writes() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "flexaudio-checked-close-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mut output = FileOutput::new(file);
        output.write_all(b"checked output").unwrap();
        output.close().unwrap();
        assert!(output.file.is_none());
        output.close().unwrap();
        assert_eq!(
            output.write(b"late").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"checked output");
        std::fs::remove_file(path).unwrap();
    }
}
