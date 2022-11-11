use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Context;

use crate::{
    config::{CacheConfig, FirecrackerBin, Kernel},
    utils::DeleteOnDrop,
};

/// Get the path to the firecracker binary, potentially downloading it if needed.
pub(crate) fn get_firecracker_path(
    firecracker: &FirecrackerBin,
    cache: &CacheConfig,
) -> anyhow::Result<PathBuf> {
    get_path_to_cached_binary(
        cache,
        "firecracker",
        firecracker.path.as_deref(),
        firecracker.url.as_deref(),
        firecracker.sha256.as_deref(),
    )
}

pub(crate) fn get_kernel_path(kernel: &Kernel, cache: &CacheConfig) -> anyhow::Result<PathBuf> {
    get_path_to_cached_binary(
        cache,
        "vmlinux",
        kernel.path.as_deref(),
        kernel.url.as_deref(),
        kernel.sha256.as_deref(),
    )
}

fn get_path_to_cached_binary(
    cache: &CacheConfig,
    name: &str,
    path: Option<&Path>,
    url: Option<&str>,
    sha256: Option<&str>,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = path {
        if path.exists() {
            tracing::debug!("{name} found at: {}", path.display());
            return Ok(path.into());
        }
        anyhow::bail!("{} does not exist", path.display());
    }

    if let Some(url) = url {
        // First check whether the binary already exists in the cache.
        let target_path = cache.dir.join(name);
        if target_path.exists() {
            tracing::debug!("{name} found in cache: {}", target_path.display());
            return Ok(target_path);
        }

        // Otherwise try to download and extract it from the provided url.
        return download_and_extract(cache, url, name, sha256)
            .with_context(|| format!("failed to download {name} from: {url}"));
    }

    anyhow::bail!("{name} path not configured")
}

fn download_and_extract(
    cache: &CacheConfig,
    url: &str,
    name: &str,
    sha256: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let extension = match url.rsplit_once("/") {
        Some((_, name)) => {
            std::path::Path::new(name).extension().and_then(|x| x.to_str()).unwrap_or("")
        }
        None => "",
    };

    let tmp_file_path = std::env::current_dir()
        .with_context(|| format!("unable to get working directory"))?
        .join(".harness-download.tmp");
    let _cleanup = DeleteOnDrop(Some(tmp_file_path.clone()));

    let writer = {
        let file = std::fs::File::create(&tmp_file_path)
            .with_context(|| format!("error creating \"{}\"", tmp_file_path.display()))?;
        auto_decompress(file, extension)?
    };

    tracing::info!("Downloading {name} from {url}");
    download_url(url, writer)?;

    let target_path = cache.dir.join(name);
    if extension.contains("tar") || extension.contains("tgz") {
        extract_from(&tmp_file_path, &target_path, |path| {
            path.to_str().map_or(false, |x| x.contains(name))
        })
        .with_context(|| format!("error extracting {} from archive", name))?;
        let _ = std::fs::remove_file(tmp_file_path);
    }
    else {
        std::fs::rename(&tmp_file_path, &target_path)
            .with_context(|| format!("error moving binary to \"{}\"", target_path.display()))?;
    }

    if let Some(expected_sha256) = sha256 {
        let sha256 =
            sha256_for_path(&target_path).with_context(|| format!("error computing digest"))?;
        if expected_sha256 != sha256 {
            let _ = std::fs::rename(&target_path, target_path.with_extension("bad"));
            anyhow::bail!("SHA256 mismatch: {sha256} != {expected_sha256}");
        }
    }

    Ok(target_path)
}

pub fn hex(bytes: &[u8]) -> String {
    const LOOKUP_4BITS: &[u8] = b"0123456789abcdef";

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(LOOKUP_4BITS[((byte >> 4) & 0xF) as usize] as char);
        out.push(LOOKUP_4BITS[(byte & 0xF) as usize] as char);
    }
    out
}

fn sha256_for_path(p: &Path) -> anyhow::Result<String> {
    use sha2::Digest;
    use std::io::Read;

    let mut file =
        std::fs::File::open(p).with_context(|| format!("failed to open: {}", p.display()))?;

    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0; 1024];
    loop {
        match file.read(&mut buf).with_context(|| format!("error reading from: {}", p.display()))? {
            0 => break,
            n => hasher.update(&buf[..n]),
        }
    }

    Ok(hex(&hasher.finalize()[..]))
}

/// Downloads `url` writing the contents to `writer`.
fn download_url<W>(url: &str, mut writer: W) -> anyhow::Result<()>
where
    W: Write,
{
    let mut client = curl::easy::Easy::new();
    client.follow_location(true)?;
    client.url(url)?;

    let mut error = None;

    let result = {
        let mut transfer = client.transfer();
        transfer.write_function(|buf| match writer.write_all(buf) {
            Ok(_) => Ok(buf.len()),
            Err(e) => {
                error = Some(e);
                Ok(0)
            }
        })?;
        transfer.perform()
    };

    match error.take() {
        Some(inner) => result.context(inner),
        _ => Ok(result?),
    }
}

/// Wraps a writer with a decompression decoder based on the file extension.
fn auto_decompress<W>(writer: W, extension: &str) -> anyhow::Result<Box<dyn Write>>
where
    W: Write + 'static,
{
    if extension.ends_with("gz") || extension.ends_with("gzip") {
        return Ok(Box::new(flate2::write::GzDecoder::new(writer)));
    }
    Ok(Box::new(writer))
}

/// Extracts a file that matches `match` from a tar archive located at `archive` and copies it to
/// `dst`.
fn extract_from(
    archive: &Path,
    dst: &Path,
    mut matches: impl FnMut(&Path) -> bool,
) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(std::fs::File::open(archive)?);
    for entry in archive.entries_with_seek().context("error reading downloaded archive")? {
        let mut entry = entry.context("corrupted archive")?;
        let path = match entry.path() {
            Ok(path) => path,
            Err(_) => continue,
        };

        if matches(path.as_ref()) {
            entry.unpack(dst)?;
            return Ok(());
        }
    }

    anyhow::bail!("target not found in the archive")
}
