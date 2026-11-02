//! The online template repository: browse and fetch DA/auth/preloader files.
//!
//! This is the CLI counterpart of the GUI's `src/templates/` module (the
//! Templates page). Same endpoint, same `meta.json` schema, and the same
//! SHA-256 verification of every downloaded file — only the transport
//! differs, since `ureq` is synchronous and this crate has no async runtime.
//!
//! Three entry points:
//!   * [`cmd_list`] / [`cmd_show`] / [`cmd_fetch`] — the `templates` subcommand
//!   * [`resolve`] — used by `--template <device>` on any device command

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::progress::{end_progress, progress};

/// Base URL of the repository. Mirrors `src/templates/mod.rs`.
pub const ENDPOINT: &str = "https://progzone122.github.io/umbrage-repo";

/// Requests must not hang forever; the GUI uses the same 20s budget.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Ceiling on any single response body. `ureq` caps un-configured reads at
/// 10 MiB; DA files are a few MiB at most, and this still leaves generous
/// headroom while making a broken or hostile server unable to stream without
/// bound.
const MAX_BODY: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// meta.json schema — field-for-field the same as src/templates/model.rs so
// that the two front ends keep accepting whatever the repository publishes.
// ---------------------------------------------------------------------------

/// Repository metadata (`meta.json`).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub vendors: BTreeMap<String, Vec<String>>,
    pub devices: BTreeMap<String, Device>,
}

/// One device, keyed by codename in [`Meta::devices`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Device {
    pub vendor: String,
    pub model: String,
    pub name: String,
    pub versions: Vec<Version>,
}

/// A flashing method. `id` is unique within a device.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Version {
    pub id: u32,
    #[serde(default)]
    pub default: bool,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub files: TemplateFile,
}

/// A version's files, keyed by file type (`da`, `auth`, `preloader`).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TemplateFile(pub BTreeMap<String, TemplateFileData>);

/// A file referenced by a version. `path` is relative to [`ENDPOINT`];
/// `sha256` is checked after download.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TemplateFileData {
    pub name: String,
    pub path: String,
    pub sha256: String,
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn agent() -> Result<ureq::Agent> {
    let config = ureq::config::Config::builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build();
    Ok(ureq::Agent::new_with_config(config))
}

/// GET `url` and return the body as bytes, reporting progress as it streams.
///
/// `ureq` treats non-2xx as an error by default (`http_status_as_error`), so
/// an HTTP failure surfaces as `http status: 404` rather than as parsed JSON.
fn get_bytes(url: &str, mut on_chunk: impl FnMut(usize, usize)) -> Result<Vec<u8>> {
    let mut response = agent()?
        .get(url)
        .call()
        .with_context(|| format!("request to {url} failed"))?;

    // Read content-length before borrowing the body for streaming.
    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    let mut reader = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .reader();

    let mut bytes = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let mut done = 0usize;
    loop {
        let read = reader
            .read(&mut buf)
            .with_context(|| format!("failed reading the response body from {url}"))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read]);
        done += read;
        on_chunk(done, total);
    }
    Ok(bytes)
}

fn get_text(url: &str) -> Result<String> {
    let bytes = get_bytes(url, |_, _| {})?;
    String::from_utf8(bytes).with_context(|| format!("{url} did not contain valid UTF-8"))
}

// ---------------------------------------------------------------------------
// Fetching metadata
// ---------------------------------------------------------------------------

/// Download and validate `meta.json`.
///
/// Parsing into [`Meta`] rather than a bare `serde_json::Value` catches a
/// missing field or wrong shape before it reaches any caller.
pub fn fetch_meta() -> Result<Meta> {
    let url = format!("{ENDPOINT}/meta.json");
    let body = get_text(&url)?;
    serde_json::from_str(&body).with_context(|| format!("invalid meta.json from {url}"))
}

/// Look a device up by codename.
///
/// Falls back to a case-insensitive match so `Alioth` finds `alioth`, but
/// errors when the name is ambiguous rather than guessing which device was
/// meant.
pub fn find_device<'a>(meta: &'a Meta, codename: &str) -> Result<&'a Device> {
    if let Some(device) = meta.devices.get(codename) {
        return Ok(device);
    }
    let wanted = codename.to_ascii_lowercase();
    let mut hits = meta
        .devices
        .iter()
        .filter(|(key, _)| key.to_ascii_lowercase() == wanted);
    match (hits.next(), hits.next()) {
        (Some((_, device)), None) => Ok(device),
        (Some(_), Some(_)) => bail!(
            "device name {codename:?} is ambiguous in the repository; \
             use the exact codename from `templates list`"
        ),
        (None, _) => bail!(
            "no device {codename:?} in the repository; run `templates list` to see what exists"
        ),
    }
}

/// Pick which version of a device to use.
///
/// Mirrors the GUI: the version flagged `default` is the recommended one, an
/// explicit id wins over it, and a device with no flagged default falls back
/// to its first version.
pub fn select_version<'a>(device: &'a Device, wanted: Option<u32>) -> Result<&'a Version> {
    if let Some(id) = wanted {
        return device
            .versions
            .iter()
            .find(|v| v.id == id)
            .with_context(|| {
                format!(
                    "device has no version with id {id}; \
                     run `templates show` to list the available ids"
                )
            });
    }
    device
        .versions
        .iter()
        .find(|v| v.default)
        .or_else(|| device.versions.first())
        .context("this device has no versions in the repository")
}

// ---------------------------------------------------------------------------
// Downloading files
// ---------------------------------------------------------------------------

/// Where a device's version files are cached between runs.
///
/// The codename comes from the repository, so it is sanitised before being
/// used as a path component: a key containing `/` or `..` must not be able to
/// write outside the cache directory.
fn cache_dir(device: &str, version: u32) -> PathBuf {
    let mut safe = String::with_capacity(device.len());
    for c in device.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
            safe.push(c);
        } else {
            safe.push('_');
        }
    }
    if safe.is_empty() || safe == "." || safe == ".." {
        safe = "_".into();
    }

    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => match std::env::var_os("HOME") {
            Some(h) if !h.is_empty() => PathBuf::from(h).join(".cache"),
            _ => std::env::temp_dir(),
        },
    };
    base.join("umbrage-cli").join("templates").join(safe).join(version.to_string())
}

/// True when `path` exists and its contents hash to `sha256`.
///
/// Verified on every reuse so a truncated or tampered cache entry is caught
/// rather than passed to the device.
fn cached_file_is_valid(path: &Path, sha256: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.is_empty() {
        return false;
    }
    let actual = Sha256::digest(&bytes);
    hex::encode(actual).eq_ignore_ascii_case(sha256)
}

/// Download one repository file to `dest`, verifying its SHA-256.
///
/// Writes to a sibling `.part` file and renames on success, so an interrupted
/// download never leaves a partial file where a later run would accept it.
fn download_file(file: &TemplateFileData, dest: &Path) -> Result<()> {
    let url = format!("{ENDPOINT}/{}", file.path);
    eprintln!("downloading {} ({url})", file.name);
    let bytes = get_bytes(&url, progress("download"))?;
    end_progress();

    let actual = hex::encode(Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(&file.sha256) {
        bail!(
            "SHA256 mismatch for {url}: expected {}, got {actual}",
            file.sha256
        );
    }

    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dest.with_file_name(format!(".{file_name}.part"));
    std::fs::write(&tmp, &bytes)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, dest).with_context(|| {
        format!("failed to move {} into place", tmp.display())
    })?;
    Ok(())
}

/// Ensure every file of `version` is present and verified under `dir`.
///
/// Existing files are re-hashed rather than trusted by name, so a changed
/// `meta.json` correctly forces a re-download.
fn ensure_files(
    version: &Version,
    dir: &Path,
    refresh: bool,
) -> Result<BTreeMap<String, PathBuf>> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;

    let mut resolved = BTreeMap::new();
    for (kind, file) in &version.files.0 {
        let dest = dir.join(&file.name);
        if !refresh && cached_file_is_valid(&dest, &file.sha256) {
            eprintln!("using cached {}", dest.display());
        } else {
            download_file(file, &dest)?;
        }
        resolved.insert(kind.clone(), dest);
    }
    Ok(resolved)
}

/// Resolve a template into local file paths, keyed by kind (`da`, `auth`,
/// `preloader`).
///
/// This is what `--template <device>` calls before connecting; the returned
/// paths fill in whichever of the three the user did not pass explicitly.
#[derive(Debug, Default)]
pub struct Resolved {
    pub files: BTreeMap<String, PathBuf>,
}

impl Resolved {
    pub fn get(&self, kind: &str) -> Option<&Path> {
        self.files.get(kind).map(|p| p.as_path())
    }
}

pub fn resolve(codename: &str, wanted: Option<u32>, refresh: bool) -> Result<Resolved> {
    let meta = fetch_meta()?;
    let device = find_device(&meta, codename)?;
    let version = select_version(device, wanted)?;

    let dir = cache_dir(codename, version.id);
    eprintln!(
        "template: {} (version {}{})",
        device.name,
        version.id,
        if version.default { ", recommended" } else { "" }
    );

    let files = ensure_files(version, &dir, refresh)?;

    Ok(Resolved { files })
}

// ---------------------------------------------------------------------------
// `templates` subcommands
// ---------------------------------------------------------------------------

pub fn cmd_list(json: bool) -> Result<()> {
    let meta = fetch_meta()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&meta)?);
        return Ok(());
    }

    if meta.devices.is_empty() {
        eprintln!("the repository is empty");
        return Ok(());
    }
    for (codename, device) in &meta.devices {
        let versions = device.versions.len();
        println!(
            "{:<20} {:<28} {versions} version{}",
            codename,
            format!("{} {}", device.vendor, device.model),
            if versions == 1 { "" } else { "s" }
        );
    }
    eprintln!("{} devices", meta.devices.len());
    Ok(())
}

pub fn cmd_show(codename: &str, wanted: Option<u32>, json: bool) -> Result<()> {
    let meta = fetch_meta()?;
    let device = find_device(&meta, codename)?;

    if json {
        println!("{}", serde_json::to_string_pretty(device)?);
        return Ok(());
    }

    println!("{} — {} {}", codename, device.vendor, device.model);
    if !device.name.is_empty() {
        println!("name: {}", device.name);
    }
    if device.versions.is_empty() {
        bail!("this device has no versions in the repository");
    }

    let selected = select_version(device, wanted)?;
    println!();
    for version in &device.versions {
        let marker = if version.id == selected.id { ">" } else { " " };
        let recommended = if version.default { "  [recommended]" } else { "" };
        println!("{marker} version {}{recommended}", version.id);
        if !version.name.is_empty() {
            println!("    {}", version.name);
        }
        if !version.description.is_empty() {
            println!("    {}", version.description);
        }
        for (kind, file) in &version.files.0 {
            println!("    {kind:<10} {}  sha256:{}", file.name, file.sha256);
        }
        println!();
    }
    Ok(())
}

pub fn cmd_fetch(
    codename: &str,
    wanted: Option<u32>,
    dest: Option<&Path>,
    refresh: bool,
    json: bool,
) -> Result<()> {
    let meta = fetch_meta()?;
    let device = find_device(&meta, codename)?;
    let version = select_version(device, wanted)?;

    let dir = match dest {
        Some(d) => d.to_path_buf(),
        None => cache_dir(codename, version.id),
    };

    let files = ensure_files(version, &dir, refresh)?;

    if json {
        let paths: BTreeMap<&str, String> = files
            .iter()
            .map(|(k, v)| (k.as_str(), v.to_string_lossy().into_owned()))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "device": codename,
                "version": version.id,
                "files": paths,
            }))?
        );
        return Ok(());
    }

    println!("device:  {} ({})", codename, device.name);
    println!("version: {}", version.id);
    for kind in ["da", "auth", "preloader"] {
        match files.get(kind) {
            Some(p) => println!("{kind:<9} {}", p.display()),
            None => println!("{kind:<9} (not provided by this version)"),
        }
    }
    Ok(())
}
