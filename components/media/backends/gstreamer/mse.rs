/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions on `libkyousei-mse` (kyousei-gstmse: a fork of GStreamer's `gstmse`
//! of gst-plugins-bad 1.26.2, with its bugs fixed). The library parses the appended segments,
//! keeps the coded frames of each source buffer, answers the buffered ranges and feeds
//! `kyouseimsesrc` (the `kyousei-mse://` source of `playbin3`). The few functions used here are
//! declared below.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int};
use std::ops::Range;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use glib::prelude::*;
use glib::translate::*;
use servo_base::generic_channel::GenericCallback;
use servo_media_player::{MediaSourceEndOfStreamError, PlayerError, PlayerEvent};

type GstObject = gstreamer::ffi::GstObject;
type GError = glib::ffi::GError;
type GstClockTime = u64;

const GST_CLOCK_TIME_NONE: GstClockTime = u64::MAX;
const GST_SOURCE_BUFFER_APPEND_MODE_SEGMENTS: c_int = 0;
const GST_SOURCE_BUFFER_APPEND_MODE_SEQUENCE: c_int = 1;
const GST_MEDIA_SOURCE_EOS_ERROR_NONE: c_int = 0;
const GST_MEDIA_SOURCE_EOS_ERROR_NETWORK: c_int = 1;
const GST_MEDIA_SOURCE_EOS_ERROR_DECODE: c_int = 2;
/// `KY_MEDIA_SOURCE_ERROR_QUOTA_EXCEEDED` (150MiB per video source buffer, 12MiB otherwise)
const GST_MEDIA_SOURCE_ERROR_QUOTA_EXCEEDED: c_int = 4;

/// `GstMediaSourceRange` (and `GstSourceBufferInterval`).
#[repr(C)]
struct GstMediaSourceRange {
    start: GstClockTime,
    end: GstClockTime,
}

/// The ABI version of `libkyousei-mse` this code is written for (`KY_MSE_ABI_VERSION`).
const ABI_VERSION: std::ffi::c_uint = 1;

/// The functions of `libkyousei-mse-1.0`, found at run time: the library ships with Kyousei OS
/// (next to the executable, in `../lib/`), and a machine without it plays no MSE (see
/// `is_available()`).
struct Api {
    ky_mse_abi_version: unsafe extern "C" fn() -> std::ffi::c_uint,
    ky_mse_register: unsafe extern "C" fn() -> glib::ffi::gboolean,
    ky_media_source_get_type: unsafe extern "C" fn() -> glib::ffi::GType,
    ky_media_source_error_quark: unsafe extern "C" fn() -> glib::ffi::GQuark,
    ky_source_buffer_get_type: unsafe extern "C" fn() -> glib::ffi::GType,
    ky_media_source_new: unsafe extern "C" fn() -> *mut GstObject,
    ky_media_source_is_type_supported:
        unsafe extern "C" fn(type_: *const c_char) -> glib::ffi::gboolean,
    ky_media_source_attach: unsafe extern "C" fn(
        media_source: *mut GstObject,
        element: *mut gstreamer::ffi::GstElement,
    ),
    ky_media_source_detach: unsafe extern "C" fn(media_source: *mut GstObject),
    ky_media_source_add_source_buffer: unsafe extern "C" fn(
        media_source: *mut GstObject,
        type_: *const c_char,
        error: *mut *mut GError,
    ) -> *mut GstObject,
    ky_media_source_remove_source_buffer: unsafe extern "C" fn(
        media_source: *mut GstObject,
        buffer: *mut GstObject,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_media_source_set_duration: unsafe extern "C" fn(
        media_source: *mut GstObject,
        duration: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_media_source_end_of_stream: unsafe extern "C" fn(
        media_source: *mut GstObject,
        eos_error: c_int,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_source_buffer_append_buffer: unsafe extern "C" fn(
        buffer: *mut GstObject,
        buf: *mut gstreamer::ffi::GstBuffer,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_source_buffer_abort: unsafe extern "C" fn(
        buffer: *mut GstObject,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_source_buffer_remove: unsafe extern "C" fn(
        buffer: *mut GstObject,
        start: GstClockTime,
        end: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_source_buffer_get_buffered: unsafe extern "C" fn(
        buffer: *mut GstObject,
        error: *mut *mut GError,
    ) -> *mut glib::ffi::GArray,
    ky_source_buffer_set_timestamp_offset: unsafe extern "C" fn(
        buffer: *mut GstObject,
        offset: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_source_buffer_set_append_window_start: unsafe extern "C" fn(
        buffer: *mut GstObject,
        start: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_source_buffer_set_append_window_end: unsafe extern "C" fn(
        buffer: *mut GstObject,
        end: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_source_buffer_set_append_mode: unsafe extern "C" fn(
        buffer: *mut GstObject,
        mode: c_int,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
    ky_source_buffer_change_content_type: unsafe extern "C" fn(
        buffer: *mut GstObject,
        type_: *const c_char,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean,
}

/// Looks up `name` in the library `handle`.
///
/// # Safety
/// `T` must be the function pointer type of the symbol.
unsafe fn symbol<T>(handle: *mut std::ffi::c_void, name: &std::ffi::CStr) -> Option<T> {
    // SAFETY: a valid handle and NUL-terminated name.
    let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if address.is_null() {
        log::warn!("MSE: libkyousei-mse has no {name:?}");
        return None;
    }
    // SAFETY: the caller gives the type of the symbol; a function pointer has the size of a
    // pointer.
    Some(unsafe { std::mem::transmute_copy::<*mut std::ffi::c_void, T>(&address) })
}

impl Api {
    /// Opens the library (`../lib/` next to the executable first, then the usual search
    /// path) and checks its ABI version.
    fn load() -> Option<Self> {
        const NAME: &str = "libkyousei-mse-1.0.so.0";
        let beside = std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.parent()?.join("lib").join(NAME)))
            .filter(|path| path.exists())
            .and_then(|path| CString::new(path.into_os_string().into_encoded_bytes()).ok());
        let candidates = beside.into_iter().chain(CString::new(NAME).ok());
        let handle = candidates.into_iter().find_map(|path| {
            // SAFETY: a valid NUL-terminated path.
            let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            (!handle.is_null()).then_some(handle)
        });
        let Some(handle) = handle else {
            log::info!("MSE: {NAME} is not installed; MediaSource is off");
            return None;
        };
        // SAFETY: the types are those of the library's header (`kyousei-mse/*.h`).
        let api = unsafe {
            Self {
                ky_mse_abi_version: symbol(handle, c"ky_mse_abi_version")?,
                ky_mse_register: symbol(handle, c"ky_mse_register")?,
                ky_media_source_get_type: symbol(handle, c"ky_media_source_get_type")?,
                ky_media_source_error_quark: symbol(handle, c"ky_media_source_error_quark")?,
                ky_source_buffer_get_type: symbol(handle, c"ky_source_buffer_get_type")?,
                ky_media_source_new: symbol(handle, c"ky_media_source_new")?,
                ky_media_source_is_type_supported: symbol(
                    handle,
                    c"ky_media_source_is_type_supported",
                )?,
                ky_media_source_attach: symbol(handle, c"ky_media_source_attach")?,
                ky_media_source_detach: symbol(handle, c"ky_media_source_detach")?,
                ky_media_source_add_source_buffer: symbol(
                    handle,
                    c"ky_media_source_add_source_buffer",
                )?,
                ky_media_source_remove_source_buffer: symbol(
                    handle,
                    c"ky_media_source_remove_source_buffer",
                )?,
                ky_media_source_set_duration: symbol(handle, c"ky_media_source_set_duration")?,
                ky_media_source_end_of_stream: symbol(handle, c"ky_media_source_end_of_stream")?,
                ky_source_buffer_append_buffer: symbol(handle, c"ky_source_buffer_append_buffer")?,
                ky_source_buffer_abort: symbol(handle, c"ky_source_buffer_abort")?,
                ky_source_buffer_remove: symbol(handle, c"ky_source_buffer_remove")?,
                ky_source_buffer_get_buffered: symbol(handle, c"ky_source_buffer_get_buffered")?,
                ky_source_buffer_set_timestamp_offset: symbol(
                    handle,
                    c"ky_source_buffer_set_timestamp_offset",
                )?,
                ky_source_buffer_set_append_window_start: symbol(
                    handle,
                    c"ky_source_buffer_set_append_window_start",
                )?,
                ky_source_buffer_set_append_window_end: symbol(
                    handle,
                    c"ky_source_buffer_set_append_window_end",
                )?,
                ky_source_buffer_set_append_mode: symbol(
                    handle,
                    c"ky_source_buffer_set_append_mode",
                )?,
                ky_source_buffer_change_content_type: symbol(
                    handle,
                    c"ky_source_buffer_change_content_type",
                )?,
            }
        };
        // SAFETY: a function of the library, without arguments.
        let version = unsafe { (api.ky_mse_abi_version)() };
        if version != ABI_VERSION {
            log::warn!(
                "MSE: libkyousei-mse has ABI {version}, {ABI_VERSION} is needed; MediaSource is off"
            );
            return None;
        }
        Some(api)
    }
}

/// The library, when it is installed and of the right version.
fn try_api() -> Option<&'static Api> {
    static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();
    API.get_or_init(Api::load).as_ref()
}

/// The library. Only reached when MediaSource is on, which needs the library (`is_available()`).
fn api() -> &'static Api {
    try_api().expect("MediaSource is used without libkyousei-mse")
}

/// Whether MediaSource can work: `libkyousei-mse` is installed and of the right version. Servo
/// turns MediaSource off when it is not.
pub fn is_available() -> bool {
    try_api().is_some()
}

/// Takes the reference a gstmse constructor returned (sinking it when it is floating).
///
/// # Safety
/// `object` must be a new reference to a GObject.
unsafe fn take_object(object: *mut GstObject) -> gstreamer::Object {
    // SAFETY: the caller passes a new reference.
    unsafe {
        if glib::gobject_ffi::g_object_is_floating(object as *mut _) != glib::ffi::GFALSE {
            glib::gobject_ffi::g_object_ref_sink(object as *mut _);
        }
        from_glib_full(object)
    }
}

/// Runs a gstmse call that reports failure through a `GError`.
fn check(call: impl FnOnce(*mut *mut GError) -> glib::ffi::gboolean) -> Result<(), PlayerError> {
    let mut error: *mut GError = ptr::null_mut();
    if call(&mut error) != glib::ffi::GFALSE {
        return Ok(());
    }
    // SAFETY: a failed call set `error` (or left it NULL).
    if !error.is_null() &&
        unsafe {
            (*error).domain == (api().ky_media_source_error_quark)() &&
                (*error).code == GST_MEDIA_SOURCE_ERROR_QUOTA_EXCEEDED
        }
    {
        // SAFETY: we own the error.
        unsafe { glib::ffi::g_error_free(error) };
        return Err(PlayerError::MediaSourceQuotaExceeded);
    }
    Err(PlayerError::MediaSource(error_message(error)))
}

fn error_message(error: *mut GError) -> String {
    if error.is_null() {
        return "unknown error".to_owned();
    }
    // SAFETY: a gstmse call that failed gave us the ownership of `error`.
    let error: glib::Error = unsafe { from_glib_full(error) };
    error.message().to_owned()
}

fn clock_time(seconds: f64) -> GstClockTime {
    if seconds.is_infinite() && seconds > 0. {
        return GST_CLOCK_TIME_NONE;
    }
    (seconds.max(0.) * 1_000_000_000.) as GstClockTime
}

fn seconds(time: GstClockTime) -> f64 {
    if time == GST_CLOCK_TIME_NONE {
        return f64::INFINITY;
    }
    time as f64 / 1_000_000_000.
}

/// Registers `kyouseimsesrc` (the `kyousei-mse://` source of `playbin3`) once.
fn register() {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    // SAFETY: called after gst_init() (the backend initializes GStreamer first).
    REGISTER.call_once(|| {
        if unsafe { (api().ky_mse_register)() } == glib::ffi::GFALSE {
            log::warn!("MSE: could not register kyouseimsesrc");
        }
    });
}

/// Whether the system has the gstmse library and a demuxer and decoders for this type.
pub fn is_type_supported(mime_type: &str) -> bool {
    let Ok(mime_type) = CString::new(mime_type) else {
        return false;
    };
    if !is_available() {
        return false;
    }
    register();
    // SAFETY: a valid NUL-terminated string.
    unsafe { (api().ky_media_source_is_type_supported)(mime_type.as_ptr()) != glib::ffi::GFALSE }
}

/// The state the signal handlers of a gstmse source buffer share with us.
#[derive(Default)]
struct SignalState {
    /// An append was given to gstmse and its end is not reported yet.
    outstanding: AtomicBool,
    /// Set by `on-error`, read and cleared by the `on-update-end` that follows.
    errored: AtomicBool,
}

struct SourceBuffer {
    object: gstreamer::Object,
    signal_state: Arc<SignalState>,
}

/// One `MediaSource` of a player. The GObjects are thread safe. The signal handlers never take
/// the lock of `source_buffers`, so gstmse may be called with it held.
pub struct MediaSource {
    object: gstreamer::Object,
    source_buffers: Mutex<HashMap<usize, SourceBuffer>>,
    next_id: Mutex<usize>,
    observer: Arc<Mutex<GenericCallback<PlayerEvent>>>,
}

impl MediaSource {
    pub fn new(observer: Arc<Mutex<GenericCallback<PlayerEvent>>>) -> Self {
        register();
        // SAFETY: the constructor returns a new reference.
        let object = unsafe {
            let _ = (api().ky_media_source_get_type)();
            let _ = (api().ky_source_buffer_get_type)();
            take_object((api().ky_media_source_new)())
        };
        let observer_ = observer.clone();
        object.connect("on-source-open", false, move |_| {
            let _ = observer_.lock().unwrap().send(PlayerEvent::MediaSourceOpen);
            None
        });
        Self {
            object,
            source_buffers: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
            observer,
        }
    }

    fn ptr(&self) -> *mut GstObject {
        self.object.as_ptr() as *mut GstObject
    }

    /// Attaches the media source to the `kyouseimsesrc` element `playbin3` made for
    /// `kyousei-mse://`.
    pub fn attach(&self, element: &gstreamer::Element) {
        // SAFETY: both objects are alive; the library checks the type of the element.
        unsafe { (api().ky_media_source_attach)(self.ptr(), element.as_ptr()) }
    }

    pub fn detach(&self) {
        // SAFETY: the object is alive.
        unsafe { (api().ky_media_source_detach)(self.ptr()) }
    }

    pub fn add_source_buffer(&self, mime_type: &str) -> Result<usize, PlayerError> {
        let mime_type =
            CString::new(mime_type).map_err(|_| PlayerError::MediaSource("bad type".to_owned()))?;
        let id = {
            let mut next_id = self.next_id.lock().unwrap();
            *next_id += 1;
            *next_id
        };
        let mut error: *mut GError = ptr::null_mut();
        // SAFETY: valid arguments; the result is a new reference or NULL with `error` set.
        let object = unsafe {
            (api().ky_media_source_add_source_buffer)(self.ptr(), mime_type.as_ptr(), &mut error)
        };
        if object.is_null() {
            return Err(PlayerError::MediaSource(error_message(error)));
        }
        // SAFETY: we own the reference returned above.
        let object = unsafe { take_object(object) };

        let signal_state = Arc::new(SignalState::default());
        let state = signal_state.clone();
        object.connect("on-error", false, move |_| {
            if state.outstanding.load(Ordering::SeqCst) {
                state.errored.store(true, Ordering::SeqCst);
            }
            None
        });
        // The library ends an append once its frames are in the source buffer. The events come
        // from another thread; an end counts only when we wait for one.
        let state = signal_state.clone();
        let observer = self.observer.clone();
        object.connect("on-update-end", false, move |_| {
            if !state.outstanding.swap(false, Ordering::SeqCst) {
                return None;
            }
            let error = state.errored.swap(false, Ordering::SeqCst);
            let _ = observer
                .lock()
                .unwrap()
                .send(PlayerEvent::SourceBufferUpdateEnd { id, error });
            None
        });

        self.source_buffers.lock().unwrap().insert(
            id,
            SourceBuffer {
                object,
                signal_state,
            },
        );
        Ok(id)
    }

    fn with_buffer<T>(
        &self,
        id: usize,
        f: impl FnOnce(*mut GstObject) -> Result<T, PlayerError>,
    ) -> Result<T, PlayerError> {
        let object = self
            .source_buffers
            .lock()
            .unwrap()
            .get(&id)
            .map(|buffer| buffer.object.clone())
            .ok_or_else(|| PlayerError::MediaSource("no such source buffer".to_owned()))?;
        f(object.as_ptr() as *mut GstObject)
    }

    pub fn remove_source_buffer(&self, id: usize) -> Result<(), PlayerError> {
        let Some(buffer) = self.source_buffers.lock().unwrap().remove(&id) else {
            return Ok(());
        };
        let object = buffer.object.as_ptr() as *mut GstObject;
        // SAFETY: both objects are alive.
        check(|error| unsafe {
            (api().ky_media_source_remove_source_buffer)(self.ptr(), object, error)
        })
    }

    pub fn set_duration(&self, duration: f64) -> Result<(), PlayerError> {
        let duration = clock_time(duration);
        // SAFETY: the object is alive.
        check(|error| unsafe { (api().ky_media_source_set_duration)(self.ptr(), duration, error) })
    }

    pub fn end_of_stream(&self, error: MediaSourceEndOfStreamError) -> Result<(), PlayerError> {
        let eos_error = match error {
            MediaSourceEndOfStreamError::None => GST_MEDIA_SOURCE_EOS_ERROR_NONE,
            MediaSourceEndOfStreamError::Network => GST_MEDIA_SOURCE_EOS_ERROR_NETWORK,
            MediaSourceEndOfStreamError::Decode => GST_MEDIA_SOURCE_EOS_ERROR_DECODE,
        };
        // SAFETY: the object is alive.
        check(|error| unsafe {
            (api().ky_media_source_end_of_stream)(self.ptr(), eos_error, error)
        })
    }

    pub fn append(&self, id: usize, data: Vec<u8>) -> Result<(), PlayerError> {
        let buffers = self.source_buffers.lock().unwrap();
        let buffer = buffers
            .get(&id)
            .ok_or_else(|| PlayerError::MediaSource("no such source buffer".to_owned()))?;
        let state = &buffer.signal_state;
        state.outstanding.store(true, Ordering::SeqCst);
        let data = gstreamer::Buffer::from_mut_slice(data);
        let object = buffer.object.as_ptr() as *mut GstObject;
        // SAFETY: `append_buffer` takes the ownership of the buffer (transfer full).
        let result = check(|error| unsafe {
            (api().ky_source_buffer_append_buffer)(object, data.into_glib_ptr(), error)
        });
        if result.is_err() {
            state.outstanding.store(false, Ordering::SeqCst);
        }
        result
    }

    pub fn abort(&self, id: usize) -> Result<(), PlayerError> {
        // SAFETY: the object is alive.
        self.with_buffer(id, |object| {
            check(|error| unsafe { (api().ky_source_buffer_abort)(object, error) })
        })
    }

    pub fn remove(&self, id: usize, start: f64, end: f64) -> Result<(), PlayerError> {
        let (start, end) = (clock_time(start), clock_time(end));
        // SAFETY: the object is alive.
        self.with_buffer(id, |object| {
            check(|error| unsafe { (api().ky_source_buffer_remove)(object, start, end, error) })
        })
    }

    /// The buffered ranges of a source buffer. gstmse (qtdemux) does not apply the edit list of a
    /// fragmented MP4, so the first frame of H.264 with B frames starts one frame late (0.0167s at
    /// 60fps), where Chrome starts at 0: YouTube then thinks its first segment did not land and
    /// asks for it again and again. A first range that starts within `START_SLACK` starts at 0.
    pub fn buffered(&self, id: usize) -> Vec<Range<f64>> {
        const START_SLACK: f64 = 0.2;
        let mut ranges = self.with_buffer(id, buffered_of).unwrap_or_default();
        if let Some(first) = ranges.first_mut() &&
            first.start > 0. &&
            first.start <= START_SLACK
        {
            first.start = 0.;
        }
        ranges
    }

    pub fn set_timestamp_offset(&self, id: usize, offset: f64) -> Result<(), PlayerError> {
        // The library keeps the offset as a GstClockTime, so a negative offset is not possible.
        let offset = clock_time(offset);
        // SAFETY: the object is alive.
        self.with_buffer(id, |object| {
            check(|error| unsafe {
                (api().ky_source_buffer_set_timestamp_offset)(object, offset, error)
            })
        })
    }

    /// The page checked the values (start < end). The library checks each end against the other
    /// one it has: move the end first when the window moves later.
    pub fn set_append_window(&self, id: usize, start: f64, end: f64) -> Result<(), PlayerError> {
        let (start, end) = (clock_time(start), clock_time(end));
        // SAFETY: the object is alive.
        self.with_buffer(id, |object| {
            let set_start = || {
                check(|error| unsafe {
                    (api().ky_source_buffer_set_append_window_start)(object, start, error)
                })
            };
            let set_end = || {
                check(|error| unsafe {
                    (api().ky_source_buffer_set_append_window_end)(object, end, error)
                })
            };
            if set_start().is_ok() {
                set_end()
            } else {
                set_end()?;
                set_start()
            }
        })
    }

    pub fn set_sequence_mode(&self, id: usize, sequence: bool) -> Result<(), PlayerError> {
        let mode = if sequence {
            GST_SOURCE_BUFFER_APPEND_MODE_SEQUENCE
        } else {
            GST_SOURCE_BUFFER_APPEND_MODE_SEGMENTS
        };
        // SAFETY: the object is alive.
        self.with_buffer(id, |object| {
            check(|error| unsafe { (api().ky_source_buffer_set_append_mode)(object, mode, error) })
        })
    }

    /// Another type in the same container (another profile or level of the codec at a quality
    /// switch); the library refuses another container.
    pub fn change_type(&self, id: usize, mime_type: &str) -> Result<(), PlayerError> {
        let mime_type =
            CString::new(mime_type).map_err(|_| PlayerError::MediaSource("bad type".to_owned()))?;
        // SAFETY: the object is alive; a valid NUL-terminated string.
        self.with_buffer(id, |object| {
            check(|error| unsafe {
                (api().ky_source_buffer_change_content_type)(object, mime_type.as_ptr(), error)
            })
        })
    }
}

impl Drop for MediaSource {
    fn drop(&mut self) {
        self.source_buffers.lock().unwrap().clear();
        self.detach();
    }
}

/// The buffered ranges of a gstmse source buffer.
fn buffered_of(object: *mut GstObject) -> Result<Vec<Range<f64>>, PlayerError> {
    let mut error: *mut GError = ptr::null_mut();
    // SAFETY: the result is a new GArray of `GstMediaSourceRange`, or NULL.
    let array = unsafe { (api().ky_source_buffer_get_buffered)(object, &mut error) };
    if array.is_null() {
        return Err(PlayerError::MediaSource(error_message(error)));
    }
    // SAFETY: the array holds `len` ranges; we free it after copying them.
    let ranges = unsafe {
        let len = (*array).len as usize;
        let data = (*array).data as *const GstMediaSourceRange;
        let ranges = (0..len)
            .map(|i| {
                let range = &*data.add(i);
                seconds(range.start)..seconds(range.end)
            })
            .collect();
        glib::ffi::g_array_unref(array);
        ranges
    };
    Ok(ranges)
}
