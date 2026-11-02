//! One function per subcommand. Each connects to the device, performs a
//! single operation, and returns.
//!
//! stdout carries data, stderr carries progress and logs, so that
//! `umbrage-cli read -p boot -o - > boot.img` yields a clean image.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use penumbra_mtk::da::extensions::{KeyDeriveId, KeySize, SejKeyId, SejParams};
use penumbra_mtk::hacc::{BootPartition, IntoBytes, LockState, OFFSET_SLOT_SUFFIX};
use penumbra_mtk::port::PortType;
use penumbra_mtk::{BootMode, Device, RpmbRegion, Storage};

use crate::connect::{self, ConnectOpts};
use crate::numbers::{human_bytes, parse_u32, parse_u64, parse_usize};
use crate::progress::{end_progress, progress};

type PenumbraResult<T> = std::result::Result<T, penumbra_mtk::Error>;

/// Open a destination for writing; `-` means stdout.
///
/// Returns `None` for stdout so callers can skip the trailing summary that
/// would otherwise corrupt a piped image.
fn open_output(path: &str) -> Result<Option<Box<dyn Write + Send>>> {
    if path == "-" {
        return Ok(None);
    }
    let file = File::create(path)
        .with_context(|| format!("failed to create {path}"))?;
    Ok(Some(Box::new(BufWriter::new(file))))
}

/// Open a source for reading; `-` means stdin.
fn open_input(path: &str) -> Result<Box<dyn Read + Send>> {
    if path == "-" {
        return Ok(Box::new(BufReader::new(std::io::stdin())));
    }
    let file =
        File::open(path).with_context(|| format!("failed to open {path}"))?;
    Ok(Box::new(BufReader::new(file)))
}

/// Read a whole input into memory. Needed where the length must be known
/// before the transfer starts (`write_partition`, `poke`, SEJ AES).
fn read_all(path: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    open_input(path)?.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Resolve a scatter-relative image path against the scatter's directory.
fn resolve_image(scatter_dir: &Path, referenced: &str) -> PathBuf {
    let p = Path::new(referenced);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        scatter_dir.join(p)
    }
}

/// Pull the platform string (e.g. "MT6768") out of raw scatter content.
///
/// Returns `None` when the field is absent or empty. Ported from the GUI's
/// `src/callbacks/scatter.rs` so both front ends read the same field the same
/// way: XML between `<platform>`/`</platform>`, YAML from a `platform:` line.
fn extract_platform(content: &str, is_xml: bool) -> Option<String> {
    if is_xml {
        let start = content.find("<platform>")? + "<platform>".len();
        let rest = &content[start..];
        let end = rest.find("</platform>")?;
        let value = rest[..end].trim();
        return (!value.is_empty()).then(|| value.to_string());
    }

    for line in content.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("platform:") {
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_string());
        }
    }

    None
}

/// Refuse a scatter file built for a different chipset than the one connected.
///
/// The GUI enforces the same rule when a scatter is loaded; without it a
/// layout meant for another SoC would be flashed verbatim, which can brick the
/// device. A missing platform on either side is not an error — plenty of
/// scatter files omit it — only a *mismatch* is refused.
fn check_scatter_platform(scatter_platform: Option<&str>, chip_platform: Option<&str>) -> Result<()> {
    let (Some(scatter), Some(chip)) = (scatter_platform, chip_platform) else {
        return Ok(());
    };

    if scatter.eq_ignore_ascii_case(chip) {
        return Ok(());
    }

    bail!(
        "scatter file platform ({scatter}) does not match the connected device ({chip}); \
         refusing to flash a layout built for another chipset"
    );
}

// ---------------------------------------------------------------- info

pub fn info(opts: &ConnectOpts) -> Result<()> {
    let mut handle = connect::connect(opts)?;
    let dev = &mut handle.device;

    let hw_code = dev.devinfo().hw_code();
    let chip = dev.devinfo().chip();
    let chip_name = chip.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
    let chip_debug = chip.map(|c| format!("{c:?}"));

    println!("chip:          {chip_name}");
    println!("hw code:       0x{hw_code:04x}");
    if let Some(platform) = chip_debug {
        println!("platform:      {platform}");
    }
    println!(
        "security:      sbc={} sla={} daa={}",
        dev.devinfo().sbc_enabled(),
        dev.devinfo().sla_enabled(),
        dev.devinfo().daa_enabled()
    );
    println!("soc id:        {}", hex::encode(dev.devinfo().soc_id()));
    println!("meid:          {}", hex::encode(dev.devinfo().meid()));

    // Everything below needs the DA uploaded. Report what we can even when
    // the upload fails, so `info` stays useful for debugging auth problems.
    if let Err(err) = connect::enter_da(&mut handle) {
        eprintln!();
        eprintln!("note: DA mode unavailable, storage details omitted: {err:#}");
        return Ok(());
    }

    let dev = &mut handle.device;
    if let Some(storage) = dev.get_storage() {
        println!("storage:       {}", storage.as_str());
    }
    match dev.get_bootctrl() {
        Ok(bootctrl) => {
            println!("active slot:   {:?}", bootctrl.get_active_slot());
        }
        Err(err) => println!("active slot:   unavailable ({err})"),
    }
    let count = dev.partitions().len();
    println!("partitions:    {count}");
    Ok(())
}

// ------------------------------------------------------------ partitions

pub fn partitions(opts: &ConnectOpts, json: bool) -> Result<()> {
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    let parts = handle.device.partitions();

    if json {
        let rows: Vec<serde_json::Value> = parts
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "size": p.size,
                    "size_human": human_bytes(p.size as u64),
                    "address": format!("0x{:x}", p.address),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!("{:<24} {:>12} {:>18}", "PARTITION", "SIZE", "ADDRESS");
    for p in &parts {
        println!(
            "{:<24} {:>12} {:>18}",
            p.name,
            human_bytes(p.size as u64),
            format!("0x{:x}", p.address),
        );
    }
    eprintln!("{} partitions", parts.len());
    Ok(())
}

// ------------------------------------------------------- read / write / erase

pub fn read_partition(opts: &ConnectOpts, partition: &str, output: &str) -> Result<()> {
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;

    let dev: &mut Device<'static, PortType> = &mut handle.device;
    let size = dev
        .get_partition(partition)
        .map(|p| p.size)
        .unwrap_or(0);

    let mut writer: Box<dyn Write + Send> = match open_output(output)? {
        Some(w) => w,
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    eprintln!(
        "reading partition {partition:?}{}...",
        if size > 0 {
            format!(" ({})", human_bytes(size as u64))
        } else {
            String::new()
        }
    );
    dev.read_partition(partition, &mut writer, progress("read"))
        .with_context(|| format!("failed to read partition {partition:?}"))?;
    writer.flush().context("failed to flush output")?;
    end_progress();

    if output != "-" {
        eprintln!("wrote {output}");
    }
    Ok(())
}

pub fn write_partition(opts: &ConnectOpts, partition: &str, image: &str) -> Result<()> {
    let data = read_all(image)?;
    if data.is_empty() {
        bail!("{image} is empty");
    }

    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;

    eprintln!("writing {} to partition {partition:?}", human_bytes(data.len() as u64));
    handle
        .device
        .write_partition(partition, data.len(), &data[..], progress("write"))
        .with_context(|| format!("failed to write partition {partition:?}"))?;
    end_progress();
    eprintln!("done");
    Ok(())
}

pub fn erase_partition(opts: &ConnectOpts, partition: &str) -> Result<()> {
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;

    eprintln!("erasing partition {partition:?}...");
    handle
        .device
        .erase_partition(partition, progress("erase"))
        .with_context(|| format!("failed to erase partition {partition:?}"))?;
    end_progress();
    eprintln!("done");
    Ok(())
}

// ----------------------------------------------------------- flash scatter

pub fn flash_scatter(opts: &ConnectOpts, scatter: &str, out_dir: Option<&str>) -> Result<()> {
    let content = std::fs::read_to_string(scatter)
        .with_context(|| format!("failed to read scatter {scatter}"))?;

    // penumbra picks XML vs YAML per DA protocol; we only need the directory
    // so that the relative image paths inside the scatter resolve.
    let scatter_dir = Path::new(scatter)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let sink_dir = match out_dir {
        Some(d) => PathBuf::from(d),
        None => scatter_dir.clone(),
    };
    std::fs::create_dir_all(&sink_dir)
        .with_context(|| format!("failed to create {}", sink_dir.display()))?;

    let mut handle = connect::connect(opts)?;

    // Safety check before entering DA mode or writing a single byte: refuse a
    // scatter built for another chipset, exactly as the GUI does when a
    // scatter is loaded. The platform comes from the scatter text itself and
    // is compared against the connected chip, which is already known after
    // the handshake.
    let is_xml = scatter.to_ascii_lowercase().ends_with(".xml");
    check_scatter_platform(
        extract_platform(&content, is_xml).as_deref(),
        handle
            .device
            .devinfo()
            .chip()
            .map(|c| format!("{c:?}"))
            .as_deref(),
    )?;

    connect::enter_da(&mut handle)?;

    eprintln!("flashing scatter {scatter}...");
    handle
        .device
        .flash_scatter(
            &content,
            move |name: &str| -> PenumbraResult<(BufReader<File>, usize)> {
                let path = resolve_image(&scatter_dir, name);
                let file = File::open(&path)
                    .map_err(|e| penumbra_mtk::Error::Io(e))?;
                let size = file
                    .metadata()
                    .map_err(|e| penumbra_mtk::Error::Io(e))?
                    .len() as usize;
                Ok((BufReader::new(file), size))
            },
            move |name: &str| -> PenumbraResult<BufWriter<File>> {
                // The DA returns synthetic names like "<id>/boot.bin"; keep
                // only the final component so nothing escapes sink_dir.
                let base = Path::new(name)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| name.to_string());
                let file = File::create(sink_dir.join(base))
                    .map_err(|e| penumbra_mtk::Error::Io(e))?;
                Ok(BufWriter::new(file))
            },
            progress("flash"),
        )
        .context("scatter flash failed")?;
    end_progress();
    eprintln!("done");
    Ok(())
}

// ------------------------------------------------------ bootloader / slot

pub fn set_bootloader_lock(opts: &ConnectOpts, unlock: bool) -> Result<()> {
    let state = if unlock { LockState::Unlock } else { LockState::Lock };
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;

    eprintln!("{} bootloader...", if unlock { "unlocking" } else { "locking" });
    handle
        .device
        .set_seccfg_lock_state(state)
        .context("failed to set seccfg lock state")?;
    eprintln!("done");
    Ok(())
}

fn parse_slot(slot: &str) -> Result<BootPartition> {
    match slot.trim().to_ascii_uppercase().as_str() {
        "A" => Ok(BootPartition::A),
        "B" => Ok(BootPartition::B),
        other => bail!("invalid slot {other:?}; expected A or B"),
    }
}

pub fn slot(opts: &ConnectOpts, action: crate::SlotAction) -> Result<()> {
    // Validate the slot name before opening the device, so a typo is reported
    // without requiring hardware to be plugged in.
    let requested = match &action {
        crate::SlotAction::Get => None,
        crate::SlotAction::Set { slot } => Some(parse_slot(slot)?),
    };

    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    let dev = &mut handle.device;

    match (action, requested) {
        (crate::SlotAction::Get, None) => {
            let bootctrl = dev
                .get_bootctrl()
                .context("device does not expose A/B boot control")?;
            println!("{:?}", bootctrl.get_active_slot());
            Ok(())
        }
        (crate::SlotAction::Set { .. }, Some(target)) => {
            let mut bootctrl = dev
                .get_bootctrl()
                .context("device does not expose A/B boot control")?;

            if bootctrl.get_active_slot() == target {
                eprintln!("active slot is already {target:?}");
                return Ok(());
            }
            bootctrl.set_active_slot(target);

            // The boot control block lives at a fixed offset inside misc.
            let bytes = bootctrl.as_bytes();
            let mut data = vec![0u8; OFFSET_SLOT_SUFFIX + bytes.len()];
            data[OFFSET_SLOT_SUFFIX..].copy_from_slice(bytes);

            eprintln!("writing boot control to misc...");
            dev.write_partition("misc", data.len(), &data[..], progress("write"))
                .context("failed to write boot control to misc")?;
            end_progress();

            // get_bootctrl() serves a cached copy; store what we just wrote so
            // a follow-up query does not report the stale slot.
            dev.devinfo().set_bootctrl(bootctrl);
            eprintln!("active slot set to {target:?}");
            Ok(())
        }
        // Unreachable: `requested` mirrors the action's shape.
        _ => unreachable!("slot action and parsed target disagree"),
    }
}

// -------------------------------------------------------- power / reboot

pub fn reboot(opts: &ConnectOpts, mode: &str) -> Result<()> {
    let bootmode = match mode.trim().to_ascii_lowercase().as_str() {
        "normal" => BootMode::Normal,
        "home" | "home_screen" | "homescreen" => BootMode::HomeScreen,
        "fastboot" => BootMode::Fastboot,
        "test" => BootMode::Test,
        "meta" => BootMode::Meta,
        other => bail!("invalid mode {other:?}; expected normal|home|fastboot|test|meta"),
    };

    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    eprintln!("rebooting to {mode}...");
    handle.device.reboot(bootmode).context("reboot failed")?;
    Ok(())
}

pub fn shutdown(opts: &ConnectOpts) -> Result<()> {
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    eprintln!("powering off...");
    handle.device.shutdown().context("shutdown failed")?;
    Ok(())
}

// ------------------------------------------------------- memory / registers

pub fn peek(opts: &ConnectOpts, address: &str, size: &str, output: &str) -> Result<()> {
    let addr = parse_u64(address)?;
    let len = parse_usize(size)?;

    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;

    let mut writer: Box<dyn Write + Send> = match open_output(output)? {
        Some(w) => w,
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    eprintln!("reading 0x{len:x} bytes from 0x{addr:x}...");
    handle
        .device
        .peek(addr, len, &mut writer, progress("peek"))
        .with_context(|| format!("peek at 0x{addr:x} failed"))?;
    writer.flush().context("failed to flush output")?;
    end_progress();

    if output != "-" {
        eprintln!("wrote {output}");
    }
    Ok(())
}

pub fn poke(opts: &ConnectOpts, address: &str, input: &str) -> Result<()> {
    let addr = parse_u64(address)?;
    let data = read_all(input)?;

    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;

    eprintln!("writing {} to 0x{addr:x}...", human_bytes(data.len() as u64));
    handle
        .device
        .poke(addr, data.len(), &data[..], progress("poke"))
        .with_context(|| format!("poke at 0x{addr:x} failed"))?;
    end_progress();
    eprintln!("done");
    Ok(())
}

pub fn read_reg(opts: &ConnectOpts, address: &str) -> Result<()> {
    let addr = parse_u64(address)?;
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    let value = handle
        .device
        .read_register(addr)
        .with_context(|| format!("read of register 0x{addr:x} failed"))?;
    println!("0x{value:08x}");
    Ok(())
}

pub fn write_reg(opts: &ConnectOpts, address: &str, value: &str) -> Result<()> {
    let addr = parse_u64(address)?;
    let val = parse_u32(value)?;
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    handle
        .device
        .write_register(addr, val)
        .with_context(|| format!("write of register 0x{addr:x} failed"))?;
    eprintln!("0x{addr:x} <= 0x{val:08x}");
    Ok(())
}

// --------------------------------------------------------------- eFuses

pub fn read_efuse(opts: &ConnectOpts, output: &str) -> Result<()> {
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;

    let mut writer: Box<dyn Write + Send> = match open_output(output)? {
        Some(w) => w,
        None => Box::new(BufWriter::new(std::io::stdout())),
    };
    handle
        .device
        .read_efuses(&mut writer)
        .context("efuse read failed")?;
    writer.flush().context("failed to flush output")?;
    if output != "-" {
        eprintln!("wrote {output}");
    }
    Ok(())
}

pub fn write_efuse(opts: &ConnectOpts, input: &str) -> Result<()> {
    let data = read_all(input)?;
    if data.is_empty() {
        bail!("{input} is empty");
    }
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    eprintln!("writing {} to eFuses (irreversible)...", human_bytes(data.len() as u64));
    handle
        .device
        .write_efuses(&data[..], data.len())
        .context("efuse write failed")?;
    eprintln!("done");
    Ok(())
}

// ----------------------------------------------------------------- RPMB

fn parse_region(region: u8) -> Result<RpmbRegion> {
    RpmbRegion::try_from(region)
        .map_err(|_| anyhow::anyhow!("invalid RPMB region {region}; expected 0-3"))
}

pub fn rpmb_read(
    opts: &ConnectOpts,
    region: u8,
    start: u32,
    count: u32,
    output: &str,
) -> Result<()> {
    let region = parse_region(region)?;
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;

    let mut writer: Box<dyn Write + Send> = match open_output(output)? {
        Some(w) => w,
        None => Box::new(BufWriter::new(std::io::stdout())),
    };
    handle
        .device
        .read_rpmb(region, start, count, &mut writer, progress("rpmb read"))
        .context("RPMB read failed")?;
    writer.flush().context("failed to flush output")?;
    end_progress();
    if output != "-" {
        eprintln!("wrote {output}");
    }
    Ok(())
}

pub fn rpmb_write(
    opts: &ConnectOpts,
    region: u8,
    start: u32,
    count: u32,
    input: &str,
) -> Result<()> {
    let region = parse_region(region)?;
    let data = read_all(input)?;
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    handle
        .device
        .write_rpmb(region, start, count, &data[..], progress("rpmb write"))
        .context("RPMB write failed")?;
    end_progress();
    eprintln!("done");
    Ok(())
}

pub fn rpmb_erase(opts: &ConnectOpts, region: u8, start: u32, count: u32) -> Result<()> {
    let region = parse_region(region)?;
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    handle
        .device
        .erase_rpmb(region, start, count, progress("rpmb erase"))
        .context("RPMB erase failed")?;
    end_progress();
    eprintln!("done");
    Ok(())
}

pub fn rpmb_auth(opts: &ConnectOpts, region: u8, key: &str) -> Result<()> {
    let region = parse_region(region)?;
    let key = hex::decode(key.trim()).context("RPMB key is not valid hex")?;
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    handle
        .device
        .auth_rpmb(region, &key)
        .context("RPMB auth failed")?;
    eprintln!("done");
    Ok(())
}

// ------------------------------------------------------------- SEJ / keys

fn parse_key_size(size: &str) -> Result<KeySize> {
    match size.trim() {
        "128" => Ok(KeySize::Key128),
        "192" => Ok(KeySize::Key192),
        "256" => Ok(KeySize::Key256),
        other => bail!("invalid key size {other:?}; expected 128|192|256"),
    }
}

fn parse_sej_key_id(id: &str) -> Result<SejKeyId> {
    match id.trim().to_ascii_lowercase().as_str() {
        "sw" => Ok(SejKeyId::SwKey),
        "hw" => Ok(SejKeyId::HwKey),
        "hw-wrapped" | "wrapped" => Ok(SejKeyId::HwWrappedKey),
        "rid" => Ok(SejKeyId::RidKey),
        "custom" => Ok(SejKeyId::CustomKey),
        other => bail!("invalid key id {other:?}; expected sw|hw|hw-wrapped|rid|custom"),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn sej_aes(
    opts: &ConnectOpts,
    input: &str,
    output: &str,
    key_id: &str,
    key_size: &str,
    decrypt: bool,
    cbc: bool,
    legacy: bool,
    anti_clone: bool,
) -> Result<()> {
    let data = read_all(input)?;
    if data.is_empty() {
        bail!("{input} is empty");
    }

    let params = SejParams {
        length: data.len() as u32,
        encrypt: !decrypt,
        anti_clone,
        xor: false,
        legacy,
        cbc,
        key_id: parse_sej_key_id(key_id)?,
        key_sz: parse_key_size(key_size)?,
        reserved: 0,
    };

    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;

    let mut writer: Box<dyn Write + Send> = match open_output(output)? {
        Some(w) => w,
        None => Box::new(BufWriter::new(std::io::stdout())),
    };
    handle
        .device
        .sej_aes(params, &data[..], &mut writer)
        .context("SEJ AES failed")?;
    writer.flush().context("failed to flush output")?;
    if output != "-" {
        eprintln!("wrote {output}");
    }
    Ok(())
}

fn parse_key_derive_id(id: &str) -> Result<KeyDeriveId> {
    match id.trim().to_ascii_lowercase().as_str() {
        "rpmb" => Ok(KeyDeriveId::Rpmb),
        "fde" => Ok(KeyDeriveId::Fde),
        "tee" => Ok(KeyDeriveId::Tee),
        "aes-img-enc" | "aes_img_enc" => Ok(KeyDeriveId::AesImgEnc),
        "aes-custom" | "aes_custom" => Ok(KeyDeriveId::AesCustom),
        "motorola" => Ok(KeyDeriveId::Motorola),
        "rot" => Ok(KeyDeriveId::Rot),
        other => bail!(
            "invalid key id {other:?}; expected rpmb|fde|tee|aes-img-enc|aes-custom|motorola|rot"
        ),
    }
}

pub fn derive_key(opts: &ConnectOpts, id: &str, size: &str) -> Result<()> {
    let key_id = parse_key_derive_id(id)?;
    let key_size = parse_key_size(size)?;

    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    let key = handle
        .device
        .derive_key_by_id(key_id, key_size)
        .with_context(|| format!("key derivation for {id} failed"))?;
    println!("{}", hex::encode(&key));
    Ok(())
}

pub fn derive_key_input(opts: &ConnectOpts, label: &str, salt: &str, size: &str) -> Result<()> {
    let key_size = parse_key_size(size)?;
    let mut handle = connect::connect(opts)?;
    connect::enter_da(&mut handle)?;
    let key = handle
        .device
        .derive_key_by_input(label.as_bytes(), salt.as_bytes(), key_size)
        .context("key derivation failed")?;
    println!("{}", hex::encode(&key));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check_scatter_platform, extract_platform};

    // -- extract_platform: XML ------------------------------------------------

    #[test]
    fn xml_platform() {
        let content = "<MTKParameter>\n  <platform>MT6768</platform>\n</MTKParameter>";
        assert_eq!(extract_platform(content, true).as_deref(), Some("MT6768"));
    }

    #[test]
    fn xml_platform_is_trimmed() {
        let content = "<platform>\n    MT6785 \n  </platform>";
        assert_eq!(extract_platform(content, true).as_deref(), Some("MT6785"));
    }

    #[test]
    fn xml_platform_absent_is_none() {
        // No <platform> tag at all.
        assert_eq!(extract_platform("<scatter/>", true), None);
    }

    #[test]
    fn xml_platform_empty_is_none() {
        // A present but empty tag must not be mistaken for a real platform.
        assert_eq!(extract_platform("<platform>  </platform>", true), None);
    }

    // -- extract_platform: YAML ----------------------------------------------

    #[test]
    fn yaml_platform() {
        let content = "general:\n  platform: MT6768\n  scatter_version: 0x0";
        assert_eq!(extract_platform(content, false).as_deref(), Some("MT6768"));
    }

    #[test]
    fn yaml_platform_indented_is_found() {
        // Leading whitespace is stripped before matching, so an indented key
        // under a parent section still resolves.
        let content = "  platform: MT6785";
        assert_eq!(extract_platform(content, false).as_deref(), Some("MT6785"));
    }

    #[test]
    fn yaml_platform_no_space_after_colon() {
        assert_eq!(
            extract_platform("platform:MT6765", false).as_deref(),
            Some("MT6765")
        );
    }

    #[test]
    fn yaml_platform_absent_is_none() {
        // A key that merely *ends* with "platform" is not the platform key.
        assert_eq!(extract_platform("scatter_platform: MT6768", false), None);
        assert_eq!(extract_platform("platform_name: MT6768", false), None);
    }

    #[test]
    fn yaml_platform_empty_is_none() {
        assert_eq!(extract_platform("platform:   ", false), None);
    }

    #[test]
    fn xml_parser_is_not_confused_by_yaml_content() {
        // Format is chosen by the caller's extension check; make sure the XML
        // path does not accidentally succeed on YAML input.
        let content = "general:\n  platform: MT6768\n";
        assert_eq!(extract_platform(content, true), None);
    }

    // -- check_scatter_platform ----------------------------------------------

    #[test]
    fn matching_platforms_are_allowed() {
        assert!(check_scatter_platform(Some("MT6768"), Some("MT6768")).is_ok());
    }

    #[test]
    fn platform_comparison_ignores_case() {
        // The scatter and the chip routinely differ only in case.
        assert!(check_scatter_platform(Some("mt6768"), Some("MT6768")).is_ok());
        assert!(check_scatter_platform(Some("Mt6785"), Some("mt6785")).is_ok());
    }

    #[test]
    fn mismatched_platforms_are_refused() {
        let err = check_scatter_platform(Some("MT6768"), Some("MT6785"))
            .expect_err("a cross-chipset scatter must be refused");
        let text = format!("{err:#}");
        assert!(text.contains("MT6768"), "error names the scatter: {text}");
        assert!(text.contains("MT6785"), "error names the device: {text}");
    }

    #[test]
    fn missing_scatter_platform_is_tolerated() {
        // Many scatter files omit <platform>; that must not block flashing.
        assert!(check_scatter_platform(None, Some("MT6768")).is_ok());
    }

    #[test]
    fn unknown_chip_platform_is_tolerated() {
        // A chip we cannot identify is not a mismatch.
        assert!(check_scatter_platform(Some("MT6768"), None).is_ok());
        assert!(check_scatter_platform(None, None).is_ok());
    }
}
