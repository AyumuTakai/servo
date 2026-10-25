/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions on GStreamer's `gstmse` library (gst-plugins-bad 1.24 and later).
//! The library parses the appended segments, keeps the coded frames of each source buffer,
//! answers the buffered ranges and feeds `msesrc` (the `mse://` source of `playbin3`).
//! gstreamer-rs has no bindings for it, so the few functions used here are declared below.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int};
use std::ops::Range;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

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
/// `GST_MEDIA_SOURCE_ERROR_QUOTA_EXCEEDED` (gstmse keeps at most 16MiB per source buffer)
const GST_MEDIA_SOURCE_ERROR_QUOTA_EXCEEDED: c_int = 4;
/// `GST_MSE_SRC_READY_STATE_HAVE_METADATA`
const GST_MSE_SRC_READY_STATE_HAVE_METADATA: c_int = 1;

/// `GstMediaSourceRange` (and `GstSourceBufferInterval`).
#[repr(C)]
struct GstMediaSourceRange {
    start: GstClockTime,
    end: GstClockTime,
}

#[link(name = "gstmse-1.0")]
unsafe extern "C" {
    fn gst_media_source_get_type() -> glib::ffi::GType;
    fn gst_media_source_error_quark() -> glib::ffi::GQuark;
    fn gst_source_buffer_get_type() -> glib::ffi::GType;
    fn gst_media_source_new() -> *mut GstObject;
    fn gst_media_source_is_type_supported(type_: *const c_char) -> glib::ffi::gboolean;
    fn gst_media_source_attach(
        media_source: *mut GstObject,
        element: *mut gstreamer::ffi::GstElement,
    );
    fn gst_media_source_detach(media_source: *mut GstObject);
    fn gst_media_source_add_source_buffer(
        media_source: *mut GstObject,
        type_: *const c_char,
        error: *mut *mut GError,
    ) -> *mut GstObject;
    fn gst_media_source_remove_source_buffer(
        media_source: *mut GstObject,
        buffer: *mut GstObject,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_media_source_set_duration(
        media_source: *mut GstObject,
        duration: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_media_source_end_of_stream(
        media_source: *mut GstObject,
        eos_error: c_int,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_source_buffer_append_buffer(
        buffer: *mut GstObject,
        buf: *mut gstreamer::ffi::GstBuffer,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_source_buffer_abort(
        buffer: *mut GstObject,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_source_buffer_get_updating(buffer: *mut GstObject) -> glib::ffi::gboolean;
    fn gst_source_buffer_remove(
        buffer: *mut GstObject,
        start: GstClockTime,
        end: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_source_buffer_get_buffered(
        buffer: *mut GstObject,
        error: *mut *mut GError,
    ) -> *mut glib::ffi::GArray;
    fn gst_source_buffer_set_timestamp_offset(
        buffer: *mut GstObject,
        offset: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_source_buffer_set_append_window_start(
        buffer: *mut GstObject,
        start: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_source_buffer_set_append_window_end(
        buffer: *mut GstObject,
        end: GstClockTime,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_source_buffer_set_append_mode(
        buffer: *mut GstObject,
        mode: c_int,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
    fn gst_source_buffer_change_content_type(
        buffer: *mut GstObject,
        type_: *const c_char,
        error: *mut *mut GError,
    ) -> glib::ffi::gboolean;
}

/// The start of `struct _GstSourceBuffer` (`gstsourcebuffer.c`, private; the same from 1.24 to main
/// in 2026-10), to raise `size_limit`.
#[repr(C)]
struct GstSourceBufferStart {
    parent_instance: GstObject,
    append_mode: c_int,
    append_window_start: GstClockTime,
    append_window_end: GstClockTime,
    content_type: *mut c_char,
    generate_timestamps: glib::ffi::gboolean,
    timestamp_offset: GstClockTime,
    updating: glib::ffi::gboolean,
    errored: glib::ffi::gboolean,
    size_limit: usize,
    size: usize,
}

/// gstmse keeps at most 16MiB of coded frames per source buffer and has no way to change it. That
/// is about 10 seconds of 1080p video: a player that appends 10 second segments (hls.js) is stuck
/// once the buffer is full, because gstmse only evicts frames 5 seconds before the position.
/// Chrome keeps 150MB of video and 12MB of audio.
const GSTMSE_DEFAULT_SIZE_LIMIT: usize = 1 << 24;
const VIDEO_SIZE_LIMIT: usize = 150 * 1024 * 1024;

/// Checks that a new gstmse source buffer is laid out as `GstSourceBufferStart` (on the values a
/// new source buffer has), and raises its `size_limit` if it is for video.
///
/// # Safety
/// `object` must be a new `GstSourceBuffer` that nothing else uses yet.
unsafe fn check_layout_and_raise_size_limit(object: *mut GstObject, mime_type: &CString) -> bool {
    // SAFETY: the caller passes a GstSourceBuffer; the reads stay inside its instance (checked
    // against the size of the type) and are checked before the write.
    unsafe {
        let mut query: glib::gobject_ffi::GTypeQuery = std::mem::zeroed();
        glib::gobject_ffi::g_type_query(gst_source_buffer_get_type(), &mut query);
        if (query.instance_size as usize) < std::mem::size_of::<GstSourceBufferStart>() {
            return false;
        }
        let start = object as *mut GstSourceBufferStart;
        let content_type = (*start).content_type;
        let laid_out_as_expected = (*start).append_window_start == 0 &&
            (*start).append_window_end == GST_CLOCK_TIME_NONE &&
            (*start).size_limit == GSTMSE_DEFAULT_SIZE_LIMIT &&
            (*start).size == 0 &&
            !content_type.is_null() &&
            std::ffi::CStr::from_ptr(content_type) == mime_type.as_c_str();
        if !laid_out_as_expected {
            log::warn!(
                "MSE: gstmse has another layout (16MiB per source buffer, no append window start)"
            );
            return false;
        }
        if mime_type.to_bytes().starts_with(b"video/") {
            (*start).size_limit = VIDEO_SIZE_LIMIT;
        }
        true
    }
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
            (*error).domain == gst_media_source_error_quark() &&
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

/// Whether the system has the gstmse library and a demuxer and decoders for this type.
pub fn is_type_supported(mime_type: &str) -> bool {
    let Ok(mime_type) = CString::new(mime_type) else {
        return false;
    };
    // SAFETY: a valid NUL-terminated string.
    unsafe { gst_media_source_is_type_supported(mime_type.as_ptr()) != glib::ffi::GFALSE }
}

/// What happens to a source buffer that gstmse does not have yet (see `SourceBuffer::object`).
enum PendingOperation {
    Append(gstreamer::Buffer),
    TimestampOffset(GstClockTime),
    AppendWindow(GstClockTime, GstClockTime),
    Mode(c_int),
    ChangeType(CString),
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
    mime_type: CString,
    /// The gstmse source buffer. **gstmse (1.24 to 1.28 and main, 2026-10) deadlocks** when a
    /// source buffer receives its initialization segment while another source buffer of the media
    /// source has none yet: `update_ready_state_for_init_segment()` of `gstmsesrc.c` returns
    /// without releasing the media source lock of `msesrc`, and every later call waits for it. A
    /// page with a video and an audio source buffer (YouTube) always hits it. So gstmse gets the
    /// first source buffer that receives data at once, and the others only when `msesrc` reached
    /// `HAVE_METADATA` (the function then returns early, releasing the lock). Until then, their
    /// operations wait in `pending`.
    object: Option<gstreamer::Object>,
    /// The gstmse source buffer is laid out as `GstSourceBufferStart`.
    known_layout: bool,
    pending: Vec<PendingOperation>,
    signal_state: Arc<SignalState>,
}

/// One `MediaSource` of a player. The GObjects are thread safe. The signal handlers never take
/// the lock of `source_buffers`, so gstmse may be called with it held.
pub struct MediaSource {
    object: gstreamer::Object,
    source_buffers: Mutex<HashMap<usize, SourceBuffer>>,
    next_id: Mutex<usize>,
    /// The `msesrc` element, once attached.
    element: Mutex<Option<gstreamer::Element>>,
    observer: Arc<Mutex<GenericCallback<PlayerEvent>>>,
}

impl MediaSource {
    pub fn new(observer: Arc<Mutex<GenericCallback<PlayerEvent>>>) -> Self {
        // SAFETY: the constructor returns a new reference.
        let object = unsafe {
            let _ = gst_media_source_get_type();
            let _ = gst_source_buffer_get_type();
            take_object(gst_media_source_new())
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
            element: Mutex::new(None),
            observer,
        }
    }

    fn ptr(&self) -> *mut GstObject {
        self.object.as_ptr() as *mut GstObject
    }

    /// Attaches the media source to the `msesrc` element `playbin3` made for `mse://`.
    pub fn attach(self: &Arc<Self>, element: &gstreamer::Element) {
        // When `msesrc` reaches HAVE_METADATA, give gstmse the source buffers that wait. The
        // notification comes with the media source lock of `msesrc` held: work on another thread.
        let weak: Weak<Self> = Arc::downgrade(self);
        element.connect_notify(Some("ready-state"), move |element, _| {
            if ready_state(element) < GST_MSE_SRC_READY_STATE_HAVE_METADATA {
                return;
            }
            let weak = weak.clone();
            std::thread::spawn(move || {
                if let Some(media_source) = weak.upgrade() {
                    media_source.create_waiting_source_buffers();
                }
            });
        });
        *self.element.lock().unwrap() = Some(element.clone());
        // SAFETY: both objects are alive; gstmse checks the type of the element.
        unsafe { gst_media_source_attach(self.ptr(), element.as_ptr()) }
    }

    pub fn detach(&self) {
        self.element.lock().unwrap().take();
        // SAFETY: the object is alive.
        unsafe { gst_media_source_detach(self.ptr()) }
    }

    fn has_metadata(&self) -> bool {
        self.element
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|element| ready_state(element) >= GST_MSE_SRC_READY_STATE_HAVE_METADATA)
    }

    pub fn add_source_buffer(&self, mime_type: &str) -> Result<usize, PlayerError> {
        let mime_type =
            CString::new(mime_type).map_err(|_| PlayerError::MediaSource("bad type".to_owned()))?;
        let id = {
            let mut next_id = self.next_id.lock().unwrap();
            *next_id += 1;
            *next_id
        };
        let mut buffer = SourceBuffer {
            mime_type,
            object: None,
            known_layout: false,
            pending: vec![],
            signal_state: Default::default(),
        };
        if self.has_metadata() {
            self.create(id, &mut buffer)?;
        }
        self.source_buffers.lock().unwrap().insert(id, buffer);
        Ok(id)
    }

    /// Creates the gstmse source buffer of `buffer`, then runs the operations that waited.
    fn create(&self, id: usize, buffer: &mut SourceBuffer) -> Result<(), PlayerError> {
        let mut error: *mut GError = ptr::null_mut();
        // SAFETY: valid arguments; the result is a new reference or NULL with `error` set.
        let object = unsafe {
            gst_media_source_add_source_buffer(self.ptr(), buffer.mime_type.as_ptr(), &mut error)
        };
        if object.is_null() {
            return Err(PlayerError::MediaSource(error_message(error)));
        }
        // SAFETY: a new source buffer, that nothing uses yet.
        buffer.known_layout =
            unsafe { check_layout_and_raise_size_limit(object, &buffer.mime_type) };
        // SAFETY: we own the reference returned above.
        let object = unsafe { take_object(object) };

        let signal_state = buffer.signal_state.clone();
        object.connect("on-error", false, move |_| {
            if signal_state.outstanding.load(Ordering::SeqCst) {
                signal_state.errored.store(true, Ordering::SeqCst);
            }
            None
        });
        // gstmse (1.26) also ends an append, once a second, when nothing was appended (the
        // append task stops waiting for data); and its events come later, from another thread.
        // An end counts only when we wait for one and gstmse is not updating any more.
        let signal_state = buffer.signal_state.clone();
        let observer = self.observer.clone();
        object.connect("on-update-end", false, move |args| {
            let Ok(object) = args[0].get::<gstreamer::Object>() else {
                return None;
            };
            // SAFETY: the object is alive.
            if unsafe { gst_source_buffer_get_updating(object.as_ptr() as *mut GstObject) } !=
                glib::ffi::GFALSE
            {
                return None;
            }
            if !signal_state.outstanding.swap(false, Ordering::SeqCst) {
                return None;
            }
            let error = signal_state.errored.swap(false, Ordering::SeqCst);
            let _ = observer
                .lock()
                .unwrap()
                .send(PlayerEvent::SourceBufferUpdateEnd { id, error });
            None
        });

        let raw = object.as_ptr() as *mut GstObject;
        buffer.object = Some(object);
        for operation in std::mem::take(&mut buffer.pending) {
            let is_append = matches!(operation, PendingOperation::Append(_));
            let result = match operation {
                PendingOperation::Append(data) => append_to(raw, &buffer.signal_state, data),
                PendingOperation::TimestampOffset(offset) => set_timestamp_offset(raw, offset),
                PendingOperation::AppendWindow(start, end) => {
                    set_append_window(raw, buffer.known_layout, start, end)
                },
                PendingOperation::Mode(mode) => set_append_mode(raw, mode),
                PendingOperation::ChangeType(mime_type) => change_content_type(raw, &mime_type),
            };
            if let Err(error) = result {
                log::warn!("MSE: an operation that waited failed: {error:?}");
                if is_append &&
                    buffer
                        .signal_state
                        .outstanding
                        .swap(false, Ordering::SeqCst)
                {
                    let _ = self
                        .observer
                        .lock()
                        .unwrap()
                        .send(PlayerEvent::SourceBufferUpdateEnd { id, error: true });
                }
            }
        }
        Ok(())
    }

    fn create_waiting_source_buffers(&self) {
        let mut buffers = self.source_buffers.lock().unwrap();
        for (id, buffer) in buffers.iter_mut() {
            if buffer.object.is_none() &&
                let Err(error) = self.create(*id, buffer)
            {
                log::warn!("MSE: could not create a source buffer: {error:?}");
            }
        }
    }

    /// Runs `operation` on the gstmse source buffer, or keeps it for when there is one.
    fn run(&self, id: usize, operation: PendingOperation) -> Result<(), PlayerError> {
        let mut buffers = self.source_buffers.lock().unwrap();
        let first = buffers.values().all(|buffer| buffer.object.is_none());
        let Some(buffer) = buffers.get_mut(&id) else {
            return Err(PlayerError::MediaSource("no such source buffer".to_owned()));
        };
        if buffer.object.is_none() {
            let is_append = matches!(operation, PendingOperation::Append(_));
            if let PendingOperation::Append(_) = operation {
                buffer
                    .signal_state
                    .outstanding
                    .store(true, Ordering::SeqCst);
            }
            // Replace a setting that waits with the newer one.
            buffer.pending.retain(|pending| {
                std::mem::discriminant(pending) != std::mem::discriminant(&operation) ||
                    matches!(pending, PendingOperation::Append(_))
            });
            buffer.pending.push(operation);
            // The first source buffer that receives data goes to gstmse at once.
            if is_append && (first || self.has_metadata()) {
                return self.create(id, buffer);
            }
            return Ok(());
        }
        let raw = buffer.object.as_ref().unwrap().as_ptr() as *mut GstObject;
        match operation {
            PendingOperation::Append(data) => append_to(raw, &buffer.signal_state, data),
            PendingOperation::TimestampOffset(offset) => set_timestamp_offset(raw, offset),
            PendingOperation::AppendWindow(start, end) => {
                set_append_window(raw, buffer.known_layout, start, end)
            },
            PendingOperation::Mode(mode) => set_append_mode(raw, mode),
            PendingOperation::ChangeType(mime_type) => change_content_type(raw, &mime_type),
        }
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
            .and_then(|buffer| buffer.object.clone())
            .ok_or_else(|| PlayerError::MediaSource("no such source buffer".to_owned()))?;
        f(object.as_ptr() as *mut GstObject)
    }

    pub fn remove_source_buffer(&self, id: usize) -> Result<(), PlayerError> {
        let Some(buffer) = self.source_buffers.lock().unwrap().remove(&id) else {
            return Ok(());
        };
        let Some(object) = buffer.object else {
            return Ok(());
        };
        let object = object.as_ptr() as *mut GstObject;
        // SAFETY: both objects are alive.
        check(|error| unsafe { gst_media_source_remove_source_buffer(self.ptr(), object, error) })
    }

    pub fn set_duration(&self, duration: f64) -> Result<(), PlayerError> {
        let duration = clock_time(duration);
        // SAFETY: the object is alive.
        check(|error| unsafe { gst_media_source_set_duration(self.ptr(), duration, error) })
    }

    pub fn end_of_stream(&self, error: MediaSourceEndOfStreamError) -> Result<(), PlayerError> {
        let eos_error = match error {
            MediaSourceEndOfStreamError::None => GST_MEDIA_SOURCE_EOS_ERROR_NONE,
            MediaSourceEndOfStreamError::Network => GST_MEDIA_SOURCE_EOS_ERROR_NETWORK,
            MediaSourceEndOfStreamError::Decode => GST_MEDIA_SOURCE_EOS_ERROR_DECODE,
        };
        // SAFETY: the object is alive.
        check(|error| unsafe { gst_media_source_end_of_stream(self.ptr(), eos_error, error) })
    }

    pub fn append(&self, id: usize, data: Vec<u8>) -> Result<(), PlayerError> {
        self.run(
            id,
            PendingOperation::Append(gstreamer::Buffer::from_mut_slice(data)),
        )
    }

    pub fn abort(&self, id: usize) -> Result<(), PlayerError> {
        // SAFETY: the object is alive.
        self.with_buffer(id, |object| {
            check(|error| unsafe { gst_source_buffer_abort(object, error) })
        })
    }

    pub fn remove(&self, id: usize, start: f64, end: f64) -> Result<(), PlayerError> {
        let (start, end) = (clock_time(start), clock_time(end));
        // SAFETY: the object is alive.
        self.with_buffer(id, |object| {
            check(|error| unsafe { gst_source_buffer_remove(object, start, end, error) })
        })
    }

    pub fn buffered(&self, id: usize) -> Vec<Range<f64>> {
        self.with_buffer(id, |object| {
            let mut error: *mut GError = ptr::null_mut();
            // SAFETY: the result is a new GArray of `GstMediaSourceRange`, or NULL.
            let array = unsafe { gst_source_buffer_get_buffered(object, &mut error) };
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
        })
        .unwrap_or_default()
    }

    pub fn set_timestamp_offset(&self, id: usize, offset: f64) -> Result<(), PlayerError> {
        // gstmse keeps the offset as a GstClockTime, so a negative offset is not possible.
        self.run(id, PendingOperation::TimestampOffset(clock_time(offset)))
    }

    pub fn set_append_window(&self, id: usize, start: f64, end: f64) -> Result<(), PlayerError> {
        self.run(
            id,
            PendingOperation::AppendWindow(clock_time(start), clock_time(end)),
        )
    }

    pub fn set_sequence_mode(&self, id: usize, sequence: bool) -> Result<(), PlayerError> {
        let mode = if sequence {
            GST_SOURCE_BUFFER_APPEND_MODE_SEQUENCE
        } else {
            GST_SOURCE_BUFFER_APPEND_MODE_SEGMENTS
        };
        self.run(id, PendingOperation::Mode(mode))
    }

    pub fn change_type(&self, id: usize, mime_type: &str) -> Result<(), PlayerError> {
        let mime_type =
            CString::new(mime_type).map_err(|_| PlayerError::MediaSource("bad type".to_owned()))?;
        self.run(id, PendingOperation::ChangeType(mime_type))
    }
}

impl Drop for MediaSource {
    fn drop(&mut self) {
        self.source_buffers.lock().unwrap().clear();
        self.detach();
    }
}

/// The `ready-state` of `msesrc` (`GstMseSrcReadyState`).
fn ready_state(element: &gstreamer::Element) -> c_int {
    let value = element.property_value("ready-state");
    // SAFETY: the property is an enum.
    unsafe { glib::gobject_ffi::g_value_get_enum(value.to_glib_none().0) }
}

fn append_to(
    object: *mut GstObject,
    signal_state: &SignalState,
    data: gstreamer::Buffer,
) -> Result<(), PlayerError> {
    signal_state.outstanding.store(true, Ordering::SeqCst);
    // SAFETY: `append_buffer` takes the ownership of the buffer (transfer full).
    let result = check(|error| unsafe {
        gst_source_buffer_append_buffer(object, data.into_glib_ptr(), error)
    });
    if result.is_err() {
        signal_state.outstanding.store(false, Ordering::SeqCst);
    }
    result
}

fn set_timestamp_offset(object: *mut GstObject, offset: GstClockTime) -> Result<(), PlayerError> {
    // SAFETY: the object is alive.
    check(|error| unsafe { gst_source_buffer_set_timestamp_offset(object, offset, error) })
}

/// gstmse (1.24 to main, 2026-10) refuses every valid append window start: the check of
/// `gst_source_buffer_set_append_window_start()` is reversed (`start <= append_window_end` is the
/// error). So a source buffer of the known layout gets both ends written under its object lock;
/// any other gets the end only. The page checked the values (start < end).
fn set_append_window(
    object: *mut GstObject,
    known_layout: bool,
    start: GstClockTime,
    end: GstClockTime,
) -> Result<(), PlayerError> {
    if !known_layout {
        // SAFETY: the object is alive.
        return check(|error| unsafe {
            gst_source_buffer_set_append_window_end(object, end, error)
        });
    }
    // SAFETY: the layout was checked when the source buffer was made; the fields are guarded by
    // the object lock, as gstmse does.
    unsafe {
        let lock = &mut (*object).lock as *mut glib::ffi::GMutex;
        glib::ffi::g_mutex_lock(lock);
        let fields = object as *mut GstSourceBufferStart;
        let updating = (*fields).updating != glib::ffi::GFALSE;
        if !updating {
            (*fields).append_window_start = start;
            (*fields).append_window_end = end;
        }
        glib::ffi::g_mutex_unlock(lock);
        if updating {
            return Err(PlayerError::MediaSource(
                "append window cannot be set while updating".to_owned(),
            ));
        }
    }
    Ok(())
}

fn set_append_mode(object: *mut GstObject, mode: c_int) -> Result<(), PlayerError> {
    // SAFETY: the object is alive.
    check(|error| unsafe { gst_source_buffer_set_append_mode(object, mode, error) })
}

fn change_content_type(object: *mut GstObject, mime_type: &CString) -> Result<(), PlayerError> {
    // SAFETY: the object is alive and the string is NUL-terminated.
    check(|error| unsafe {
        gst_source_buffer_change_content_type(object, mime_type.as_ptr(), error)
    })
}
