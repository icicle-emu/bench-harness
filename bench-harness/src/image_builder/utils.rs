use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::{utils::DeleteOnDrop, XShellExt};

const BLOCK_SIZE: u64 = 512;

/// Aligns the size to the next block boundary
pub(crate) fn align_to_block_size(size: u64) -> u64 {
    (size + BLOCK_SIZE - 1) & !(BLOCK_SIZE - 1)
}

pub(crate) struct ZeroFile(usize);

impl ZeroFile {
    fn from_bytes(bytes: usize) -> Self {
        Self(bytes)
    }
}

impl std::io::Read for ZeroFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = buf.len().min(self.0);
        self.0 -= len;
        buf[..len].fill(0);
        Ok(len)
    }
}

pub(crate) struct MountHandle {
    pub path: Option<PathBuf>,
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

impl MountHandle {
    pub fn unmount(&mut self) -> anyhow::Result<()> {
        if let Some(path) = self.path.take() {
            if path.exists() {
                let sh = xshell::Shell::new()?;
                xshell::cmd!(sh, "umount {path}").read_with_err()?;
            }
        }
        Ok(())
    }

    pub fn copy_from(&self, from: &Path, prefix: &Path) -> anyhow::Result<()> {
        if let Some(to) = &self.path {
            let to = to.join(prefix);
            let sh = xshell::Shell::new()?;
            xshell::cmd!(sh, "mkdir -p {to}").read_with_err()?;
            xshell::cmd!(sh, "cp -a {from} {to}").read_with_err()?;
        }
        Ok(())
    }
}

/// Mount the file system stored in `file` at `mount_path`
pub(crate) fn mount_file_system(
    file: &Path,
    mount_path: &Path,
) -> Result<MountHandle, anyhow::Error> {
    std::fs::create_dir_all(mount_path)
        .with_context(|| format!("failed to create mount point: {}", mount_path.display()))?;
    let sh = xshell::Shell::new()?;
    xshell::cmd!(sh, "mount {file} {mount_path}").read_with_err()?;
    Ok(MountHandle { path: Some(mount_path.to_owned()) })
}

#[must_use]
pub(crate) fn init_fs(path: &Path, size: u64) -> anyhow::Result<DeleteOnDrop> {
    // Create an empty file initialized filled `size` bytes of 0x00
    let mut file = std::fs::File::create(path)
        .with_context(|| format!("failed to create: {}", path.display()))?;
    std::io::copy(&mut ZeroFile::from_bytes(size as usize), &mut file)?;

    // Initialize the file system
    let sh = xshell::Shell::new()?;
    xshell::cmd!(sh, "mkfs.ext4 -F -q -E lazy_itable_init=1 {path}").run()?;
    Ok(DeleteOnDrop(Some(path.into())))
}
