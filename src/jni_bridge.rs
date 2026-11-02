//! JNI bridge for Android integration.
//!
//! This module provides the FFI entry points for integrating umbrage-cli
//! with Android applications (e.g., Adbify). It implements the
//! `AndroidUsbMTKPort` that implements penumbra_mtk's `MtkPort` trait using
//! JNI calls to the Android USB Host API.
//!
//! The Java/Kotlin side needs to:
//! 1. Find MTK USB devices via `UsbManager`
//! 2. Open a `UsbDeviceConnection` and claim interfaces
//! 3. Find bulk IN/OUT endpoints
//! 4. Call `Native.init()` then `Native.openDevice(connection, epIn, epOut, connType, vid, pid)`
//! 5. Use the returned handle for subsequent operations

#[cfg(target_os = "android")]
mod android {
    #![allow(unused_imports)]

    use jni::objects::{GlobalRef, JClass, JObject, JValue};
    use jni::sys::{jint, jlong, jstring};
    use jni::{JNIEnv, JavaVM};
    use once_cell::sync::OnceCell;
    use penumbra_mtk::error::{ConnectionError, Error as PenumbraError, Result as PenumbraResult};
    use penumbra_mtk::port::{ConnectionType, MtkPort};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    static JVM: OnceCell<JavaVM> = OnceCell::new();

    pub fn set_vm(vm: JavaVM) {
        let _ = JVM.set(vm);
    }

    fn with_env<F, R>(f: F) -> PenumbraResult<R>
    where
        F: FnOnce(&mut JNIEnv) -> PenumbraResult<R>,
    {
        let vm = JVM.get().ok_or_else(|| {
            PenumbraError::Connection(ConnectionError::OpenFailed("JavaVM not set".into()))
        })?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| PenumbraError::Connection(ConnectionError::OpenFailed(format!("attach: {e}"))))?;
        f(&mut env)
    }

    // ---------------------------------------------------------------------------
    // Session registry
    // ---------------------------------------------------------------------------

    /// A session holds the port state for an open USB device.
    pub struct JniSession {
        pub port: AndroidUsbMTKPort,
    }

    static SESSIONS: Mutex<Vec<Option<Arc<Mutex<JniSession>>>>> = Mutex::new(Vec::new());

    fn create_session(port: AndroidUsbMTKPort) -> jlong {
        let session = Arc::new(Mutex::new(JniSession { port }));
        let mut sessions = SESSIONS.lock().unwrap();
        let handle: jlong = sessions.len() as i64;
        sessions.push(Some(session));
        handle
    }

    fn get_session(handle: jlong) -> Option<Arc<Mutex<JniSession>>> {
        if handle < 0 {
            return None;
        }
        let sessions = SESSIONS.lock().ok()?;
        sessions.get(handle as usize)?.clone()
    }

    fn remove_session(handle: jlong) -> bool {
        if handle < 0 {
            return false;
        }
        let mut sessions = SESSIONS.lock().unwrap();
        if handle as usize >= sessions.len() {
            return false;
        }
        sessions[handle as usize] = None;
        true
    }

    // ---------------------------------------------------------------------------
    // AndroidUsbMTKPort — implements MtkPort via JNI to UsbDeviceConnection
    // ---------------------------------------------------------------------------

    /// Android USB Host backend implementing penumbra_mtk::port::MtkPort.
    ///
    /// Uses JNI calls to `UsbDeviceConnection.bulkTransfer()` and
    /// `UsbDeviceConnection.controlTransfer()` for USB communication.
    pub struct AndroidUsbMTKPort {
        connection: GlobalRef,
        ep_in: GlobalRef,
        ep_out: GlobalRef,
        timeout: Duration,
        connection_type: ConnectionType,
        port_name: String,
        is_open: bool,
        read_buf: VecDeque<u8>,
        temp_buf: Vec<u8>,
    }

    impl AndroidUsbMTKPort {
        /// Create a new port from Java-side USB objects.
        pub fn from_java(
            env: &mut JNIEnv,
            connection: JObject,
            ep_in: JObject,
            ep_out: JObject,
            conn_type: ConnectionType,
            vid: u16,
            pid: u16,
        ) -> PenumbraResult<Self> {
            let connection = env
                .new_global_ref(connection)
                .map_err(|e| PenumbraError::Connection(ConnectionError::OpenFailed(format!("ref: {e}"))))?;
            let ep_in = env
                .new_global_ref(ep_in)
                .map_err(|e| PenumbraError::Connection(ConnectionError::OpenFailed(format!("ref: {e}"))))?;
            let ep_out = env
                .new_global_ref(ep_out)
                .map_err(|e| PenumbraError::Connection(ConnectionError::OpenFailed(format!("ref: {e}"))))?;

            Ok(Self {
                connection,
                ep_in,
                ep_out,
                timeout: Duration::from_millis(1000),
                connection_type: conn_type,
                port_name: format!("USB {:04X}:{:04X} (android)", vid, pid),
                is_open: true,
                read_buf: VecDeque::new(),
                temp_buf: vec![0u8; 0x80000],
            })
        }

        /// CDC line coding setup for Preloader/DA mode.
        pub fn setup_cdc(&mut self) -> PenumbraResult<()> {
            const CDC_INTERFACE: u16 = 1;
            const SET_LINE_CODING: u8 = 0x20;
            const SET_CONTROL_LINE_STATE: u8 = 0x22;
            const LINE_CODING: [u8; 7] = [0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x08];
            let request_type = 0x21u8;
            self.ctrl_out(request_type, SET_LINE_CODING, 0, CDC_INTERFACE, &LINE_CODING)?;
            self.ctrl_out(request_type, SET_CONTROL_LINE_STATE, 0x03, CDC_INTERFACE, &[])?;
            Ok(())
        }

        /// Perform a bulk transfer via JNI.
        fn bulk_inner(
            env: &mut JNIEnv,
            connection: &GlobalRef,
            timeout: Duration,
            endpoint: &GlobalRef,
            buf: &mut [u8],
            length: i32,
        ) -> PenumbraResult<i32> {
            let java_buf = env
                .byte_array_from_slice(&buf[..length as usize])
                .map_err(|e| PenumbraError::Io(std::io::Error::other(e.to_string())))?;

            let result = env
                .call_method(
                    connection,
                    "bulkTransfer",
                    "(Landroid/hardware/usb/UsbEndpoint;[BII)I",
                    &[
                        JValue::Object(endpoint.as_obj()),
                        JValue::Object(&java_buf),
                        JValue::Int(length),
                        JValue::Int(timeout.as_millis().min(i32::MAX as u128) as i32),
                    ],
                )
                .map_err(|e| PenumbraError::Io(std::io::Error::other(e.to_string())))?
                .i()
                .map_err(|e| PenumbraError::Io(std::io::Error::other(e.to_string())))?;

            if result < 0 {
                return Err(PenumbraError::Timeout);
            }
            if result > 0 {
                let bytes = env
                    .convert_byte_array(&java_buf)
                    .map_err(|e| PenumbraError::Io(std::io::Error::other(e.to_string())))?;
                let n = result as usize;
                buf[..n].copy_from_slice(&bytes[..n]);
            }
            Ok(result)
        }
    }

    impl MtkPort for AndroidUsbMTKPort {
        fn open(&mut self) -> PenumbraResult<()> {
            self.is_open = true;
            if self.connection_type != ConnectionType::Brom {
                let _ = self.setup_cdc();
            }
            Ok(())
        }

        fn close(&mut self) -> PenumbraResult<()> {
            self.is_open = false;
            Ok(())
        }

        fn reenumerate(&mut self, _vid: u16, _pid: u16) -> PenumbraResult<()> {
            Err(PenumbraError::Connection(ConnectionError::OpenFailed(
                "reenumerate unsupported — unplug/replug or wait for new USB device".into(),
            )))
        }

        fn read_exact(&mut self, buf: &mut [u8]) -> PenumbraResult<usize> {
            if !self.is_open {
                return Err(PenumbraError::Connection(ConnectionError::OpenFailed(
                    "not open".into(),
                )));
            }
            with_env(|env: &mut JNIEnv| {
                let mut total = 0usize;
                while total < buf.len() && !self.read_buf.is_empty() {
                    buf[total] = self.read_buf.pop_front().unwrap();
                    total += 1;
                }
                while total < buf.len() {
                    let needed = buf.len() - total;
                    let chunk = needed.min(self.temp_buf.len());
                    let n = Self::bulk_inner(
                        env,
                        &self.connection,
                        self.timeout,
                        &self.ep_in,
                        &mut self.temp_buf[..chunk],
                        chunk as i32,
                    )?;
                    if n == 0 {
                        continue;
                    }
                    let n = n as usize;
                    let copy = n.min(needed);
                    buf[total..total + copy].copy_from_slice(&self.temp_buf[..copy]);
                    total += copy;
                    if copy < n {
                        self.read_buf.extend(&self.temp_buf[copy..n]);
                    }
                }
                Ok(total)
            })
        }

        fn write_all(&mut self, buf: &[u8]) -> PenumbraResult<()> {
            if !self.is_open {
                return Err(PenumbraError::Connection(ConnectionError::OpenFailed(
                    "not open".into(),
                )));
            }
            with_env(|env: &mut JNIEnv| {
                let mut written = 0;
                let mut tmp = buf.to_vec();
                while written < buf.len() {
                    let n = Self::bulk_inner(
                        env,
                        &self.connection,
                        self.timeout,
                        &self.ep_out,
                        &mut tmp[written..],
                        (buf.len() - written) as i32,
                    )?;
                    if n <= 0 {
                        return Err(PenumbraError::Timeout);
                    }
                    written += n as usize;
                }
                Ok(())
            })
        }

        fn flush(&mut self) -> PenumbraResult<()> {
            Ok(())
        }

        fn get_baudrate(&self) -> u32 {
            0
        }

        fn get_port_name(&self) -> String {
            self.port_name.clone()
        }

        fn set_timeout(&mut self, timeout: Duration) -> PenumbraResult<()> {
            self.timeout = timeout;
            Ok(())
        }

        fn get_timeout(&self) -> Duration {
            self.timeout
        }

        fn connection_type(&self) -> ConnectionType {
            self.connection_type
        }

        fn set_connection_type(&mut self, t: ConnectionType) -> PenumbraResult<()> {
            self.connection_type = t;
            Ok(())
        }

        fn ctrl_out(
            &mut self,
            request_type: u8,
            request: u8,
            value: u16,
            index: u16,
            data: &[u8],
        ) -> PenumbraResult<()> {
            with_env(|env: &mut JNIEnv| {
                let java_buf = env
                    .byte_array_from_slice(data)
                    .map_err(|e| PenumbraError::Io(std::io::Error::other(e.to_string())))?;

                let result = env
                    .call_method(
                        &self.connection,
                        "controlTransfer",
                        "(IIII[BII)I",
                        &[
                            JValue::Int(request_type as i32),
                            JValue::Int(request as i32),
                            JValue::Int(value as i32),
                            JValue::Int(index as i32),
                            JValue::Object(&java_buf),
                            JValue::Int(data.len() as i32),
                            JValue::Int(self.timeout.as_millis().min(i32::MAX as u128) as i32),
                        ],
                    )
                    .map_err(|_| PenumbraError::Connection(ConnectionError::CtrlTransferOutFailed))?
                    .i()
                    .map_err(|_| PenumbraError::Connection(ConnectionError::CtrlTransferOutFailed))?;

                if result < 0 {
                    return Err(PenumbraError::Connection(ConnectionError::CtrlTransferOutFailed));
                }
                Ok(())
            })
        }

        fn ctrl_in(
            &mut self,
            request_type: u8,
            request: u8,
            value: u16,
            index: u16,
            len: usize,
        ) -> PenumbraResult<Vec<u8>> {
            with_env(|env: &mut JNIEnv| {
                let mut buf = vec![0u8; len];
                let java_buf = env
                    .byte_array_from_slice(&buf)
                    .map_err(|e| PenumbraError::Io(std::io::Error::other(e.to_string())))?;

                let result = env
                    .call_method(
                        &self.connection,
                        "controlTransfer",
                        "(IIII[BII)I",
                        &[
                            JValue::Int(request_type as i32),
                            JValue::Int(request as i32),
                            JValue::Int(value as i32),
                            JValue::Int(index as i32),
                            JValue::Object(&java_buf),
                            JValue::Int(len as i32),
                            JValue::Int(self.timeout.as_millis().min(i32::MAX as u128) as i32),
                        ],
                    )
                    .map_err(|_| PenumbraError::Connection(ConnectionError::CtrlTransferInFailed))?
                    .i()
                    .map_err(|_| PenumbraError::Connection(ConnectionError::CtrlTransferInFailed))?;

                if result < 0 {
                    return Err(PenumbraError::Connection(ConnectionError::CtrlTransferInFailed));
                }

                let bytes = env
                    .convert_byte_array(&java_buf)
                    .map_err(|e| PenumbraError::Io(std::io::Error::other(e.to_string())))?;
                let n = result as usize;
                buf[..n].copy_from_slice(&bytes[..n]);
                buf.truncate(n);
                Ok(buf)
            })
        }
    }

    // ---------------------------------------------------------------------------
    // JNI entry points
    // ---------------------------------------------------------------------------

    /// Initialize logging and store the JavaVM reference.
    #[unsafe(no_mangle)]
    #[allow(non_snake_case)]
    pub unsafe extern "system" fn Java_com_adbify_Native_init(
        env: JNIEnv,
        _: JClass,
    ) {
        if let Ok(vm) = env.get_java_vm() {
            set_vm(vm);
        }
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Debug)
                .with_tag("UmbrageNative"),
        );
        log::info!("umbrage native ready");
    }

    /// Open a device session from Java-side USB objects.
    ///
    /// Returns a session handle (>= 0) on success, or a negative error code.
    #[unsafe(no_mangle)]
    #[allow(non_snake_case)]
    pub unsafe extern "system" fn Java_com_adbify_Native_openDevice(
        mut env: JNIEnv,
        _: JClass,
        connection: JObject,
        ep_in: JObject,
        ep_out: JObject,
        conn_type: jint,
        vid: jint,
        pid: jint,
    ) -> jlong {
        let ct = match conn_type {
            0 => ConnectionType::Brom,
            1 => ConnectionType::Preloader,
            2 => ConnectionType::Da,
            _ => {
                log::error!("invalid conn_type: {conn_type}");
                return -1;
            }
        };

        let port = match with_env(|env: &mut JNIEnv| {
            AndroidUsbMTKPort::from_java(env, connection, ep_in, ep_out, ct, vid as u16, pid as u16)
        }) {
            Ok(p) => p,
            Err(e) => {
                log::error!("open port: {e}");
                return -2;
            }
        };

        let handle = create_session(port);
        log::info!("opened device {vid:04X}:{pid:04X} handle={handle}");
        handle
    }

    /// Close a device session.
    #[unsafe(no_mangle)]
    #[allow(non_snake_case)]
    pub unsafe extern "system" fn Java_com_adbify_Native_closeDevice(
        _env: JNIEnv,
        _: JClass,
        handle: jlong,
    ) {
        if remove_session(handle) {
            log::info!("closed session {handle}");
        }
    }

    /// Get the port name for a session.
    #[unsafe(no_mangle)]
    #[allow(non_snake_case)]
    pub unsafe extern "system" fn Java_com_adbify_Native_getPortName(
        mut env: JNIEnv,
        _: JClass,
        handle: jlong,
    ) -> jstring {
        let sessions = SESSIONS.lock().unwrap();
        if handle < 0 || handle as usize >= sessions.len() {
            return std::ptr::null_mut();
        }
        let name = match &sessions[handle as usize] {
            Some(s) => {
                if let Ok(s) = s.lock() {
                    s.port.get_port_name()
                } else {
                    return std::ptr::null_mut();
                }
            }
            None => return std::ptr::null_mut(),
        };
        env.new_string(&name)
            .map(|jstr| jstr.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }

    /// Get the connection type for a session.
    #[unsafe(no_mangle)]
    #[allow(non_snake_case)]
    pub unsafe extern "system" fn Java_com_adbify_Native_getConnectionType(
        _env: JNIEnv,
        _: JClass,
        handle: jlong,
    ) -> jint {
        if let Some(session) = get_session(handle) {
            if let Ok(s) = session.lock() {
                return match s.port.connection_type() {
                    ConnectionType::Brom => 0,
                    ConnectionType::Preloader => 1,
                    ConnectionType::Da => 2,
                };
            }
        }
        -1
    }

    /// Get the last error message.
    #[unsafe(no_mangle)]
    #[allow(non_snake_case)]
    pub unsafe extern "system" fn Java_com_adbify_Native_getLastError(
        mut env: JNIEnv,
        _: JClass,
        handle: jlong,
    ) -> jstring {
        let _ = handle;
        std::ptr::null_mut()
    }
}

#[cfg(target_os = "android")]
pub use android::*;

/// Sets the Java VM for Android USB Host communication.
/// Must be called from `JNI_OnLoad` or from Java native methods.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn JNI_OnLoad(
    vm: jni::JavaVM,
    _: *mut std::ffi::c_void,
) -> jni::sys::jint {
    use jni::sys::JNI_VERSION_1_6;
    set_vm_global(vm);
    JNI_VERSION_1_6 as jint
}