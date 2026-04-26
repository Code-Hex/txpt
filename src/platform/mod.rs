#![allow(unsafe_code)]

use std::io;
use std::path::Path;

#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

#[cfg(target_os = "linux")]
const FICLONE: libc::c_ulong = 0x4004_9409;

pub fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unix"
    }
}

pub fn clone_file(src: &Path, dst: &Path) -> io::Result<&'static str> {
    #[cfg(target_os = "macos")]
    {
        clonefile(src, dst)?;
        return Ok("apfs-clonefile");
    }
    #[cfg(target_os = "linux")]
    {
        ficlone(src, dst)?;
        return Ok("linux-ficlone");
    }
    #[allow(unreachable_code)]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clone engine is unavailable on this platform",
    ))
}

#[cfg(target_os = "macos")]
fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    unsafe extern "C" {
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32)
        -> libc::c_int;
    }
    // SAFETY: The C strings are NUL-terminated and live for the duration of the call.
    // The pointers are not retained by clonefile.
    let rc = unsafe { clonefile(src.as_ptr(), dst.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn ficlone(src: &Path, dst: &Path) -> io::Result<()> {
    let src = File::open(src)?;
    let dst = File::create(dst)?;
    // SAFETY: ioctl is called with valid file descriptors. FICLONE does not
    // retain pointers, and the third argument is the source file descriptor as
    // required by ioctl_ficlonerange(2).
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) };
    if rc == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        let _ = fs::remove_file(dst);
        Err(err)
    }
}
