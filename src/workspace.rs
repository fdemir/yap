use std::{
    ffi::OsString,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
#[cfg(unix)]
use cap_std::fs::MetadataExt;
use cap_std::{
    ambient_authority,
    fs::{Dir, Metadata, OpenOptions},
};

use crate::tool::ToolError;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Workspace {
    dir: Dir,
}

pub(crate) struct BoundedRead {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

pub(crate) struct WorkspaceEntry {
    pub name: String,
    pub is_dir: bool,
}

impl Workspace {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = std::fs::canonicalize(root.into())?;
        let dir = Dir::open_ambient_dir(&root, ambient_authority())?;
        Ok(Self { dir })
    }

    pub fn exists(&self, path: &Path) -> bool {
        self.resolve_existing(path).is_ok()
    }

    pub fn read_bounded(&self, path: &Path, max_bytes: usize) -> Result<BoundedRead, ToolError> {
        let path = self.resolve_existing(path)?;
        let mut file = open_file_nofollow(&self.dir, &path)?;
        if !file.metadata().map_err(map_io_error)?.is_file() {
            return Err(ToolError::Execution("path is not a regular file".into()));
        }
        let limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(limit)
            .read_to_end(&mut bytes)
            .map_err(map_io_error)?;
        let truncated = bytes.len() > max_bytes;
        bytes.truncate(max_bytes);
        Ok(BoundedRead { bytes, truncated })
    }

    pub fn read_dir_bounded(
        &self,
        path: &Path,
        max_entries: usize,
    ) -> Result<(Vec<WorkspaceEntry>, bool), ToolError> {
        let path = self.resolve_existing(path)?;
        let directory = self.dir.open_dir_nofollow(path).map_err(map_io_error)?;
        let entries = directory.entries().map_err(map_io_error)?;
        let mut output = Vec::with_capacity(max_entries.min(256));
        let mut truncated = false;
        for entry in entries {
            if output.len() == max_entries {
                truncated = true;
                break;
            }
            let entry = entry.map_err(map_io_error)?;
            output.push(WorkspaceEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: entry.file_type().map_err(map_io_error)?.is_dir(),
            });
        }
        Ok((output, truncated))
    }

    pub fn replace_atomic(
        &self,
        path: &Path,
        expected: &[u8],
        updated: &[u8],
    ) -> Result<(), ToolError> {
        let path = self.resolve_existing(path)?;
        let parent_path = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let file_name = path
            .file_name()
            .ok_or_else(|| ToolError::Execution("patch target has no file name".into()))?;
        let parent = self
            .dir
            .open_dir_nofollow(parent_path)
            .map_err(map_io_error)?;
        let metadata = parent.symlink_metadata(file_name).map_err(map_io_error)?;
        if !metadata.is_file() {
            return Err(ToolError::ConcurrentModification);
        }

        let (temp_name, mut temp) = create_temp_file(&parent, file_name)?;
        let result = (|| {
            temp.write_all(updated).map_err(map_io_error)?;
            temp.set_permissions(metadata.permissions())
                .map_err(map_io_error)?;
            temp.sync_all().map_err(map_io_error)?;

            let (current, current_metadata) =
                read_all(&parent, file_name, expected.len().saturating_add(1))?;
            if current != expected || !same_file(&metadata, &current_metadata) {
                return Err(ToolError::ConcurrentModification);
            }

            parent
                .rename(&temp_name, &parent, file_name)
                .map_err(map_io_error)?;
            sync_directory(&parent)
        })();
        if result.is_err() {
            let _ = parent.remove_file(&temp_name);
        }
        result
    }

    fn resolve_existing(&self, path: &Path) -> Result<PathBuf, ToolError> {
        validate_relative(path)?;
        self.dir.canonicalize(path).map_err(map_io_error)
    }
}

fn validate_relative(path: &Path) -> Result<(), ToolError> {
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ToolError::OutsideWorkspace);
    }
    Ok(())
}

fn open_file_nofollow(directory: &Dir, path: &Path) -> Result<cap_std::fs::File, ToolError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    directory.open_with(path, &options).map_err(map_io_error)
}

fn create_temp_file(
    parent: &Dir,
    target_name: &std::ffi::OsStr,
) -> Result<(OsString, cap_std::fs::File), ToolError> {
    for _ in 0..16 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!(
            ".{}.yap-{}-{sequence}.tmp",
            target_name.to_string_lossy(),
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match parent.open_with(&name, &options) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(map_io_error(error)),
        }
    }
    Err(ToolError::Execution(
        "could not allocate an atomic patch file".into(),
    ))
}

fn read_all(
    parent: &Dir,
    path: &std::ffi::OsStr,
    limit: usize,
) -> Result<(Vec<u8>, Metadata), ToolError> {
    let mut file = open_file_nofollow(parent, Path::new(path))?;
    let metadata = file.metadata().map_err(map_io_error)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(map_io_error)?;
    Ok((bytes, metadata))
}

#[cfg(unix)]
fn same_file(expected: &Metadata, current: &Metadata) -> bool {
    expected.dev() == current.dev() && expected.ino() == current.ino()
}

#[cfg(not(unix))]
fn same_file(_expected: &Metadata, _current: &Metadata) -> bool {
    true
}

#[cfg(unix)]
fn sync_directory(directory: &Dir) -> Result<(), ToolError> {
    directory
        .try_clone()
        .map_err(map_io_error)?
        .into_std_file()
        .sync_all()
        .map_err(map_io_error)
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Dir) -> Result<(), ToolError> {
    Ok(())
}

fn map_io_error(error: io::Error) -> ToolError {
    if matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
    ) {
        ToolError::OutsideWorkspace
    } else {
        ToolError::Execution(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_replace_detects_a_concurrent_change_and_cleans_up() {
        let root = tempfile::tempdir().expect("workspace should be created");
        std::fs::write(root.path().join("file.txt"), "changed").expect("fixture should be created");
        let workspace = Workspace::open(root.path()).expect("workspace should open");

        let error = workspace
            .replace_atomic(Path::new("file.txt"), b"old", b"new")
            .expect_err("stale replacement should be rejected");

        assert_eq!(error, ToolError::ConcurrentModification);
        assert_eq!(
            std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
            "changed"
        );
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".yap-")
        }));
    }
}
