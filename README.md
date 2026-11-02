# Umbrage CLI

A headless command-line tool for MediaTek devices, built on the same
[penumbra](https://github.com/umbrage-org/umbrage-penumbra) core as the Umbrage
GUI — but with **no Qt, no display, and no GUI build system**. It is a single
self-contained binary you can drop onto any arm64 Linux box and drive from a
shell, a script, or CI.

It covers everything the GUI does, plus a set of low-level and exploit
operations the GUI does not expose: raw memory access, register and eFuse
access, RPMB, SEJ AES, and hardware key derivation.

> **Warning**
> Several commands are destructive and some are irreversible
> (`write-efuse`, `rpmb-auth`, `poke`, `erase`, `unlock`/`lock`). A wrong
> address or image can brick a device. Read `--help` for a command before
> running it, and prefer a backup (`read`) before any write.

## Contents

- [Highlights](#highlights)
- [Building](#building)
- [Quick start](#quick-start)
- [How commands reach the device](#how-commands-reach-the-device)
- [Global options](#global-options)
- [Command reference](#command-reference)
- [Conventions](#conventions)
- [Templates](#templates)
- [Exit codes](#exit-codes)
- [See also](#see-also)

## Highlights

- **Standalone.** Links no Qt at all. `ldd` shows only `libc`, `libudev`,
  `libgcc_s`, and `libcap`. The packaging script enforces this with an `ldd`
  guard so a future change cannot silently pull Qt in.
- **Relocatable.** No bundled-library directory, no `rpath` fixups, no
  environment to set. Copy the binary anywhere and run it.
- **Script-friendly.** Data goes to stdout, progress and diagnostics to stderr,
  so `umbrage-cli read -p boot -o - > boot.img` yields a clean image. Exit
  codes distinguish success, operational failure, and usage error.
- **Single USB session per command.** One process, one connection, one
  operation, then exit — the predictable shape for a CLI.
- **Safe defaults.** Bare digits parse as decimal and are never silently
  reinterpreted as hex; a scatter file built for a different chipset is
  refused before a single byte is written.

## Building

### Prerequisites

| Requirement | Why |
| --- | --- |
| Rust **nightly** (via [rustup](https://rustup.rs)) | `penumbra-mtk` uses nightly-only features; `cli/rust-toolchain.toml` selects it automatically |
| `pkg-config`, `libudev-dev` | `rusb` links `libudev` for USB enumeration |
| A C toolchain (`cc`) | Builds the vendored `libusb` and `ring` |
| Network access | First build fetches crates from crates.io |

On Debian/Ubuntu:

```sh
sudo apt install pkg-config libudev-dev build-essential
```

### Build and test

```sh
cd cli
cargo build --release          # -> target/release/umbrage-cli
cargo test  --release
```

The build needs **no Qt environment variables** — that is the whole point of
`cli/` being a standalone crate. If you find yourself exporting `QT`/`QMAKE`,
you are in the wrong directory.

> `cargo build --release` writes a fresh binary, so any manual `patchelf`
> rpath you applied for local testing is lost. The packaging script sets its
> own rpath; see [Packaging](docs/DEVELOPMENT.md#packaging).

### Cross-compiling

This crate is developed and tested for the host architecture. For other
targets, use the usual `cargo build --release --target <triple>` with the
matching toolchain; nothing in the crate is architecture-specific. The
repository's `CROSSCOMPILE.md` covers the GUI's cross-build, which is a
separate and heavier process (Qt must be built for the target too).

#### aarch64 Android

```sh
cd cli
cargo build --release --target aarch64-linux-android
```

**Prerequisites:**

| Requirement | Install / set |
| --- | --- |
| Rust `aarch64-linux-android` target | `rustup target add aarch64-linux-android` |
| Android NDK (for the clang cross-compiler) | Download from <developer.android.com/ndk> |
| `CC_aarch64_linux_android` env var | Point to NDK's `aarch64-linux-android<TARGET>-clang` |

If you use the NDK via `$ANDROID_NDK_HOME`:

```sh
export NDK=$ANDROID_NDK_HOME
export CC_aarch64_linux_android=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android21-clang
cargo build --release --target aarch64-linux-android
```

The resulting binary needs only `libc` and `libdl` from the Android system
— no Qt, no Qt runtime. Check with:

```sh
readelf -d target/aarch64-linux-android/release/umbrage-cli | grep NEEDED
```

## Quick start

```sh
# What chip is this?
umbrage-cli info

# List partitions as JSON
umbrage-cli partitions --json

# Dump and restore one partition
umbrage-cli read  -p boot -o boot.img
umbrage-cli write -p boot -i boot.img

# Flash a whole scatter file (refuses a mismatched chipset)
umbrage-cli flash-scatter --scatter MT6768_Android_scatter.xml

# Bootloader
umbrage-cli unlock
umbrage-cli reboot --mode fastboot

# Use the community template repo for the DA/auth/preloader
umbrage-cli templates list
umbrage-cli info --template lancelot
```

Most operations need the device plugged in, powered off, and in BROM or
preloader mode; protected operations additionally need `--da`/`--auth`/
`--preloader`, or a `--template`.

## How commands reach the device

Every device command follows the same sequence:

1. **Validate arguments and open local files** — before any USB work, so a
   typo or missing file is reported as such instead of being masked by
   "no device found".
2. **Resolve `--template`** (if given) — fetch and verify the DA/auth/
   preloader from the repository. Explicit `--da`/`--auth`/`--preloader`
   always win over the template.
3. **Discover the device** — poll USB for an MTK port until one appears or
   `--wait` expires.
4. **Handshake** (`init`) — identify the chip.
5. **Enter DA mode** — upload the Download Agent, unlocking storage access.
6. **Perform the operation** and exit.

`info` is the exception: it skips step 5 when DA upload fails, so it can still
identify a chip that refuses the Agent.

## Global options

These apply to every command that touches the device:

| Option | Meaning |
| --- | --- |
| `--da <FILE>` | Download Agent |
| `--auth <FILE>` | Auth/SBC file, required on DAA-protected devices |
| `--preloader <FILE>` | Preloader |
| `--template <DEVICE>` | Fetch the device's da/auth/preloader from the repo instead of passing them by hand |
| `--template-version <ID>` | Template version id (requires `--template`) |
| `--refresh-template` | Re-download template files even if verified and cached (requires `--template`) |
| `--backend <auto\|libusb>` | USB backend (only these two are compiled in) |
| `--vid <ID>` / `--pid <ID>` | Restrict discovery to one USB device (hex or decimal) |
| `--wait <SECS>` | How long to wait for a device to appear (default `15`) |
| `-v`, `--verbose` | Trace-level logging on stderr |

## Command reference

Run `umbrage-cli <command> --help` for a command's full options.

### Device

| Command | Description |
| --- | --- |
| `info` | Chip, hardware code, security flags, SoC/MEID, storage, active slot, partition count |
| `partitions [--json]` | List partitions |
| `reboot [--mode <MODE>]` | Reboot into `normal`, `home`, `fastboot`, `test`, or `meta` |
| `shutdown` | Power the device off |
| `slot get` / `slot set <A\|B>` | Query or change the active A/B slot |

### Partitions

| Command | Description |
| --- | --- |
| `read -p <NAME> -o <FILE\|->` | Read a partition to a file, or stdout with `-` |
| `write -p <NAME> -i <FILE\|->` | Write an image into a partition, or stdin with `-` |
| `erase -p <NAME>` | Erase a partition |
| `flash-scatter -s <FILE> [--out-dir <DIR>]` | Flash every image in a scatter file; readback goes to `--out-dir` |

`flash-scatter` parses the scatter's `<platform>`/`platform:` field and
**refuses to flash** if it does not match the connected chip, so a layout built
for another chipset never reaches the device.

### Bootloader

| Command | Description |
| --- | --- |
| `unlock` | Unlock the bootloader (writes `seccfg`) |
| `lock` | Lock the bootloader (writes `seccfg`) |

### Memory and registers

| Command | Description |
| --- | --- |
| `peek -a <ADDR> -s <SIZE> -o <FILE\|->` | Read raw memory |
| `poke -a <ADDR> -i <FILE\|->` | Write raw memory |
| `read-reg -a <ADDR>` | Read a 32-bit register |
| `write-reg -a <ADDR> -v <VALUE>` | Write a 32-bit register |

### eFuse

| Command | Description |
| --- | --- |
| `read-efuse -o <FILE\|->` | Read the eFuse array |
| `write-efuse -i <FILE>` | Write the eFuse array — **irreversible** |

### RPMB

| Command | Description |
| --- | --- |
| `rpmb-read --region <0-3> --start <N> --count <N> -o <FILE>` | Read an RPMB region |
| `rpmb-write --region <0-3> --start <N> --count <N> -i <FILE>` | Write an RPMB region |
| `rpmb-erase --region <0-3> --start <N> --count <N>` | Erase an RPMB region |
| `rpmb-auth --region <0-3> --key <32-BYTE-HEX>` | Provision an RPMB key — **often irreversible** |

### Crypto

| Command | Description |
| --- | --- |
| `sej-aes -i <IN> -o <OUT> [--key-id sw] [--key-size 128] [--decrypt] [--cbc] [--legacy] [--anti-clone]` | Run SEJ AES over a buffer |
| `derive-key --id <ID> [--size 256]` | Derive a hardware key by well-known ID (`rpmb`, `fde`, `tee`, `aes-img-enc`, `aes-custom`, `motorola`, `rot`) |
| `derive-key-input --label <L> --salt <S> [--size 256]` | Derive a hardware key from a label and salt |

### Templates

| Command | Description |
| --- | --- |
| `templates list [--json]` | List the device codenames in the online repository |
| `templates show <DEVICE> [--version <ID>] [--json]` | Show a device's versions and their files |
| `templates fetch <DEVICE> [--version <ID>] [--dest <DIR>] [--refresh] [--json]` | Download and verify a device's files, print the local paths |

## Conventions

- **stdout carries data, stderr carries progress.** Pipe stdout freely; progress
  bars and log lines never pollute it.
- **`-` means standard stream.** For any `-o`/`--output`, `-` is stdout; for
  any `-i`/`--input`, `-` is stdin.
- **Numbers are decimal by default.** `0x1000` is hex; `1000` is one thousand.
  The tool never guesses, because silently reinterpreting an address in
  something that writes memory is dangerous. Underscores are allowed as
  separators (`0x1000_0000`).
- **Arguments are validated before USB.** A bad `--slot`, a missing file, or a
  malformed address fails immediately, without waiting for a device.

## Templates

The template repository holds community-maintained DA/auth/preloader files,
keyed by device codename and verified with SHA-256.

```sh
# Browse
umbrage-cli templates list
umbrage-cli templates show lancelot

# Download and verify, print paths (usable in a script with --json)
umbrage-cli templates fetch lancelot --json

# Or let a device command fetch what it needs on the fly
umbrage-cli info --template lancelot
umbrage-cli write -p boot -i boot.img --template lancelot --da ./custom-da.bin
```

Downloaded files are cached under `$XDG_CACHE_HOME/umbrage-cli/templates/`
(and re-hashed on every reuse, so a truncated or tampered cache entry is
detected rather than flashed). Pass `--refresh-template` to force a fresh
download. An explicit `--da`/`--auth`/`--preloader` always wins over the
template, so a template can fill in only the pieces you left out.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Operational failure (no device, USB error, verification failed, …) |
| `2` | Usage error (bad flags; reported by clap) |

## See also

- [Development guide](docs/DEVELOPMENT.md) — architecture, module map, design
  decisions, adding a subcommand, testing, and packaging.
- The [Umbrage GUI](../README.md) — the graphical front end over the same core.

## License

AGPL-3.0-or-later. See the repository's license files.
