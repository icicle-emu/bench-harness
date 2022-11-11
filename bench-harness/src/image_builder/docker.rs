//! Handle interactions with docker

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::Context;

use crate::{utils::DeleteOnDrop, XShellExt};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DockerSource {
    /// The name of the docker image used for creating the root file system.
    pub tag: String,

    /// The directory containing the docker context used to build `image`.
    pub build_path: PathBuf,

    /// Paths to copy from the container to the file system.
    #[serde(default)]
    pub copy: Vec<PathBuf>,

    /// Empty folders to create in the file system.
    #[serde(default)]
    pub create_dirs: Vec<PathBuf>,
}

pub(crate) fn build_image(config: &DockerSource, no_cache: bool) -> anyhow::Result<()> {
    let tag = &config.tag;
    let root = &config.build_path;
    let no_cache = no_cache.then(|| "--no-cache");
    let sh = xshell::Shell::new()?;
    xshell::cmd!(sh, "docker build -t {tag} {root} {no_cache...}").trace_cmd().run()?;
    Ok(())
}

/// Get the size of a docker image
pub(crate) fn get_image_size(config: &DockerSource) -> anyhow::Result<u64> {
    let tag = &config.tag;
    let sh = xshell::Shell::new()?;
    let output = xshell::cmd!(sh, "docker image inspect {tag} --format='{{.Size}}'").output()?;

    if !output.status.success() {
        anyhow::bail!(
            "error inspecting size of docker image: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let size = String::from_utf8(output.stdout)
        .map_err(anyhow::Error::msg)
        .and_then(|x| x.trim().parse::<u64>().map_err(anyhow::Error::msg))
        .context("error parsing image size")?;

    Ok(size)
}

/// Get the time the docker image was created at.
pub(crate) fn get_creation_time(config: &DockerSource) -> anyhow::Result<SystemTime> {
    let sh = xshell::Shell::new()?;
    let tag = &config.tag;
    let output = xshell::cmd!(sh, "docker image inspect {tag} --format='{{.Created}}'").output()?;

    if !output.status.success() {
        anyhow::bail!(
            "error inspecting creation date of docker image: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let time = String::from_utf8(output.stdout)
        .map_err(anyhow::Error::msg)
        .and_then(|x| {
            time::OffsetDateTime::parse(&x.trim(), &time::format_description::well_known::Rfc3339)
                .map_err(anyhow::Error::msg)
        })
        .context("error parsing image creation date")?;

    Ok(time.into())
}

struct CopyState<'a> {
    config: &'a DockerSource,
    container: Option<String>,
    root: &'a Path,
}

impl<'a> CopyState<'a> {
    fn cleanup(&mut self) -> anyhow::Result<()> {
        if let Some(name) = self.container.take() {
            remove_container(&name)?;
        }
        Ok(())
    }
}

impl<'a> Drop for CopyState<'a> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Copy the contents of a docker container to a target directory.
pub(crate) fn copy_image(config: &DockerSource, dst_root: &Path) -> anyhow::Result<()> {
    let mut state = CopyState { config, container: None, root: dst_root };

    // Create a new docker container and initialize it with the approprate state.
    state.container = Some(create_container(&state.config.tag)?);
    let container = state.container.as_ref().unwrap();

    // Copy files from the container into the mounted folder
    copy_files(&container, &state.config.copy, &state.root)?;

    // Create any mounted folders
    let sh = xshell::Shell::new()?;
    for dir in &state.config.create_dirs {
        let path = state.root.join(dir);
        xshell::cmd!(sh, "mkdir {path}").trace_cmd().run()?;
    }

    state.cleanup()?;

    Ok(())
}

fn create_container(image: &str) -> anyhow::Result<String> {
    let sh = xshell::Shell::new()?;
    let result = xshell::cmd!(sh, "docker create {image}")
        .read_with_err()
        .context("failed to create container")?;
    Ok(result.trim().to_owned())
}

fn remove_container(name: &str) -> anyhow::Result<()> {
    let sh = xshell::Shell::new()?;
    xshell::cmd!(sh, "docker rm {name}").run().context("failed to remove container")?;
    Ok(())
}

fn copy_files(name: &str, files: &[impl AsRef<Path>], to: &Path) -> anyhow::Result<()> {
    let tmp_path = std::env::temp_dir().join("bench-harness-docker-extract");
    let handle = DeleteOnDrop(Some(tmp_path.clone()));

    for file in files {
        let file = file.as_ref();
        // @fixme: docker cp seems to fail sometimes when directly copying it to the target folder,
        // instead we pipe the output to a file and use tar to perform the extraction.
        let tmp_file = std::fs::File::create(&tmp_path)
            .context("failed to create temporary file for copying")?;
        let output = std::process::Command::new("docker")
            .arg("cp")
            .arg(format!("{}:/{}", name, file.display()))
            .arg("-")
            .stdout(tmp_file)
            .output()
            .context("error running docker cp")?;

        if !output.status.success() {
            anyhow::bail!("error running docker cp: {}", String::from_utf8_lossy(&output.stderr));
        }

        // Succeeded creating tar file, now extract it.
        let mut archive = tar::Archive::new(std::fs::File::open(&tmp_path)?);
        archive.set_preserve_permissions(true);
        archive.unpack(to).context("error unpacking archive")?;
    }

    drop(handle);
    Ok(())
}
