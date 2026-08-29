use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::path::Path;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Clone, Copy)]
pub struct DirectoryIdentity {
    #[cfg(unix)]
    pub(crate) device: u64,
    #[cfg(unix)]
    pub(crate) inode: u64,
    #[cfg(unix)]
    pub(crate) owner: u32,
}

pub struct CleanupDirectory {
    path: PathBuf,
    identity: DirectoryIdentity,
    remove_on_drop: bool,
}

impl CleanupDirectory {
    pub fn claim(path: &Path) -> Result<Self> {
        anyhow::ensure!(
            path.is_absolute(),
            "target cleanup directory must be absolute"
        );
        anyhow::ensure!(
            path.to_str().is_some(),
            "target cleanup directory must be valid Unicode"
        );
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspect target cleanup directory {}", path.display()))?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink() && metadata.is_dir(),
            "target cleanup path must identify an existing directory"
        );
        #[cfg(unix)]
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "target cleanup directory must be owned by the runner user"
        );
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolve target cleanup directory {}", path.display()))?;
        anyhow::ensure!(
            canonical.to_str().is_some(),
            "canonical target cleanup directory must be valid Unicode"
        );
        Ok(Self {
            path: canonical,
            identity: identity(&metadata),
            remove_on_drop: true,
        })
    }

    pub fn adopt(path: PathBuf, expected: DirectoryIdentity) -> Result<Self> {
        let directory = Self::claim(&path)?;
        if !same_identity(directory.identity, expected) {
            directory.preserve();
            bail!("target cleanup directory changed before ownership transfer")
        }
        Ok(directory)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn identity(&self) -> DirectoryIdentity {
        self.identity
    }

    pub fn remove(mut self) -> Result<()> {
        self.remove_on_drop = false;
        self.remove_owned()
    }

    pub fn preserve(mut self) {
        self.remove_on_drop = false;
    }

    fn remove_owned(&self) -> Result<()> {
        let metadata = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect target cleanup directory {}", self.path.display())
                });
            }
        };
        anyhow::ensure!(
            !metadata.file_type().is_symlink()
                && metadata.is_dir()
                && same_identity(identity(&metadata), self.identity),
            "target cleanup directory identity changed before removal"
        );
        #[cfg(unix)]
        make_directory_tree_removable(&self.path)?;
        std::fs::remove_dir_all(&self.path)
            .with_context(|| format!("remove target cleanup directory {}", self.path.display()))
    }
}

impl Drop for CleanupDirectory {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = self.remove_owned();
        }
    }
}

fn identity(metadata: &std::fs::Metadata) -> DirectoryIdentity {
    DirectoryIdentity {
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        owner: metadata.uid(),
    }
}

fn same_identity(left: DirectoryIdentity, right: DirectoryIdentity) -> bool {
    #[cfg(unix)]
    {
        left.device == right.device && left.inode == right.inode && left.owner == right.owner
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        true
    }
}

#[cfg(unix)]
fn make_directory_tree_removable(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect target cleanup path {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }

    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("repair target cleanup permissions for {}", path.display()))?;

    for entry in std::fs::read_dir(path)
        .with_context(|| format!("read target cleanup directory {}", path.display()))?
    {
        let entry =
            entry.with_context(|| format!("read target cleanup entry under {}", path.display()))?;
        make_directory_tree_removable(&entry.path())?;
    }
    Ok(())
}
