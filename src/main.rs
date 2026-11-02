//! Headless CLI for flashing and exploring MTK devices.
//!
//! This crate is deliberately standalone: it links no Qt and never touches the
//! GUI's build script. Every operation here calls penumbra-mtk directly, on a
//! single thread, because `Device` is `!Send` (it holds `Rc` internally via
//! `DevInfo`). That is the same reason the GUI runs its device on a dedicated
//! worker thread.
//!
//! Conventions:
//!   * Data goes to stdout, progress and diagnostics go to stderr, so the CLI
//!     composes with pipes and scripts.
//!   * Exit codes: 0 success, 1 operational failure, 2 usage error (clap).

mod connect;
mod numbers;
mod ops;
mod progress;
mod repo;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::connect::ConnectOpts;

#[derive(Parser)]
#[command(
    name = "umbrage-cli",
    version,
    about = "Headless CLI for flashing and exploring MTK devices",
    long_about = "Headless CLI for flashing and exploring MTK devices.\n\n\
        Talks to a MediaTek device in BROM/preloader/DA mode over USB. Most \
        commands need the device plugged in and, for protected operations, a \
        DA/auth/preloader triple supplied via --da/--auth/--preloader.\n\n\
        Data is written to stdout; progress and logs go to stderr."
)]
struct Cli {
    #[command(flatten)]
    connect: ConnectOpts,

    /// Trace-level logging on stderr
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Identify the connected chip and report its state
    Info,

    /// List the device's partitions
    Partitions {
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
    },

    /// Read a partition to a file (or stdout with `-`)
    Read {
        /// Partition name, e.g. `boot`
        #[arg(long, short = 'p')]
        partition: String,
        /// Destination file, or `-` for stdout
        #[arg(long, short)]
        output: String,
    },

    /// Write an image file into a partition
    Write {
        #[arg(long, short = 'p')]
        partition: String,
        /// Image file to write, or `-` for stdin
        #[arg(long, short)]
        image: String,
    },

    /// Erase a partition
    Erase {
        #[arg(long, short = 'p')]
        partition: String,
    },

    /// Flash every image referenced by a scatter file
    FlashScatter {
        /// Scatter file (XML or YAML)
        #[arg(long, short)]
        scatter: String,
        /// Directory for readback output (default: alongside the scatter)
        #[arg(long)]
        out_dir: Option<String>,
    },

    /// Unlock the bootloader (writes seccfg)
    Unlock,

    /// Lock the bootloader (writes seccfg)
    Lock,

    /// Query or change the active A/B slot
    Slot {
        #[command(subcommand)]
        action: SlotAction,
    },

    /// Reboot the device into a mode
    Reboot {
        /// normal | home | fastboot | test | meta
        #[arg(long, default_value = "normal")]
        mode: String,
    },

    /// Power the device off
    Shutdown,

    /// Read raw memory to a file (or stdout with `-`)
    Peek {
        /// Start address (hex `0x...` or decimal)
        #[arg(long, short)]
        address: String,
        /// Byte count
        #[arg(long, short)]
        size: String,
        /// Destination file, or `-` for stdout
        #[arg(long, short)]
        output: String,
    },

    /// Write raw memory from a file (or stdin with `-`)
    Poke {
        #[arg(long, short)]
        address: String,
        /// Source file, or `-` for stdin
        #[arg(long, short)]
        input: String,
    },

    /// Read a 32-bit register
    ReadReg {
        #[arg(long, short)]
        address: String,
    },

    /// Write a 32-bit register
    WriteReg {
        #[arg(long, short)]
        address: String,
        /// Value (hex `0x...` or decimal)
        #[arg(long, short)]
        value: String,
    },

    /// Read the eFuse array to a file (or stdout with `-`)
    ReadEfuse {
        #[arg(long, short)]
        output: String,
    },

    /// Write the eFuse array from a file. Irreversible.
    WriteEfuse {
        #[arg(long, short)]
        input: String,
    },

    /// Read a region of RPMB storage
    RpmbRead {
        /// Region index 0-3
        #[arg(long)]
        region: u8,
        #[arg(long)]
        start: u32,
        #[arg(long)]
        count: u32,
        #[arg(long, short)]
        output: String,
    },

    /// Write a region of RPMB storage
    RpmbWrite {
        #[arg(long)]
        region: u8,
        #[arg(long)]
        start: u32,
        #[arg(long)]
        count: u32,
        #[arg(long, short)]
        input: String,
    },

    /// Erase a region of RPMB storage
    RpmbErase {
        #[arg(long)]
        region: u8,
        #[arg(long)]
        start: u32,
        #[arg(long)]
        count: u32,
    },

    /// Provision an RPMB authentication key. Often irreversible.
    RpmbAuth {
        #[arg(long)]
        region: u8,
        /// 32-byte key as hex
        #[arg(long)]
        key: String,
    },

    /// Run SEJ AES over a buffer (encrypt or decrypt)
    SejAes {
        #[arg(long, short)]
        input: String,
        #[arg(long, short)]
        output: String,
        #[arg(long, default_value = "sw")]
        key_id: String,
        #[arg(long, default_value = "128")]
        key_size: String,
        #[arg(long, default_value_t = false)]
        decrypt: bool,
        #[arg(long, default_value_t = false)]
        cbc: bool,
        #[arg(long, default_value_t = false)]
        legacy: bool,
        #[arg(long, default_value_t = false)]
        anti_clone: bool,
    },

    /// Derive a hardware key by well-known ID
    DeriveKey {
        /// rpmb | fde | tee | aes-img-enc | aes-custom | motorola | rot
        #[arg(long)]
        id: String,
        /// 128 | 192 | 256
        #[arg(long, default_value = "256")]
        size: String,
    },

    /// Derive a hardware key from a label and salt
    DeriveKeyInput {
        #[arg(long)]
        label: String,
        #[arg(long)]
        salt: String,
        /// 128 | 192 | 256
        #[arg(long, default_value = "256")]
        size: String,
    },

    /// Browse and fetch DA/auth files from the online template repository
    ///
    /// These subcommands only talk to the network; they never touch USB, so
    /// they work with no device plugged in.
    Templates {
        #[command(subcommand)]
        action: TemplatesAction,
    },
}

#[derive(Subcommand, Clone, Debug)]
pub enum SlotAction {
    /// Print the currently active slot
    Get,
    /// Set the active slot
    Set {
        /// A or B
        slot: String,
    },
}

#[derive(Subcommand)]
enum TemplatesAction {
    /// List the device codenames available in the repository
    List {
        /// Emit the repository's meta.json as JSON instead of a table
        #[arg(long)]
        json: bool,
    },

    /// Show one device's versions and the files each one provides
    Show {
        /// Device codename, e.g. `alioth` (see `templates list`)
        device: String,
        /// Only show this version id
        #[arg(long)]
        version: Option<u32>,
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
    },

    /// Download a device's da/auth/preloader and print the local paths
    Fetch {
        /// Device codename, e.g. `alioth` (see `templates list`)
        device: String,
        /// Version id; defaults to the repository's recommended one
        #[arg(long)]
        version: Option<u32>,
        /// Write files here instead of the local cache
        #[arg(long)]
        dest: Option<String>,
        /// Re-download even if a verified copy is already cached
        #[arg(long)]
        refresh: bool,
        /// Emit JSON (`device`, `version`, `files`) for use in scripts
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let level = if cli.verbose {
        log::LevelFilter::Trace
    } else {
        log::LevelFilter::Info
    };
    env_logger::Builder::new()
        .filter_level(level)
        .target(env_logger::Target::Stderr)
        .init();

    match run(cli) {
        Ok(()) => {}
        Err(err) => {
            eprintln!("error: {err:#}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let connect = cli.connect;
    match cli.command {
        // `templates` only talks to the network; it never touches USB.
        Command::Templates { action } => match action {
            TemplatesAction::List { json } => repo::cmd_list(json),
            TemplatesAction::Show {
                device,
                version,
                json,
            } => repo::cmd_show(&device, version, json),
            TemplatesAction::Fetch {
                device,
                version,
                dest,
                refresh,
                json,
            } => repo::cmd_fetch(
                &device,
                version,
                dest.as_deref().map(std::path::Path::new),
                refresh,
                json,
            ),
        },
        Command::Info => ops::info(&connect),
        Command::Partitions { json } => ops::partitions(&connect, json),
        Command::Read { partition, output } => ops::read_partition(&connect, &partition, &output),
        Command::Write { partition, image } => ops::write_partition(&connect, &partition, &image),
        Command::Erase { partition } => ops::erase_partition(&connect, &partition),
        Command::FlashScatter { scatter, out_dir } => {
            ops::flash_scatter(&connect, &scatter, out_dir.as_deref())
        }
        Command::Unlock => ops::set_bootloader_lock(&connect, true),
        Command::Lock => ops::set_bootloader_lock(&connect, false),
        Command::Slot { action } => ops::slot(&connect, action),
        Command::Reboot { mode } => ops::reboot(&connect, &mode),
        Command::Shutdown => ops::shutdown(&connect),
        Command::Peek {
            address,
            size,
            output,
        } => ops::peek(&connect, &address, &size, &output),
        Command::Poke { address, input } => ops::poke(&connect, &address, &input),
        Command::ReadReg { address } => ops::read_reg(&connect, &address),
        Command::WriteReg { address, value } => ops::write_reg(&connect, &address, &value),
        Command::ReadEfuse { output } => ops::read_efuse(&connect, &output),
        Command::WriteEfuse { input } => ops::write_efuse(&connect, &input),
        Command::RpmbRead {
            region,
            start,
            count,
            output,
        } => ops::rpmb_read(&connect, region, start, count, &output),
        Command::RpmbWrite {
            region,
            start,
            count,
            input,
        } => ops::rpmb_write(&connect, region, start, count, &input),
        Command::RpmbErase {
            region,
            start,
            count,
        } => ops::rpmb_erase(&connect, region, start, count),
        Command::RpmbAuth { region, key } => ops::rpmb_auth(&connect, region, &key),
        Command::SejAes {
            input,
            output,
            key_id,
            key_size,
            decrypt,
            cbc,
            legacy,
            anti_clone,
        } => ops::sej_aes(
            &connect,
            &input,
            &output,
            &key_id,
            &key_size,
            decrypt,
            cbc,
            legacy,
            anti_clone,
        ),
        Command::DeriveKey { id, size } => ops::derive_key(&connect, &id, &size),
        Command::DeriveKeyInput { label, salt, size } => {
            ops::derive_key_input(&connect, &label, &salt, &size)
        }
    }
}
