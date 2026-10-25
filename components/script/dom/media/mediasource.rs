/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions: <https://w3c.github.io/media-source/>.
//!
//! The media player backend (GStreamer's gstmse) parses the appended segments and keeps the
//! coded frames. The DOM side keeps the states the specification gives to the objects
//! (`readyState`, `updating`, the attributes) and fires the events.

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::ServoMedia;
use servo_media::player::{MediaSourceEndOfStreamError, Player, PlayerError};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, ReadyState,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmlmediaelement::HTMLMediaElement;
use crate::dom::sourcebuffer::SourceBuffer;
use crate::dom::sourcebufferlist::SourceBufferList;
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    ready_state: Cell<ReadyState>,
    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    duration: Cell<f64>,
    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    source_buffers: MutNullableDom<SourceBufferList>,
    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    /// (every source buffer is active: the backend plays all of their tracks)
    active_source_buffers: MutNullableDom<SourceBufferList>,
    /// The media element this media source is attached to.
    media_element: MutNullableDom<HTMLMediaElement>,
}

impl MediaSource {
    fn new_inherited() -> Self {
        Self {
            eventtarget: EventTarget::new_inherited(),
            ready_state: Cell::new(ReadyState::Closed),
            duration: Cell::new(f64::NAN),
            source_buffers: Default::default(),
            active_source_buffers: Default::default(),
            media_element: Default::default(),
        }
    }

    pub(crate) fn ready_state(&self) -> ReadyState {
        self.ready_state.get()
    }

    fn queue_event(&self, name: &str) {
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue_simple_event(self.upcast(), Atom::from(name));
    }

    fn source_buffer_list(&self, cx: &mut JSContext) -> DomRoot<SourceBufferList> {
        let global = self.global();
        let window = global.as_window();
        self.source_buffers
            .or_init(|| SourceBufferList::new(cx, window))
    }

    fn active_source_buffer_list(&self, cx: &mut JSContext) -> DomRoot<SourceBufferList> {
        let global = self.global();
        let window = global.as_window();
        self.active_source_buffers
            .or_init(|| SourceBufferList::new(cx, window))
    }

    /// Runs `f` with the player of the media element this media source is attached to.
    pub(crate) fn with_player<R>(
        &self,
        f: impl FnOnce(&dyn Player) -> Result<R, PlayerError>,
    ) -> Result<R, PlayerError> {
        let Some(media_element) = self.media_element.get() else {
            return Err(PlayerError::MediaSourceNotSupported);
        };
        media_element
            .with_player(f)
            .unwrap_or(Err(PlayerError::MediaSourceNotSupported))
    }

    /// The media element took this media source as its media provider (the resource fetch
    /// algorithm). It opens when the backend reports that the source is attached.
    pub(crate) fn attach(&self, media_element: &HTMLMediaElement) {
        self.media_element.set(Some(media_element));
    }

    /// <https://w3c.github.io/media-source/#mediasource-attach>: the backend is ready.
    pub(crate) fn open(&self) {
        if self.media_element.get().is_none() || self.ready_state.get() != ReadyState::Closed {
            return;
        }
        self.ready_state.set(ReadyState::Open);
        self.queue_event("sourceopen");
    }

    /// <https://w3c.github.io/media-source/#mediasource-detach>
    pub(crate) fn detach(&self) {
        if self.media_element.get().is_none() {
            return;
        }
        self.ready_state.set(ReadyState::Closed);
        self.duration.set(f64::NAN);
        for list in [self.source_buffers.get(), self.active_source_buffers.get()]
            .into_iter()
            .flatten()
        {
            for buffer in list.take_all() {
                buffer.removed();
            }
        }
        self.media_element.set(None);
        self.queue_event("sourceclose");
    }

    pub(crate) fn has_source_buffer_updating(&self) -> bool {
        self.source_buffers
            .get()
            .is_some_and(|list| list.iter().any(|buffer| buffer.is_updating()))
    }

    /// The backend finished an append on the source buffer with the given id.
    pub(crate) fn source_buffer_update_end(&self, id: usize, error: bool) {
        let Some(list) = self.source_buffers.get() else {
            return;
        };
        if let Some(buffer) = list.iter().find(|buffer| buffer.id() == id) {
            buffer.backend_update_end(error);
        }
    }

    /// The buffered ranges of the media element: the intersection of the ranges of the active
    /// source buffers (<https://w3c.github.io/media-source/#htmlmediaelement-extensions-buffered>).
    pub(crate) fn buffered(&self) -> Vec<(f64, f64)> {
        let Some(list) = self.active_source_buffers.get() else {
            return vec![];
        };
        let mut result: Option<Vec<(f64, f64)>> = None;
        for buffer in list.iter() {
            let ranges = buffer.buffered_ranges();
            result = Some(match result {
                None => ranges,
                Some(previous) => intersect(&previous, &ranges),
            });
        }
        result.unwrap_or_default()
    }

    /// <https://w3c.github.io/media-source/#dfn-transition-to-open> on an append to an ended source.
    pub(crate) fn reopen_if_ended(&self) {
        if self.ready_state.get() == ReadyState::Ended {
            self.ready_state.set(ReadyState::Open);
            self.queue_event("sourceopen");
        }
    }

    /// <https://w3c.github.io/media-source/#end-of-stream-algorithm> with the decode error (an
    /// append failed). The backend has already ended its stream.
    pub(crate) fn end_with_decode_error(&self) {
        if self.ready_state.get() == ReadyState::Open {
            self.ready_state.set(ReadyState::Ended);
            self.queue_event("sourceended");
        }
    }
}

fn intersect(a: &[(f64, f64)], b: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut result = vec![];
    for &(a_start, a_end) in a {
        for &(b_start, b_end) in b {
            let start = a_start.max(b_start);
            let end = a_end.min(b_end);
            if start < end {
                result.push((start, end));
            }
        }
    }
    result
}

fn backend_error(error: PlayerError) -> Error {
    Error::InvalidState(Some(format!("{error:?}")))
}

impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    /// <https://w3c.github.io/media-source/#dom-mediasource-constructor>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<MediaSource> {
        reflect_dom_object_with_proto(cx, Box::new(MediaSource::new_inherited()), window, proto)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    fn SourceBuffers(&self, cx: &mut JSContext) -> DomRoot<SourceBufferList> {
        self.source_buffer_list(cx)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    fn ActiveSourceBuffers(&self, cx: &mut JSContext) -> DomRoot<SourceBufferList> {
        self.active_source_buffer_list(cx)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    fn ReadyState(&self) -> ReadyState {
        self.ready_state.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn Duration(&self) -> f64 {
        if self.ready_state.get() == ReadyState::Closed {
            return f64::NAN;
        }
        self.duration.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn SetDuration(&self, value: f64) -> ErrorResult {
        // Step 1. If the value being set is negative or NaN then throw a TypeError exception.
        if value < 0. || value.is_nan() {
            return Err(Error::Type(c"The duration is negative or NaN".to_owned()));
        }
        // Step 2. If the readyState attribute is not "open" then throw an InvalidStateError.
        // Step 3. If the updating attribute equals true on any SourceBuffer in sourceBuffers,
        // then throw an InvalidStateError exception.
        if self.ready_state.get() != ReadyState::Open || self.has_source_buffer_updating() {
            return Err(Error::InvalidState(None));
        }
        // Step 4. Run the duration change algorithm with new duration set to the value being
        // assigned to this attribute.
        if self.duration.get() == value {
            return Ok(());
        }
        self.with_player(|player| player.media_source_set_duration(value))
            .map_err(backend_error)?;
        self.duration.set(value);
        Ok(())
    }

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceopen
    event_handler!(sourceopen, GetOnsourceopen, SetOnsourceopen);

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceended
    event_handler!(sourceended, GetOnsourceended, SetOnsourceended);

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceclose
    event_handler!(sourceclose, GetOnsourceclose, SetOnsourceclose);

    /// <https://w3c.github.io/media-source/#dom-mediasource-canconstructindedicatedworker>
    fn CanConstructInDedicatedWorker(_window: &Window) -> bool {
        false
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    fn AddSourceBuffer(
        &self,
        cx: &mut JSContext,
        type_: DOMString,
    ) -> Fallible<DomRoot<SourceBuffer>> {
        let type_ = type_.str().to_string();
        // Step 1. If type is an empty string then throw a TypeError exception.
        if type_.is_empty() {
            return Err(Error::Type(c"The type is empty".to_owned()));
        }
        // Step 2. If type contains a MIME type that is not supported ..., then throw a
        // NotSupportedError exception.
        if !ServoMedia::get().is_media_source_type_supported(&type_) {
            return Err(Error::NotSupported(None));
        }
        // Step 4. If the readyState attribute is not in the "open" state then throw an
        // InvalidStateError exception.
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        // Step 5. Create a new SourceBuffer object and associated resources.
        let id = self
            .with_player(|player| player.media_source_add_source_buffer(&type_))
            .map_err(|error| match error {
                PlayerError::MediaSource(message) => Error::NotSupported(Some(message)),
                error => backend_error(error),
            })?;
        let buffer = SourceBuffer::new(cx, self.global().as_window(), self, id);
        // Step 8. Add the new object to sourceBuffers and queue a task to fire an event named
        // addsourcebuffer at sourceBuffers.
        self.source_buffer_list(cx).push(&buffer);
        self.active_source_buffer_list(cx).push(&buffer);
        Ok(buffer)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn RemoveSourceBuffer(&self, cx: &mut JSContext, source_buffer: &SourceBuffer) -> ErrorResult {
        // Step 1. If sourceBuffer specifies an object that is not in sourceBuffers then throw a
        // NotFoundError exception.
        let list = self.source_buffer_list(cx);
        if !list.contains(source_buffer) {
            return Err(Error::NotFound(None));
        }
        // Step 2. If the sourceBuffer.updating attribute equals true, then run the abort steps.
        source_buffer.abort_updating();
        let _ =
            self.with_player(|player| player.media_source_remove_source_buffer(source_buffer.id()));
        // Steps 9-11. Remove sourceBuffer from activeSourceBuffers and sourceBuffers, and fire
        // removesourcebuffer at them.
        self.active_source_buffer_list(cx).remove(source_buffer);
        list.remove(source_buffer);
        // Step 12. Destroy all resources for sourceBuffer.
        source_buffer.removed();
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn EndOfStream(&self, error: Option<EndOfStreamError>) -> ErrorResult {
        // Step 1. If the readyState attribute is not in the "open" state then throw an
        // InvalidStateError exception.
        // Step 2. If the updating attribute equals true on any SourceBuffer in sourceBuffers,
        // then throw an InvalidStateError exception.
        if self.ready_state.get() != ReadyState::Open || self.has_source_buffer_updating() {
            return Err(Error::InvalidState(None));
        }
        // Step 3. Run the end of stream algorithm with the error parameter set to error.
        let error = match error {
            None => MediaSourceEndOfStreamError::None,
            Some(EndOfStreamError::Network) => MediaSourceEndOfStreamError::Network,
            Some(EndOfStreamError::Decode) => MediaSourceEndOfStreamError::Decode,
        };
        self.ready_state.set(ReadyState::Ended);
        self.queue_event("sourceended");
        let _ = self.with_player(|player| player.media_source_end_of_stream(error));
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn SetLiveSeekableRange(&self, start: Finite<f64>, end: Finite<f64>) -> ErrorResult {
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        if *start < 0. || *start > *end {
            return Err(Error::Type(c"Bad range".to_owned()));
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn ClearLiveSeekableRange(&self) -> ErrorResult {
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    fn IsTypeSupported(_window: &Window, type_: DOMString) -> bool {
        let type_ = type_.str().to_string();
        !type_.is_empty() && ServoMedia::get().is_media_source_type_supported(&type_)
    }
}
