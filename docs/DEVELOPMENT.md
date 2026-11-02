# Development guide

How the CLI is put together, why it is built the way it is, and how to change
it safely.

- [Architecture](#architecture)
- [Module map](#module-map)
- [Design decisions](#design-decisions)
- [The build](#the-build)
- [Testing](#testing)
- [Packaging](#packaging)
- [Adding a subcommand](#adding-a-subcommand)
- [Troubleshooting](#troubleshooting)

## Architecture

The CLI is a single binary that calls `penumbra-mtk` directly. There is no
async runtime, no worker thread, and no Qt. One process handles one command:
connect, act, exit.

```
main.rs        clap definitions, dispatch, exit codes
  |
  +-- connect.rs    find USB port, read da/auth/preloader, handshake, enter DA
  |
  +-- ops.rs        one function per device subcommand
  |                   |
  |                   +-- numbers.rs    argument parsing (u64/u32/usize), human sizes
  |                   +-- progress.rs   stderr progress rendering
  |
  +-- repo.rs       online template repository (fetch, verify, cache)
```

### Execution flow

Every device command runs the same six stages (see `connect::connect`):

```
validate args & open local files     <- failures here need no device
        |
resolve --template (if given)       <- network; explicit flags win
        |
find USB port (poll until --wait)   <- "no device found" lives here
        |
device.init()                        <- handshake; identifies the chip
        |
device.enter_da_mode()               <- upload the Download Agent
        |
perform the operation, exit
```

Two commands are exceptions:

- **`info`** skips `enter_da_mode` when DA upload fails, so it can still
  identify a chip that refuses the Agent. It reports what it can and notes the
  omission on stderr.
- **`templates`** never touches USB at all. It is pure network + filesystem.

## Module map

### `main.rs`

Owns the `clap` definitions (`Cli`, `Command`, `TemplatesAction`, `SlotAction`)
and `run()`, which maps each subcommand to a function in `ops` or `repo`. Also
sets up `env_logger` (stderr, trace level with `-v`).

`run()` matches `Command::Templates` first as a distinct arm — the templates
subcommands need no `ConnectOpts`, though the global flags still parse.

### `connect.rs`

Everything about reaching a device.

- `ConnectOpts` — the `#[derive(Parser)]` struct flattened into `Cli`. Holds
  `--da`/`--auth`/`--preloader`, `--template` and friends, `--backend`,
  `--vid`/`--pid`, `--wait`.
- `read_blob()` — read a file into a `'static` buffer (see
  [Box::leak](#device-is-sendso-buffers-are-leaked)).
- `find_port()` — poll `PortType::find_and_open` until a device appears or the
  deadline passes.
- `connect()` — the staged flow above; returns a `DeviceHandle`.
- `enter_da()` — upload the DA, unlocking storage operations.

### `ops.rs`

One `pub fn` per device subcommand, each taking `&ConnectOpts` plus the
command's own arguments. Also holds shared helpers:

- `open_output()` / `open_input()` — the `-`-means-stdio convention.
- `read_all()` — slurp an input, for operations whose length must be known
  before the transfer starts.
- `resolve_image()` — resolve a scatter-relative path.
- `extract_platform()` / `check_scatter_platform()` — the scatter safety check.

### `repo.rs`

The CLI counterpart of the GUI's `src/templates/`. Fetches
`meta.json`, walks its schema (`Meta` → `Device` → `Version` → `TemplateFile`),
downloads files with SHA-256 verification, and caches them.

- `fetch_meta()` / `find_device()` / `select_version()` — repository lookup.
- `resolve()` — the `--template` entry point; returns verified local paths.
- `cmd_list()` / `cmd_show()` / `cmd_fetch()` — the `templates` subcommands.

### `numbers.rs`

`parse_u64` / `parse_u32` / `parse_usize` and `human_bytes`. Carries the unit
tests for numeric parsing.

### `progress.rs`

`progress(label)` builds a callback that redraws a single line on stderr,
redrawing only when the percentage changes; `end_progress()` finishes the line.
Kept separate from `ops` so both `ops` and `repo` can use it without depending
on each other.

## Design decisions

These are the invariants that are easy to break by accident. Each one exists
for a concrete reason.

### `cli/` is a standalone crate, not a workspace member

`/Cargo.toml` (the GUI package) deliberately has **no `[workspace]` table
pointing at `cli/`**. The GUI's `build.rs` locates Qt via `qmake`; if `cli/`
were a workspace member, building the CLI would run that script and require a
Qt install.

Because it is standalone, `cli/` has its own `Cargo.lock` and its own
`target/`. This is what makes "no Qt, ever" a structural guarantee rather than
a convention.

**Consequence:** the packaging script runs `cargo build` with `cwd=cli/`, and a
`ldd` check fails the build if `libQt6`/`libQt5` ever appears in the CLI's
dependencies.

### `Device` is `!Send`, so the CLI is single-threaded

`penumbra_mtk::Device` holds `Rc` internally (via `DevInfo`), so it cannot be
sent across threads. The GUI works around this with a dedicated worker thread
that owns the device and receives commands over a channel. The CLI sidesteps
the problem entirely: one thread, one device, one operation.

This is also why the CLI uses **no async runtime** — there is nothing to
concurrently drive.

### Buffers are leaked with `Box::leak`

`Device<'static, PortType>` borrows the DA/auth/preloader blobs for its whole
lifetime. Rather than fight the borrow checker with owned buffers, `read_blob`
leaks the `Vec<u8>` into a `&'static [u8]`.

This is the same trick the GUI uses, and it costs nothing here: the process
runs one command and exits, so the memory is returned to the OS on exit.

### `ureq`, not `reqwest`, for HTTP

`repo.rs` uses [`ureq`](https://docs.rs/ureq) for its two HTTP needs (meta.json
and file downloads). Reasons:

- **Synchronous.** No tokio, no hyper — consistent with the single-threaded
  design above.
- **`ring`, not `aws-lc-rs`.** ureq's default `rustls` feature selects the
  `ring` crypto provider, so there is no `aws-lc-sys` C build (which needs
  cmake and is fragile to cross-compile).
- **`webpki-roots`.** Root certificates are bundled, so there is no runtime
  dependency on the host CA store — important for a relocatable binary.

It is pinned to an exact version in `Cargo.toml` so the dependency tree stays
reproducible and offline-buildable from the registry cache.

### Numbers are decimal unless prefixed

Bare digits parse as **decimal**, and nothing is ever auto-promoted to hex.
`parse_u64` accepts `0x`/`0X` for hex and `_` as a separator; anything without a
`0x` prefix is decimal. There is deliberately no octal or binary form, and no
"looks like hex, treat it as hex" heuristics.

**Rationale:** in a tool that writes to arbitrary memory and eFuses, silently
reading `1000` as `0x1000` (4096) instead of one thousand is a brick waiting to
happen. Being explicit is worth the keystroke.

### Arguments are validated before USB

Argument parsing and local file reads happen *before* any device discovery. A
malformed `--slot`, a missing `--da` file, or a bad address therefore reports
the actual problem, instead of being masked by a 15-second "no device found"
timeout.

### stdout is data, stderr is progress

Every byte of *result* goes to stdout; progress bars, `downloading …` lines,
and diagnostics go to stderr. This makes the tool compose with pipes and
scripts:

```sh
umbrage-cli read -p boot -o - > boot.img          # clean image
eval $(umbrage-cli templates fetch lancelot --json | jq -r '.files | to_entries[] | "\(.key)=\(.value)"')
```

### Explicit flags beat templates

`--da`/`--auth`/`--preloader` always win over `--template`, per file. If a user
passes all three explicitly, the template is skipped entirely (and a note is
printed) rather than triggering a pointless network fetch that could fail a
command for no reason.

### `flash-scatter` refuses a mismatched chipset

Before entering DA mode, `flash_scatter()` extracts the scatter file's
`<platform>` (XML) or `platform:` (YAML) field and compares it to the connected
chip. A mismatch is a hard error.

This mirrors a check the GUI performs when a scatter is loaded. Without it, a
layout built for another SoC would be flashed verbatim — a common way to brick
a device. A *missing* platform on either side is tolerated (many scatter files
omit it, and an unidentified chip is not a mismatch); only a genuine mismatch
is refused.

The parsing logic is unit-tested, because a bug there would silently disable
the protection.

### Files are written atomically and re-verified

`repo.rs` downloads to a sibling `.part` file and renames on success, so an
interrupted download never leaves a partial file where a later run would accept
it. Cached files are **re-hashed on every reuse**, so a truncated or tampered
cache entry is detected rather than passed to the device.

## The build

```sh
cd cli
cargo build --release
```

That is the entire build. No Qt environment variables, no `qmake`, no `rcc`.

- **Toolchain:** nightly, selected automatically by `cli/rust-toolchain.toml`.
  Stable fails because `penumbra-mtk` (via `acon`) uses `const_trait_impl` and
  `trait_alias`.
- **System libraries:** `libudev` (linked dynamically, for USB enumeration) and
  a C toolchain (for vendored `libusb` and `ring`).
- **`rusb` is built with the `vendored` feature**, so `libusb` is statically
  linked into the binary. Only `libudev` remains a runtime dependency.

To confirm the binary is Qt-free:

```sh
ldd target/release/umbrage-cli | grep -i qt || echo "clean"
```

## Testing

```sh
cargo test --release
```

Tests are unit tests living beside the code:

- `numbers::tests` — hex/decimal/octal/binary parsing, underscore grouping,
  garbage rejection, `human_bytes`, and that error messages name the offending
  input.
- `ops::tests` — the scatter platform parser (`extract_platform`) for both XML
  and YAML, and the match/mismatch/missing/unknown decision matrix
  (`check_scatter_platform`).

The scatter tests exist because that check is a safety interlock: if the
parser silently stopped matching, flashing would proceed unchecked. They cover
the negative cases explicitly (a key merely *ending* in `platform` must not
match; YAML content must not parse through the XML path).

There are no hardware-in-the-loop tests. Device behaviour is exercised manually
against real hardware.

### Manual verification checklist

After a change that touches device I/O, verify against real hardware:

1. `info` reports chip, security flags, storage, and partition count.
2. `partitions` lists partitions; `--json` is valid JSON.
3. `read`/`write` round-trip a safe partition (e.g. `boot`).
4. `flash-scatter` flashes a matching scatter, and **refuses** a mismatched one.
5. `templates list` / `show` / `fetch` work with no device plugged in.
6. `--template <device>` resolves files, then reaches USB discovery.
7. Exit codes: `0` on success, `1` with no device, `2` on bad flags.

## Packaging

Packaging lives in the repository's `scripts/`, shared with the GUI so the two
artifacts cannot drift apart.

### `scripts/build_portable.py`

Builds the GUI AppDir **and** the CLI in one pass:

1. `build_cli()` — runs `cargo build --release` with `cwd=cli/`.
2. Deploys the GUI into an AppDir (reusing `build_appimage.py`'s pipeline).
3. `make_portable()` — assembles the folder and copies:
   - `usr/bin/umbrage-cli` — the real binary. It links no Qt, so it needs
     neither the bundled libraries nor an `rpath`.
   - `umbrage-cli` at the root — a Qt-free launcher:
     ```sh
     #!/bin/sh
     HERE="$(dirname "$(readlink -f "$0")")"
     exec "$HERE/usr/bin/umbrage-cli" "$@"
     ```
4. `smoke_test_cli()` — runs `--version` and `--help`, then runs `ldd` on the
   real binary and **fails the build** if `libQt6` or `libQt5` appears.

Run it with the Qt environment set (this is only needed because the *GUI* is
built in the same pass — the CLI itself needs none of it):

```sh
export QT=/opt/Qt/6.10.3/gcc_arm64
export PATH="$QT/bin:$PATH"
export LD_LIBRARY_PATH="$QT/lib"
export QMAKE="$QT/bin/qmake"
export RCC="$QT/libexec/rcc"

python3 scripts/build_portable.py --smoke-test
```

Produces `dist/umbrage-<version>-linux-<arch>/` and a matching `.tar.gz`.

### Building the CLI alone

For a CLI-only artifact, skip the packaging script:

```sh
cd cli && cargo build --release
cp target/release/umbrage-cli /somewhere/
```

No rpath fixups are needed — the binary's only non-libc dependency is
`libudev`, which comes from the target system.

> **Note:** `cargo build --release` replaces the binary, so any `patchelf`
> rpath applied for local testing is lost on the next build.

## Adding a subcommand

The pattern, end to end. Suppose we add `vbat` (read the battery voltage).

1. **Define it in `main.rs`.** Add a variant to `Command`:

   ```rust
   /// Read the battery voltage
   Vbat,
   ```

   Clap turns `Vbat` into the subcommand `vbat` and derives `--help` from the
   doc comment.

2. **Dispatch in `run()`.** Add an arm:

   ```rust
   Command::Vbat => ops::vbat(&connect),
   ```

3. **Implement it in `ops.rs`.** Follow the established shape — connect, enter
   DA, act, print to stdout:

   ```rust
   pub fn vbat(opts: &ConnectOpts) -> Result<()> {
       let mut handle = connect::connect(opts)?;
       connect::enter_da(&mut handle)?;
       let mv = handle.device.read_vbat().context("battery read failed")?;
       println!("{mv}");
       Ok(())
   }
   ```

4. **Document it** in `README.md` under the right group, and mention any
   destructive behaviour in the top warning.

Checklist for any new subcommand:

- [ ] Data on **stdout**, progress/logs on **stderr**.
- [ ] `-` supported for any new file argument (use `open_output`/`open_input`).
- [ ] Numbers parsed through `numbers::parse_*`, never `str::parse` directly.
- [ ] Validation and local file I/O before `connect::connect()`.
- [ ] Destructive/irreversible operations documented in `--help` **and** the
      README warning.
- [ ] Errors wrapped with `.context(...)` so failures are self-describing.
- [ ] `cargo test --release` still passes; add tests for any new pure logic.

## Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| `error: no MTK device found after Ns` | Device not plugged in, not powered off, or not in BROM/preloader mode. Try `--wait 60`. On a fresh system as a non-root user you may be missing udev rules (see below). |
| `USB enumeration failed` / permission denied | The user cannot access the USB device. Run once as root to confirm, or install udev rules for the MTK vendor ID (the GUI's Setup page installs `50-android.rules`, `51-edl.rules`, `52-mtk.rules`). The CLI does not yet ship this check. |
| `Could not find Qt` / missing `qmake` | You are building the **GUI**. The CLI needs no Qt; make sure you are in `cli/`. |
| Build fails with `const_trait_impl` / `trait_alias` errors, or "edition 2024 requires a nightly compiler" | Building with stable Rust. Use the nightly toolchain — `cli/rust-toolchain.toml` selects it automatically via rustup. |
| `SHA256 mismatch for …` | The repository's file does not match its published checksum. Do **not** flash it. Re-run with `--refresh-template`; if it persists, the repository entry is wrong or the download is being tampered with. |
| `scatter file platform (X) does not match the connected device (Y)` | The safety interlock. You are about to flash a layout built for another chipset. Use the correct scatter file; do not bypass this. |
| Handshake fails but the port is found | Usually the wrong or missing `--da`/`--auth`. Try `--template <device>`, or run `info` first, which reports what it can even when DA upload fails. |
