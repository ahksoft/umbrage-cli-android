//! Native library API for umbrage-cli.
//!
//! This crate-type = `cdylib` target exposes a C ABI suitable for JNI or
//! direct dlopen on Android and other targets. The functions mirror the
//! CLI's core operations but use raw pointers and return codes instead of
//! `Result`.
//!
//! Build:
//! ```sh
//! cargo build --release --target aarch64-linux-android --lib
//! ```
//! The output is `target/aarch64-linux-android/release/libumbrage_cli.so`.

#![allow(non_snake_case, non_camel_case_types)]

mod connect;
mod numbers;
mod ops;
mod progress;
mod repo;

#[cfg(target_os = "android")]
mod jni_bridge;

use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;
use std::ffi::{CStr, CString};
use std::io::Write;
use std::slice;
use std::str;

// Re-export types needed by `ops.rs` (which is shared between the binary
// and the library crate).
pub use connect::{ConnectOpts, DeviceHandle};

// Import the Storage trait to get `as_str()` on StorageKind variants.
use penumbra_mtk::Storage;

/// Slot action for A/B slot management.
#[derive(Clone, Debug)]
pub enum SlotAction {
    /// Print the currently active slot
    Get,
    /// Set the active slot
    Set { slot: String },
}

// ---------------------------------------------------------------------------
// Status codes
// ---------------------------------------------------------------------------

/// Success (0) or a negative error code.
pub const UMBRAGE_OK: c_int = 0;
pub const UMBRAGE_ERR_NO_DEVICE: c_int = -1;
pub const UMBRAGE_ERR_USB: c_int = -2;
pub const UMBRAGE_ERR_DA: c_int = -3;
pub const UMBRAGE_ERR_NOT_FOUND: c_int = -4;
pub const UMBRAGE_ERR_IO: c_int = -5;
pub const UMBRAGE_ERR_INVALID_ARG: c_int = -6;
/// A human-readable error string is available via `umbrage_last_error`.
pub const UMBRAGE_ERR_RUNTIME: c_int = -7;

// Thread-local buffer for the last error message.
thread_local! {
    static LAST_ERROR: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
}

/// Returns the last error message set by a failing call, or `NULL` if there
/// is none.  The caller must free the returned pointer with
/// `umbrage_free_string`.
///
/// ```c
/// const char* umbrage_last_error(void);
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn umbrage_last_error() -> *mut c_char {
    LAST_ERROR.with(|s| {
        match s.borrow().as_ref() {
            Some(msg) => CString::new(msg.as_str())
                .unwrap()
                .into_raw(),
            None => std::ptr::null_mut(),
        }
    })
}

/// Clears the last-error buffer.  Safe to call from any thread.
#[unsafe(no_mangle)]
pub extern "C" fn umbrage_clear_error() {
    LAST_ERROR.with(|s| *s.borrow_mut() = None);
}

/// Stores an error message in thread-local storage.
fn set_error(msg: String) {
    LAST_ERROR.with(|s| *s.borrow_mut() = Some(msg));
}

/// A no-op progress callback compatible with `ProgressCallback`.
fn silent_progress(_written: usize, _total: usize) {}

/// Returns the library version string.  The caller must free with
/// `umbrage_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn umbrage_version() -> *mut c_char {
    CString::new(format!("umbrage-cli {}", env!("CARGO_PKG_VERSION")))
        .unwrap()
        .into_raw()
}

// ---------------------------------------------------------------------------
// Device info
// ---------------------------------------------------------------------------

/// Result struct for `umbrage_device_info`.
/// All string fields are heap-allocated and must be freed with
/// `umbrage_device_info_free`.
#[repr(C)]
pub struct UmbrageDeviceInfo {
    pub chip: *mut c_char,
    pub hw_code: u16,
    pub sbc: c_int,
    pub sla: c_int,
    pub daa: c_int,
    pub soc_id: *mut c_char,
    pub meid: *mut c_char,
    pub storage: *mut c_char,
    pub active_slot: *mut c_char,
    pub partition_count: usize,
}

/// Retrieve device information.
///
/// * `da`, `auth`, `preloader` – optional paths to the DA/auth/preloader blobs
///   (may be NULL / empty string).
/// * `wait_secs` – seconds to wait for a device to appear (0 = default 15s).
///
/// On success returns `UMBRAGE_OK` and fills `out` (caller-allocated).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_device_info(
    da: *const c_char,
    auth: *const c_char,
    preloader: *const c_char,
    wait_secs: u64,
    out: *mut UmbrageDeviceInfo,
) -> c_int {
    if out.is_null() {
        return UMBRAGE_ERR_INVALID_ARG;
    }

    let opts = match cstr_to_string_opts(da, auth, preloader, wait_secs) {
        Ok(o) => o,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    let mut handle = match connect::connect(&opts) {
        Ok(h) => h,
        Err(e) => {
            set_error(e.to_string());
            return UMBRAGE_ERR_NO_DEVICE;
        }
    };

    let dev = &mut handle.device;
    let hw_code = dev.devinfo().hw_code();
    let chip = dev.devinfo().chip();
    let chip_str = chip.map(|c| format!("{c:?}")).unwrap_or_default();
    let soc_id = hex::encode(dev.devinfo().soc_id());
    let meid = hex::encode(dev.devinfo().meid());
    let sbc = dev.devinfo().sbc_enabled() as c_int;
    let sla = dev.devinfo().sla_enabled() as c_int;
    let daa = dev.devinfo().daa_enabled() as c_int;
    let partition_count = dev.partitions().len();
    // Release the mutable borrow before re-borrowing handle for enter_da.
    let _ = dev;

    // Try to enter DA mode for storage details.
    let storage = match connect::enter_da(&mut handle) {
        Ok(_) => {
            let dev = &mut handle.device;
            let s = dev.get_storage();
            // StorageKind doesn't implement Debug; match on the variant.
            match s {
                Some(penumbra_mtk::StorageKind::Emmc(e)) => e.as_str().to_string(),
                Some(penumbra_mtk::StorageKind::Ufs(u)) => u.as_str().to_string(),
                None => String::new(),
            }
        }
        Err(_) => String::new(),
    };

    let active_slot = if !storage.is_empty() {
        let dev = &mut handle.device;
        match dev.get_bootctrl() {
            Ok(bootctrl) => {
                let as_ref = bootctrl.get_active_slot();
                format!("{as_ref:?}")
            }
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };

    (*out).chip = to_c_string(chip_str);
    (*out).hw_code = hw_code;
    (*out).sbc = sbc;
    (*out).sla = sla;
    (*out).daa = daa;
    (*out).soc_id = to_c_string(soc_id);
    (*out).meid = to_c_string(meid);
    (*out).storage = to_c_string(storage);
    (*out).active_slot = to_c_string(active_slot);
    (*out).partition_count = partition_count;

    UMBRAGE_OK
}

/// Frees a [`UmbrageDeviceInfo`] struct and all its string fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_device_info_free(info: *mut UmbrageDeviceInfo) {
    if info.is_null() {
        return;
    }
    unsafe {
        let info = &mut *info;
        free_c_string(std::mem::take(&mut info.chip));
        free_c_string(std::mem::take(&mut info.soc_id));
        free_c_string(std::mem::take(&mut info.meid));
        free_c_string(std::mem::take(&mut info.storage));
        free_c_string(std::mem::take(&mut info.active_slot));
    }
}

// ---------------------------------------------------------------------------
// Partition listing
// ---------------------------------------------------------------------------

/// Callback type for partition enumeration.  Receives the partition name,
/// size in bytes, and load address.  Return non-zero to stop enumeration.
pub type PartitionCallback =
    extern "C" fn(name: *const c_char, size: u64, address: u64, userdata: *mut c_void) -> c_int;

/// List partitions via a callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_list_partitions(
    da: *const c_char,
    auth: *const c_char,
    preloader: *const c_char,
    wait_secs: u64,
    cb: PartitionCallback,
    userdata: *mut c_void,
) -> c_int {
    let opts = match cstr_to_string_opts(da, auth, preloader, wait_secs) {
        Ok(o) => o,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    let mut handle = match connect::connect(&opts) {
        Ok(h) => h,
        Err(e) => {
            set_error(e.to_string());
            return UMBRAGE_ERR_NO_DEVICE;
        }
    };

    if let Err(e) = connect::enter_da(&mut handle) {
        set_error(e.to_string());
        return UMBRAGE_ERR_DA;
    }

    for p in handle.device.partitions() {
        let c_name = to_c_string(p.name.clone());
        let rc = cb(c_name, p.size as u64, p.address as u64, userdata);
        unsafe { free_c_string(c_name) };
        if rc != 0 {
            break;
        }
    }

    UMBRAGE_OK
}

// ---------------------------------------------------------------------------
// Read / write partition
// ---------------------------------------------------------------------------

/// Read a partition to a caller-provided buffer.
///
/// If `out` is NULL, the function sets `*out_len` to the partition size
/// without copying data (useful for buffer allocation).
/// If `out` is non-NULL, data is copied and `*out_len` is updated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_read_partition(
    da: *const c_char,
    auth: *const c_char,
    preloader: *const c_char,
    wait_secs: u64,
    partition: *const c_char,
    out: *mut u8,
    out_len: *mut usize,
) -> c_int {
    let part_str = match cstr_to_string(partition) {
        Ok(s) => s,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    if out_len.is_null() {
        return UMBRAGE_ERR_INVALID_ARG;
    }

    let opts = match cstr_to_string_opts(da, auth, preloader, wait_secs) {
        Ok(o) => o,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    let mut handle = match connect::connect(&opts) {
        Ok(h) => h,
        Err(e) => {
            set_error(e.to_string());
            return UMBRAGE_ERR_NO_DEVICE;
        }
    };

    if let Err(e) = connect::enter_da(&mut handle) {
        set_error(e.to_string());
        return UMBRAGE_ERR_DA;
    }

    let dev = &mut handle.device;
    let part_info = match dev.get_partition(&part_str) {
        Some(p) => p,
        None => {
            set_error(format!("partition {part_str:?} not found"));
            return UMBRAGE_ERR_NOT_FOUND;
        }
    };

    let size = part_info.size;
    unsafe { *out_len = size };

    if out.is_null() {
        return UMBRAGE_OK;
    }

    let buf_len = unsafe { *out_len };
    if buf_len < size {
        set_error(format!("buffer too small: need {size} bytes, got {buf_len}"));
        return UMBRAGE_ERR_IO;
    }

    let buf = unsafe { slice::from_raw_parts_mut(out, size) };
    if let Err(e) = dev.read_partition(&part_str, buf, silent_progress) {
        set_error(e.to_string());
        return UMBRAGE_ERR_IO;
    }

    UMBRAGE_OK
}

/// Write a partition from a buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_write_partition(
    da: *const c_char,
    auth: *const c_char,
    preloader: *const c_char,
    wait_secs: u64,
    partition: *const c_char,
    data: *const u8,
    data_len: usize,
) -> c_int {
    let part_str = match cstr_to_string(partition) {
        Ok(s) => s,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    if data.is_null() && data_len > 0 {
        return UMBRAGE_ERR_INVALID_ARG;
    }

    let data_slice = unsafe { slice::from_raw_parts(data, data_len) };

    let opts = match cstr_to_string_opts(da, auth, preloader, wait_secs) {
        Ok(o) => o,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    let mut handle = match connect::connect(&opts) {
        Ok(h) => h,
        Err(e) => {
            set_error(e.to_string());
            return UMBRAGE_ERR_NO_DEVICE;
        }
    };

    if let Err(e) = connect::enter_da(&mut handle) {
        set_error(e.to_string());
        return UMBRAGE_ERR_DA;
    }

    if let Err(e) =
        handle.device.write_partition(&part_str, data_len, data_slice, silent_progress)
    {
        set_error(e.to_string());
        return UMBRAGE_ERR_IO;
    }

    UMBRAGE_OK
}

// ---------------------------------------------------------------------------
// Memory peek / poke
// ---------------------------------------------------------------------------

/// Read raw memory at a given address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_peek(
    da: *const c_char,
    auth: *const c_char,
    preloader: *const c_char,
    wait_secs: u64,
    address: u64,
    size: usize,
    out: *mut u8,
    out_len: *mut usize,
) -> c_int {
    if out_len.is_null() {
        return UMBRAGE_ERR_INVALID_ARG;
    }
    unsafe { *out_len = size };

    if out.is_null() {
        return UMBRAGE_OK;
    }

    let opts = match cstr_to_string_opts(da, auth, preloader, wait_secs) {
        Ok(o) => o,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    let mut handle = match connect::connect(&opts) {
        Ok(h) => h,
        Err(e) => {
            set_error(e.to_string());
            return UMBRAGE_ERR_NO_DEVICE;
        }
    };

    if let Err(e) = connect::enter_da(&mut handle) {
        set_error(e.to_string());
        return UMBRAGE_ERR_DA;
    }

    let buf = unsafe { slice::from_raw_parts_mut(out, size) };
    if let Err(e) = handle.device.peek(address, size, buf, silent_progress) {
        set_error(e.to_string());
        return UMBRAGE_ERR_IO;
    }

    UMBRAGE_OK
}

/// Write raw memory at a given address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_poke(
    da: *const c_char,
    auth: *const c_char,
    preloader: *const c_char,
    wait_secs: u64,
    address: u64,
    data: *const u8,
    data_len: usize,
) -> c_int {
    if data.is_null() && data_len > 0 {
        return UMBRAGE_ERR_INVALID_ARG;
    }

    let data_slice = unsafe { slice::from_raw_parts(data, data_len) };

    let opts = match cstr_to_string_opts(da, auth, preloader, wait_secs) {
        Ok(o) => o,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    let mut handle = match connect::connect(&opts) {
        Ok(h) => h,
        Err(e) => {
            set_error(e.to_string());
            return UMBRAGE_ERR_NO_DEVICE;
        }
    };

    if let Err(e) = connect::enter_da(&mut handle) {
        set_error(e.to_string());
        return UMBRAGE_ERR_DA;
    }

    if let Err(e) = handle.device.poke(address, data_len, data_slice, silent_progress) {
        set_error(e.to_string());
        return UMBRAGE_ERR_IO;
    }

    UMBRAGE_OK
}

// ---------------------------------------------------------------------------
// Register access
// ---------------------------------------------------------------------------

/// Read a 32-bit register at `address`.  Result is stored in `*value`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_read_reg(
    da: *const c_char,
    auth: *const c_char,
    preloader: *const c_char,
    wait_secs: u64,
    address: u64,
    value: *mut u32,
) -> c_int {
    if value.is_null() {
        return UMBRAGE_ERR_INVALID_ARG;
    }

    let opts = match cstr_to_string_opts(da, auth, preloader, wait_secs) {
        Ok(o) => o,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    let mut handle = match connect::connect(&opts) {
        Ok(h) => h,
        Err(e) => {
            set_error(e.to_string());
            return UMBRAGE_ERR_NO_DEVICE;
        }
    };

    if let Err(e) = connect::enter_da(&mut handle) {
        set_error(e.to_string());
        return UMBRAGE_ERR_DA;
    }

    match handle.device.read_register(address) {
        Ok(v) => {
            unsafe { *value = v };
            UMBRAGE_OK
        }
        Err(e) => {
            set_error(e.to_string());
            UMBRAGE_ERR_IO
        }
    }
}

/// Write a 32-bit register at `address` to `value`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_write_reg(
    da: *const c_char,
    auth: *const c_char,
    preloader: *const c_char,
    wait_secs: u64,
    address: u64,
    value: u32,
) -> c_int {
    let opts = match cstr_to_string_opts(da, auth, preloader, wait_secs) {
        Ok(o) => o,
        Err(e) => {
            set_error(e);
            return UMBRAGE_ERR_INVALID_ARG;
        }
    };

    let mut handle = match connect::connect(&opts) {
        Ok(h) => h,
        Err(e) => {
            set_error(e.to_string());
            return UMBRAGE_ERR_NO_DEVICE;
        }
    };

    if let Err(e) = connect::enter_da(&mut handle) {
        set_error(e.to_string());
        return UMBRAGE_ERR_DA;
    }

    if let Err(e) = handle.device.write_register(address, value) {
        set_error(e.to_string());
        return UMBRAGE_ERR_IO;
    }

    UMBRAGE_OK
}

// ---------------------------------------------------------------------------
// Utility: free a string
// ---------------------------------------------------------------------------

/// Frees a string allocated by `umbrage_last_error`, `umbrage_version`, or
/// partition info fields.  Passing NULL is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn umbrage_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    // Reconstruct the CString and drop it.
    unsafe { drop(CString::from_raw(s)) };
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Parse a single C string pointer to an Option<PathBuf>, returning None for
/// NULL or empty strings.
unsafe fn opt_path(p: *const c_char) -> Option<std::path::PathBuf> {
    if p.is_null() {
        return None;
    }
    let cstr = unsafe { CStr::from_ptr(p) };
    let s = cstr.to_str().ok()?;
    if s.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(s))
}

/// Build `ConnectOpts` from raw C strings (for the native library API).
///
/// Empty or NULL strings are treated as "not provided". `wait_secs` of 0
/// means a short default wait (the same as the CLI default of 15 seconds).
unsafe fn cstr_to_string_opts(
    da: *const c_char,
    auth: *const c_char,
    preloader: *const c_char,
    wait_secs: u64,
) -> Result<ConnectOpts, String> {
    let opts = unsafe { connect::parse_connect_opts(da, auth, preloader, wait_secs) };
    opts.map_err(|e| e.to_string())
}

/// Convert a C string pointer to a Rust String, returning an error on null or
/// invalid UTF-8.
unsafe fn cstr_to_string(cstr: *const c_char) -> Result<String, String> {
    if cstr.is_null() {
        return Err("null pointer".to_string());
    }
    let cstr = unsafe { CStr::from_ptr(cstr) };
    let s = cstr.to_str().map_err(|_| "invalid UTF-8")?;
    Ok(s.to_string())
}

fn to_c_string(s: String) -> *mut c_char {
    if s.is_empty() {
        return std::ptr::null_mut();
    }
    CString::new(s).unwrap().into_raw()
}

unsafe fn free_c_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}
