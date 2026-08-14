//! Copy-on-write tree cloning.
//!
//! Fast path on macOS is `clonefile(2)`, which clones an entire directory
//! hierarchy in-kernel as CoW references — no data is copied. On Linux the
//! fast path is the `FICLONE` ioctl per regular file (btrfs/xfs/bcachefs).
//! When a fast path fails for an entry we fall back to walking it and cloning
//! item by item; entries that can't be represented (sockets, fifos, devices)
//! are skipped and reported.

use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct CloneStats {
    /// Top-level entries cloned.
    pub entries: u64,
    /// Paths skipped because they can't be cloned or copied (sockets, fifos…).
    pub skipped: Vec<PathBuf>,
    /// Paths where the CoW fast path failed and we degraded to a plain copy.
    pub copied: u64,
}

/// Clone the children of `src` into `dst` (created if missing). Names in
/// `exclude` are skipped at the top level only.
pub fn clone_tree(src: &Path, dst: &Path, exclude: &[&OsStr]) -> Result<CloneStats> {
    fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    let mut stats = CloneStats::default();
    for entry in fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if exclude.iter().any(|x| name == **x) {
            continue;
        }
        clone_entry(&entry.path(), &dst.join(&name), &mut stats)?;
        stats.entries += 1;
    }
    Ok(stats)
}

/// Clone a single filesystem entry (file, dir, or symlink) to `dst`.
pub fn clone_entry(src: &Path, dst: &Path, stats: &mut CloneStats) -> Result<()> {
    match reflink(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => fallback_walk(src, dst, stats),
    }
}

/// Item-by-item clone used when the in-kernel fast path fails (cross-device,
/// unsupported filesystem, or an uncloneable item somewhere in the tree).
fn fallback_walk(src: &Path, dst: &Path, stats: &mut CloneStats) -> Result<()> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    let ft = meta.file_type();
    if ft.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            clone_entry(&entry.path(), &dst.join(entry.file_name()), stats)?;
        }
        let _ = fs::set_permissions(dst, meta.permissions());
        Ok(())
    } else if ft.is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(&target, dst)?;
        Ok(())
    } else if ft.is_file() {
        // reflink already failed for this path; degrade to a real copy but
        // preserve mtime so tree diffs stay meaningful.
        fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
        if let Ok(modified) = meta.modified() {
            if let Ok(f) = fs::File::options().write(true).open(dst) {
                let _ = f.set_modified(modified);
            }
        }
        stats.copied += 1;
        Ok(())
    } else {
        // Sockets, fifos, devices: not part of workspace state we can snapshot.
        stats.skipped.push(src.to_path_buf());
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn cstr(p: &Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(target_os = "macos")]
fn reflink(src: &Path, dst: &Path) -> io::Result<()> {
    // CLONE_NOFOLLOW: clone a symlink itself rather than its target.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    extern "C" {
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32)
            -> libc::c_int;
    }
    let (s, d) = (cstr(src)?, cstr(dst)?);
    let rc = unsafe { clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn reflink(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    const FICLONE: libc::c_ulong = 0x40049409;
    let meta = fs::symlink_metadata(src)?;
    if !meta.is_file() {
        // Directories and symlinks take the fallback walk on Linux.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "not a regular file",
        ));
    }
    let s = fs::File::open(src)?;
    let d = fs::File::options().write(true).create_new(true).open(dst)?;
    let rc = unsafe { libc::ioctl(d.as_raw_fd(), FICLONE, s.as_raw_fd()) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        drop(d);
        let _ = fs::remove_file(dst);
        return Err(err);
    }
    let _ = fs::set_permissions(dst, meta.permissions());
    if let Ok(modified) = meta.modified() {
        let _ = d.set_modified(modified);
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn reflink(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no reflink support on this platform",
    ))
}
