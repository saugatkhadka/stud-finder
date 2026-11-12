//! Where the daemon for a repo root lives.
//!
//! macOS caps `sun_path` at 104 bytes and repo paths can be long, so the
//! socket is never placed inside the repo. Instead every root maps to
//! `<dir>/<hash>.{sock,lock,log}` where `<hash>` is FNV-1a 64 of the
//! canonical root path and `<dir>` is, in order of preference:
//! `$RIPPLE_SOCKET_DIR`, `$XDG_RUNTIME_DIR/ripple`, `/tmp/ripple-$UID`.
//! The directory is created `0700` and must be owned by the current user.

use anyhow::{Context, Result, bail};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Longest socket path we accept (macOS `sun_path` is 104 bytes incl. NUL).
const MAX_SOCKET_PATH: usize = 100;

#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub dir: PathBuf,
    pub socket: PathBuf,
    /// Held with an exclusive `flock` for the daemon's lifetime; contains its pid.
    pub lock: PathBuf,
    pub log: PathBuf,
}

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn candidate_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(d) = std::env::var_os("RIPPLE_SOCKET_DIR").filter(|d| !d.is_empty()) {
        out.push(PathBuf::from(d));
        return out;
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        out.push(PathBuf::from(d).join("ripple"));
    }
    out.push(PathBuf::from(format!("/tmp/ripple-{}", uid())));
    out
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let meta = std::fs::metadata(dir)?;
    if meta.uid() != uid() {
        bail!("{} is owned by another user; refusing to use it for the daemon socket", dir.display());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

impl DaemonPaths {
    /// Paths for `root`, which must already be canonical. Does not create anything.
    pub fn for_root(root: &Path) -> Result<Self> {
        let hash = format!("{:016x}", fnv1a64(root.as_os_str().as_encoded_bytes()));
        let dirs = candidate_dirs();
        let dir = dirs
            .iter()
            .find(|d| d.join(format!("{hash}.sock")).as_os_str().len() <= MAX_SOCKET_PATH)
            .or(dirs.last())
            .cloned()
            .context("no socket directory")?;
        let socket = dir.join(format!("{hash}.sock"));
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            bail!("socket path {} is too long for a Unix socket; set RIPPLE_SOCKET_DIR to a short directory", socket.display());
        }
        Ok(Self { lock: dir.join(format!("{hash}.lock")), log: dir.join(format!("{hash}.log")), socket, dir })
    }

    pub fn create_dir(&self) -> Result<()> {
        ensure_private_dir(&self.dir)
    }

    /// Pid recorded in the lock file, if any.
    pub fn recorded_pid(&self) -> Option<i32> {
        std::fs::read_to_string(&self.lock).ok()?.trim().parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_stable() {
        let long = PathBuf::from(format!("/Users/someone/{}", "very long directory name/".repeat(10)));
        let a = DaemonPaths::for_root(&long).unwrap();
        let b = DaemonPaths::for_root(&long).unwrap();
        assert_eq!(a.socket, b.socket);
        assert!(a.socket.as_os_str().len() <= MAX_SOCKET_PATH);
        assert_ne!(a.socket, DaemonPaths::for_root(Path::new("/other")).unwrap().socket);
    }
}
