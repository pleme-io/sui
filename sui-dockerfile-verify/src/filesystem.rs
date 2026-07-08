//! Final-filesystem comparison via `docker save` + tar inspection.
//!
//! `docker save` writes an OCI/Docker-format tarball: a `manifest.json`
//! naming the ordered list of per-layer `layer.tar` archives, plus the
//! layer tars themselves (each an AUFS-style whiteout-capable tar). This
//! module builds the *union filesystem* those layers describe — the
//! same semantics the container runtime applies at run time — as a
//! typed `path -> content hash` map, so two images can be compared file
//! by file without ever starting a container.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use serde::Deserialize;
use sui_dockerfile_wrapper::command::{CommandRunner, DockerBuildInvocation};

use crate::VerifyError;

/// One `docker save` manifest entry — only the `Layers` ordering
/// matters here (the config/RepoTags fields are ignored).
#[derive(Debug, Deserialize)]
struct ManifestEntry {
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// Content state of one final-filesystem path, keyed by the merged
/// union across every layer in manifest order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub content_hash: String,
}

/// The whole union filesystem a `docker save` tarball describes,
/// path -> its final [`FileState`] (deleted paths — an AUFS
/// `.wh.<name>` whiteout in a later layer — are absent, not present
/// with a tombstone value).
pub type UnionFilesystem = BTreeMap<String, FileState>;

/// Run `docker save -o <output_path> <image_ref>` via the shared
/// [`CommandRunner`] seam.
///
/// # Errors
///
/// Returns [`VerifyError::Command`] on spawn failure or
/// [`VerifyError::CommandFailed`] on a non-zero exit.
pub fn save_image<R: CommandRunner>(runner: &R, image_ref: &str, output_path: &Path) -> Result<(), VerifyError> {
    let invocation = DockerBuildInvocation::save(image_ref, output_path);
    let outcome = runner.run(&invocation).map_err(VerifyError::Command)?;
    if !outcome.success {
        return Err(VerifyError::CommandFailed {
            image: image_ref.to_string(),
            stderr_tail: outcome.stderr_tail(4096),
        });
    }
    Ok(())
}

/// Parse a `docker save` tarball on disk into its [`UnionFilesystem`].
///
/// # Errors
///
/// Returns [`VerifyError::Tar`] on any I/O/tar-format failure, or
/// [`VerifyError::ManifestMissing`] if the tarball has no
/// `manifest.json` (not a valid `docker save` output).
pub fn union_filesystem(tar_path: &Path) -> Result<UnionFilesystem, VerifyError> {
    let layer_paths = read_manifest_layers(tar_path)?;
    let layer_bytes = extract_named_entries(tar_path, &layer_paths)?;

    let mut fs: UnionFilesystem = BTreeMap::new();
    for layer_path in &layer_paths {
        let bytes = layer_bytes
            .get(layer_path)
            .ok_or_else(|| VerifyError::ManifestMissing { layer_path: layer_path.clone() })?;
        apply_layer(&mut fs, bytes)?;
    }
    Ok(fs)
}

/// Apply one layer's tar bytes onto the accumulated union filesystem —
/// whiteout entries (`.wh.<name>`) remove the shadowed path; every
/// other regular-file entry overwrites it with the new content hash.
fn apply_layer(fs: &mut UnionFilesystem, layer_tar_bytes: &[u8]) -> Result<(), VerifyError> {
    let mut archive = tar::Archive::new(Cursor::new(layer_tar_bytes));
    for entry in archive.entries().map_err(VerifyError::Tar)? {
        let mut entry = entry.map_err(VerifyError::Tar)?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let path = entry.path().map_err(VerifyError::Tar)?.to_string_lossy().into_owned();
        let (dir, file_name) = split_dir_and_name(&path);

        if let Some(deleted_name) = file_name.strip_prefix(".wh.") {
            let deleted_path = join_dir_and_name(dir, deleted_name);
            fs.remove(&deleted_path);
            continue;
        }

        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(VerifyError::Tar)?;
        let content_hash = blake3::hash(&buf).to_hex().to_string();
        fs.insert(path, FileState { content_hash });
    }
    Ok(())
}

fn split_dir_and_name(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn join_dir_and_name(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        let mut joined = String::with_capacity(dir.len() + 1 + name.len());
        joined.push_str(dir);
        joined.push('/');
        joined.push_str(name);
        joined
    }
}

fn read_manifest_layers(tar_path: &Path) -> Result<Vec<String>, VerifyError> {
    let file = File::open(tar_path).map_err(VerifyError::Tar)?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries().map_err(VerifyError::Tar)? {
        let mut entry = entry.map_err(VerifyError::Tar)?;
        let path = entry.path().map_err(VerifyError::Tar)?.to_string_lossy().into_owned();
        if path == "manifest.json" {
            let mut buf = String::new();
            entry.read_to_string(&mut buf).map_err(VerifyError::Tar)?;
            let manifest: Vec<ManifestEntry> = serde_json::from_str(&buf)?;
            return Ok(manifest.into_iter().next().map(|m| m.layers).unwrap_or_default());
        }
    }
    Err(VerifyError::ManifestMissing { layer_path: "manifest.json".to_string() })
}

/// Extract the raw bytes of every top-level entry in `tar_path` whose
/// name is in `wanted` — used to pull out the nested per-layer tars
/// named by `manifest.json`.
fn extract_named_entries(tar_path: &Path, wanted: &[String]) -> Result<BTreeMap<String, Vec<u8>>, VerifyError> {
    let file = File::open(tar_path).map_err(VerifyError::Tar)?;
    let mut archive = tar::Archive::new(file);
    let mut out = BTreeMap::new();
    for entry in archive.entries().map_err(VerifyError::Tar)? {
        let mut entry = entry.map_err(VerifyError::Tar)?;
        let path = entry.path().map_err(VerifyError::Tar)?.to_string_lossy().into_owned();
        if wanted.contains(&path) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(VerifyError::Tar)?;
            out.insert(path, buf);
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    //! Synthetic `docker save`-shaped tarballs — the documented
    //! fallback for exercising [`super::union_filesystem`] without a
    //! real docker daemon: a hand-constructed outer tar containing a
    //! `manifest.json` plus one or more nested layer tars, mirroring
    //! exactly what `docker save` itself writes (proven against a real
    //! daemon separately in `tests/real_docker_equivalence.rs`, gated
    //! `#[ignore]`).

    use std::io::Write;

    /// Build one nested layer tar's raw bytes from `(path, content)`
    /// regular-file entries, plus optional `.wh.<name>` whiteout
    /// entries (pass the deleted name with the `.wh.` prefix already
    /// applied as the "path").
    pub fn build_layer_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, *path, *content).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// Build a full `docker save`-shaped outer tarball: `manifest.json`
    /// (naming the layers in order) + the layer tars themselves under
    /// `<n>/layer.tar`.
    pub fn build_save_tar(layers: &[Vec<u8>]) -> Vec<u8> {
        let layer_names: Vec<String> = (0..layers.len()).map(|i| format!("{i}/layer.tar")).collect();
        let manifest = serde_json::json!([{ "Config": "config.json", "RepoTags": [], "Layers": layer_names }]);
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();

        let mut builder = tar::Builder::new(Vec::new());
        for (name, layer_bytes) in layer_names.iter().zip(layers.iter()) {
            let mut header = tar::Header::new_gnu();
            header.set_size(layer_bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, Cursor2(layer_bytes)).unwrap();
        }
        let mut manifest_header = tar::Header::new_gnu();
        manifest_header.set_size(manifest_bytes.len() as u64);
        manifest_header.set_mode(0o644);
        manifest_header.set_cksum();
        builder.append_data(&mut manifest_header, "manifest.json", manifest_bytes.as_slice()).unwrap();

        builder.into_inner().unwrap()
    }

    // A tiny `Read` wrapper so `append_data` accepts `&Vec<u8>` content
    // without an extra clone.
    struct Cursor2<'a>(&'a [u8]);
    impl std::io::Read for Cursor2<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[allow(dead_code)]
    pub fn write_to(path: &std::path::Path, bytes: &[u8]) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::{build_layer_tar, build_save_tar, write_to};
    use super::*;

    #[test]
    fn union_filesystem_merges_layers_in_manifest_order() {
        let layer0 = build_layer_tar(&[("etc/os-release", b"debian" as &[u8]), ("opt/app/bin", b"v1")]);
        let layer1 = build_layer_tar(&[("opt/app/bin", b"v2")]);
        let tar_bytes = build_save_tar(&[layer0, layer1]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.tar");
        write_to(&path, &tar_bytes);

        let fs = union_filesystem(&path).unwrap();

        assert!(fs.contains_key("etc/os-release"));
        let bin_hash = fs.get("opt/app/bin").unwrap().content_hash.clone();
        assert_eq!(bin_hash, blake3::hash(b"v2").to_hex().to_string(), "later layer must win");
    }

    #[test]
    fn whiteout_entry_removes_the_shadowed_path() {
        let layer0 = build_layer_tar(&[("opt/app/stale", b"gone soon" as &[u8])]);
        let layer1 = build_layer_tar(&[("opt/app/.wh.stale", b"" as &[u8])]);
        let tar_bytes = build_save_tar(&[layer0, layer1]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.tar");
        write_to(&path, &tar_bytes);

        let fs = union_filesystem(&path).unwrap();

        assert!(!fs.contains_key("opt/app/stale"), "whiteout must remove the path from the union");
    }
}
