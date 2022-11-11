//! Utilities for generating the initial root filesystem for the VM
mod docker;
mod utils;

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::Context;

use crate::{
    config::CacheConfig,
    image_builder::{docker::DockerSource, utils::MountHandle},
};

#[derive(serde::Deserialize)]
pub(crate) struct ImageSource {
    #[serde(flatten)]
    pub kind: SourceKind,
    pub size: Option<u64>,
}

impl ImageSource {
    pub fn get_size(&self, measured_size: u64) -> anyhow::Result<u64> {
        let base_size = match self.size {
            Some(size) if size < measured_size => anyhow::bail!(
                "target size ({size} bytes) too small (required {measured_size} bytes)."
            ),
            Some(size) => size,
            None => measured_size + 1000,
        };
        Ok(utils::align_to_block_size(base_size))
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct HostSource {
    pub paths: Vec<PathToCopy>,
}

#[derive(serde::Deserialize)]
pub struct PathToCopy {
    dst: PathBuf,
    src: PathBuf,
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SourceKind {
    Docker(DockerSource),
    Host(HostSource),
}

impl SourceKind {
    fn build(&self, cache: &CacheConfig) -> anyhow::Result<()> {
        match self {
            SourceKind::Docker(inner) => docker::build_image(inner, cache.disable_image_cache),
            SourceKind::Host(_) => Ok(()),
        }
    }

    fn get_total_size_and_modified_time(&self) -> anyhow::Result<(u64, SystemTime)> {
        match self {
            SourceKind::Docker(inner) => {
                let size = docker::get_image_size(inner)?;
                let time = docker::get_creation_time(inner)?;
                Ok((size, time))
            }
            SourceKind::Host(inner) => {
                let mut newest_modified_time = std::time::UNIX_EPOCH;
                let mut total_size = 0;
                for entry in &inner.paths {
                    let (size, time) = get_total_size_and_modified_time(&entry.src)?;
                    total_size += size;
                    newest_modified_time = newest_modified_time.max(time);
                }
                Ok((total_size, newest_modified_time))
            }
        }
    }

    fn copy(&self, mount: &MountHandle) -> anyhow::Result<()> {
        match self {
            SourceKind::Docker(inner) => docker::copy_image(inner, &mount.path.as_ref().unwrap()),
            SourceKind::Host(inner) => {
                for entry in &inner.paths {
                    mount.copy_from(&entry.src, &entry.dst).with_context(|| {
                        format!("error copying {} to {}", entry.src.display(), entry.dst.display())
                    })?;
                }
                Ok(())
            }
        }
    }
}

/// Get the path to a cached disk image
pub(crate) fn get_image_path(name: &str, cache: &CacheConfig) -> anyhow::Result<PathBuf> {
    let path = cache.dir.join(format!("{name}.ext4"));
    // Check that the path exists at this point -- it still could be deleted before it is used, but
    // checking it handles the more common case where the image has yet to be created allowing us to
    // produce a better error message.
    if let Err(e) = path.metadata() {
        anyhow::bail!(
            "failed to find image for \"{name}\": {e}\n\n(you may need to run `{} build` first!)",
            env!("CARGO_BIN_NAME"),
        );
    }

    Ok(path)
}

/// Build a disk image from a source.
pub(crate) fn build_image(
    name: &str,
    source: &ImageSource,
    cache: &CacheConfig,
) -> anyhow::Result<PathBuf> {
    let path = cache.dir.join(format!("{name}.ext4"));

    let mut image_time = None;
    let mut existing_size = 0;
    if let Ok(metadata) = path.metadata() {
        if cache.skip_validation {
            tracing::debug!("Existing image found for {name} skipping validation");
            return Ok(path);
        }
        if !cache.disable_image_cache {
            image_time = metadata.modified().ok();
        }
        existing_size = metadata.len();
    }

    source.kind.build(cache)?;

    // Checks whether we need to rebuild the image based on modification time and changes to the
    // image size.
    let (measured_size, source_time) =
        source.kind.get_total_size_and_modified_time().context("error computing metadata")?;
    let size = source.get_size(measured_size)?;

    let source_is_newer = image_time.map_or(true, |time| time < source_time);
    if !source_is_newer && existing_size == size {
        tracing::info!("Cached image for {name} is up to date, skiping image creation");
        return Ok(path);
    }

    tracing::info!(
        "{name}: source ({size} bytes) modified at {}, `{}` ({existing_size} bytes) modified at {}",
        DisplayOptionalDateTime(Some(source_time)),
        path.display(),
        DisplayOptionalDateTime(image_time)
    );
    tracing::info!("Rebuilding {name} at `{}`", path.display());
    let disk = utils::init_fs(&path, size).context("failed to initialize file system")?;

    let mount_path = std::env::temp_dir().join(format!("bench-harness-image_builder-{name}"));
    let mount = utils::mount_file_system(&path, &mount_path)?;

    source.kind.copy(&mount)?;

    disk.finalize();

    Ok(path)
}

struct DisplayOptionalDateTime(Option<std::time::SystemTime>);

impl std::fmt::Display for DisplayOptionalDateTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let time = match self.0 {
            Some(time) => time,
            None => return f.write_str("<never>"),
        };
        match time::OffsetDateTime::from(time)
            .format(&time::format_description::well_known::Rfc3339)
        {
            Ok(string) => f.write_str(&string),
            Err(_) => write!(f, "{:?}", time),
        }
    }
}

/// Computes the total size and the date of the newest file in the given directory.
fn get_total_size_and_modified_time(path: &Path) -> anyhow::Result<(u64, SystemTime)> {
    let mut total_size = 0;

    let mut newest_modified_time = std::time::UNIX_EPOCH;
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry?;

        let metadata = entry.metadata()?;
        total_size += get_on_disk_size(&metadata);
        newest_modified_time = newest_modified_time.max(metadata.modified()?);
    }

    Ok((total_size, newest_modified_time))
}

#[cfg(unix)]
fn get_on_disk_size(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::prelude::MetadataExt;
    metadata.blksize() * metadata.blocks()
}

#[cfg(not(unix))]
fn get_on_disk_size(metadata: &std::fs::Metadata) -> u64 {
    utils::align_to_block_size(metadata.len())
}
