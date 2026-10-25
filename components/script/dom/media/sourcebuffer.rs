/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use servo_media::player::PlayerError;
use stylo_atoms::Atom;

use crate::dom::bindings::buffer_source::get_buffer_source_copy;
use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{MediaSourceMethods, ReadyState};
use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    AppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::mediasource::MediaSource;
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};
use crate::dom::window::Window;

/// <https://w3c.github.io/media-source/#sourcebuffer>
///
/// gstmse (1.26) runs an append asynchronously and reports its end, but `abort()` and
/// `remove()` neither report anything nor wait. So the `updating` attribute and the events
/// are kept here: an append the page aborted still runs in the backend, its end is swallowed,
/// and the next append waits for it.
#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    /// The id of the source buffer in the media player backend.
    id: usize,
    /// The parent media source; `None` once removed from it.
    media_source: MutNullableDom<MediaSource>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    updating: Cell<bool>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    mode: Cell<AppendMode>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    timestamp_offset: Cell<f64>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    append_window_start: Cell<f64>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    append_window_end: Cell<f64>,
    /// An append is running in the backend.
    backend_busy: Cell<bool>,
    /// Appends the backend finishes that nobody waits for any more (aborted ones).
    swallowed_update_ends: Cell<u32>,
    /// The data of an append that waits for the backend to finish an aborted one.
    #[ignore_malloc_size_of = "Bytes waiting for the backend"]
    queued_append: DomRefCell<Option<Vec<u8>>>,
    /// Incremented when a removal is aborted, so that its queued end does nothing.
    remove_generation: Cell<u32>,
}

impl SourceBuffer {
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        media_source: &MediaSource,
        id: usize,
    ) -> DomRoot<SourceBuffer> {
        reflect_dom_object(
            cx,
            Box::new(SourceBuffer {
                eventtarget: EventTarget::new_inherited(),
                id,
                media_source: MutNullableDom::new(Some(media_source)),
                updating: Cell::new(false),
                mode: Cell::new(AppendMode::Segments),
                timestamp_offset: Cell::new(0.),
                append_window_start: Cell::new(0.),
                append_window_end: Cell::new(f64::INFINITY),
                backend_busy: Cell::new(false),
                swallowed_update_ends: Cell::new(0),
                queued_append: DomRefCell::new(None),
                remove_generation: Cell::new(0),
            }),
            window,
        )
    }

    pub(crate) fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn is_updating(&self) -> bool {
        self.updating.get()
    }

    fn queue_event(&self, name: &str) {
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue_simple_event(self.upcast(), Atom::from(name));
    }

    /// The source buffer was removed from its media source (or the media source closed).
    pub(crate) fn removed(&self) {
        self.media_source.set(None);
        self.updating.set(false);
        self.queued_append.borrow_mut().take();
    }

    pub(crate) fn buffered_ranges(&self) -> Vec<(f64, f64)> {
        let Some(media_source) = self.media_source.get() else {
            return vec![];
        };
        media_source
            .with_player(|player| Ok(player.source_buffer_buffered(self.id)))
            .unwrap_or_default()
            .into_iter()
            .map(|range| (range.start, range.end))
            .collect()
    }

    /// Checks shared by the attribute setters: removed or updating.
    fn check_not_removed_nor_updating(&self) -> Result<DomRoot<MediaSource>, Error> {
        let Some(media_source) = self.media_source.get() else {
            return Err(Error::InvalidState(None));
        };
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        Ok(media_source)
    }

    /// Gives an append to the backend.
    fn send_append(&self, media_source: &MediaSource, data: Vec<u8>) -> Result<(), PlayerError> {
        self.backend_busy.set(true);
        let result = media_source.with_player(|player| player.source_buffer_append(self.id, data));
        if result.is_err() {
            self.backend_busy.set(false);
        }
        result
    }

    /// <https://w3c.github.io/media-source/#dfn-append-error>
    fn append_error(&self, media_source: &MediaSource) {
        self.updating.set(false);
        self.queue_event("error");
        self.queue_event("updateend");
        media_source.end_with_decode_error();
    }

    /// The backend finished an append (the end of the buffer append algorithm).
    pub(crate) fn backend_update_end(&self, error: bool) {
        if !self.backend_busy.get() {
            return;
        }
        self.backend_busy.set(false);
        let Some(media_source) = self.media_source.get() else {
            return;
        };
        if self.swallowed_update_ends.get() > 0 {
            self.swallowed_update_ends
                .set(self.swallowed_update_ends.get() - 1);
        } else if self.updating.get() {
            if error {
                self.append_error(&media_source);
            } else {
                self.updating.set(false);
                self.queue_event("update");
                self.queue_event("updateend");
            }
        }
        // An append that waited for the backend: too late to throw, a full source buffer is a
        // failed append too.
        if let Some(data) = self.queued_append.borrow_mut().take() &&
            self.send_append(&media_source, data).is_err()
        {
            self.append_error(&media_source);
        }
    }

    /// The abort steps of `abort()` and `removeSourceBuffer()`, when updating.
    pub(crate) fn abort_updating(&self) {
        if !self.updating.get() {
            return;
        }
        if self.queued_append.borrow_mut().take().is_none() && self.backend_busy.get() {
            // The backend goes on with the append; nobody waits for its end.
            self.swallowed_update_ends
                .set(self.swallowed_update_ends.get() + 1);
        }
        self.remove_generation.set(self.remove_generation.get() + 1);
        self.updating.set(false);
        self.queue_event("abort");
        self.queue_event("updateend");
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn Mode(&self) -> AppendMode {
        self.mode.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn SetMode(&self, value: AppendMode) -> ErrorResult {
        let media_source = self.check_not_removed_nor_updating()?;
        media_source.reopen_if_ended();
        media_source
            .with_player(|player| {
                player.source_buffer_set_sequence_mode(self.id, value == AppendMode::Sequence)
            })
            .map_err(|_| Error::Type(c"The mode is not supported".to_owned()))?;
        self.mode.set(value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn Updating(&self) -> bool {
        self.updating.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn GetBuffered(&self, cx: &mut JSContext) -> Result<DomRoot<TimeRanges>, Error> {
        if self.media_source.get().is_none() {
            return Err(Error::InvalidState(None));
        }
        let mut ranges = TimeRangesContainer::default();
        for (start, end) in self.buffered_ranges() {
            let _ = ranges.add(start, end);
        }
        Ok(TimeRanges::new(cx, self.global().as_window(), ranges))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn TimestampOffset(&self) -> Finite<f64> {
        Finite::wrap(self.timestamp_offset.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn SetTimestampOffset(&self, value: Finite<f64>) -> ErrorResult {
        let media_source = self.check_not_removed_nor_updating()?;
        media_source.reopen_if_ended();
        let _ = media_source
            .with_player(|player| player.source_buffer_set_timestamp_offset(self.id, *value));
        self.timestamp_offset.set(*value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(self.append_window_start.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn SetAppendWindowStart(&self, value: Finite<f64>) -> ErrorResult {
        let media_source = self.check_not_removed_nor_updating()?;
        if *value < 0. || *value >= self.append_window_end.get() {
            return Err(Error::Type(c"Bad append window start".to_owned()));
        }
        let _ = media_source.with_player(|player| {
            player.source_buffer_set_append_window(self.id, *value, self.append_window_end.get())
        });
        self.append_window_start.set(*value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn AppendWindowEnd(&self) -> f64 {
        self.append_window_end.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn SetAppendWindowEnd(&self, value: f64) -> ErrorResult {
        let media_source = self.check_not_removed_nor_updating()?;
        if value.is_nan() {
            return Err(Error::Type(c"The append window end is NaN".to_owned()));
        }
        if value <= self.append_window_start.get() {
            return Err(Error::Type(c"Bad append window end".to_owned()));
        }
        let _ = media_source.with_player(|player| {
            player.source_buffer_set_append_window(self.id, self.append_window_start.get(), value)
        });
        self.append_window_end.set(value);
        Ok(())
    }

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdatestart
    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdate
    event_handler!(update, GetOnupdate, SetOnupdate);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdateend
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onerror
    event_handler!(error, GetOnerror, SetOnerror);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onabort
    event_handler!(abort, GetOnabort, SetOnabort);

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, data: ArrayBufferViewOrArrayBuffer) -> ErrorResult {
        // The prepare append algorithm.
        // Step 1. If the SourceBuffer has been removed from the sourceBuffers attribute of the
        // parent media source then throw an InvalidStateError exception and abort these steps.
        // Step 2. If the updating attribute equals true, then throw an InvalidStateError
        // exception and abort these steps.
        let media_source = self.check_not_removed_nor_updating()?;
        // Step 4. If the readyState attribute of the parent media source is in the "ended"
        // state then run the following steps: set the readyState attribute of the parent media
        // source to "open", queue a task to fire an event named sourceopen at the parent media
        // source.
        media_source.reopen_if_ended();

        // Step 2 (of appendBuffer). Add data to the end of the [[input buffer]].
        let data = match data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => {
                get_buffer_source_copy((&view).into())
            },
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => {
                get_buffer_source_copy((&buffer).into())
            },
        };
        // Step 5. Asynchronously run the buffer append algorithm. A full source buffer throws
        // QuotaExceededError (the coded frame eviction of step 3 of the prepare append
        // algorithm is the backend's: it evicts frames 5 seconds before the position).
        let result = if self.backend_busy.get() {
            *self.queued_append.borrow_mut() = Some(data);
            Ok(())
        } else {
            self.send_append(&media_source, data)
        };
        if result == Err(PlayerError::MediaSourceQuotaExceeded) {
            return Err(Error::QuotaExceeded {
                quota: None,
                requested: None,
            });
        }
        // Step 3. Set the updating attribute to true.
        self.updating.set(true);
        // Step 4. Queue a task to fire an event named updatestart at this SourceBuffer object.
        self.queue_event("updatestart");
        if result.is_err() {
            self.append_error(&media_source);
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self) -> ErrorResult {
        // Step 1. If this object has been removed from the sourceBuffers attribute of the parent
        // media source then throw an InvalidStateError exception and abort these steps.
        let Some(media_source) = self.media_source.get() else {
            return Err(Error::InvalidState(None));
        };
        // Step 2. If the readyState attribute of the parent media source is not in the "open"
        // state then throw an InvalidStateError exception and abort these steps.
        if media_source.ready_state() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        // Step 4. If the sourceBuffer.updating attribute equals true, then run the following
        // steps: abort the buffer append algorithm, set updating to false, queue abort and
        // updateend.
        self.abort_updating();
        // Steps 6-7. Set appendWindowStart to 0 and appendWindowEnd to positive Infinity.
        self.append_window_start.set(0.);
        self.append_window_end.set(f64::INFINITY);
        let _ = media_source.with_player(|player| {
            player.source_buffer_set_append_window(self.id, 0., f64::INFINITY)
        });
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-changetype>
    fn ChangeType(&self, type_: DOMString) -> ErrorResult {
        let type_ = type_.str().to_string();
        if type_.is_empty() {
            return Err(Error::Type(c"The type is empty".to_owned()));
        }
        let media_source = self.check_not_removed_nor_updating()?;
        if !servo_media::ServoMedia::get().is_media_source_type_supported(&type_) {
            return Err(Error::NotSupported(None));
        }
        media_source.reopen_if_ended();
        media_source
            .with_player(|player| player.source_buffer_change_type(self.id, &type_))
            .map_err(|_| Error::NotSupported(None))?;
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, start: Finite<f64>, end: f64) -> ErrorResult {
        let media_source = self.check_not_removed_nor_updating()?;
        // Step 3. If duration equals NaN, then throw a TypeError exception.
        // Step 4. If start is negative or greater than duration, then throw a TypeError.
        // Step 5. If end is less than or equal to start or end equals NaN, then throw a TypeError.
        let duration = media_source.Duration();
        if duration.is_nan() || *start < 0. || *start > duration || end.is_nan() || end <= *start {
            return Err(Error::Type(c"Bad range".to_owned()));
        }
        // Step 6. If the readyState attribute of the parent media source is in the "ended" state
        // then transition it to "open".
        media_source.reopen_if_ended();
        // The range removal algorithm. (gstmse 1.26 keeps the frames: it evicts the old ones
        // itself when an append needs the room.)
        let _ =
            media_source.with_player(|player| player.source_buffer_remove(self.id, *start, end));
        // Step 4-5 (range removal). Set updating to true and queue updatestart.
        self.updating.set(true);
        self.queue_event("updatestart");
        // Steps 7-9 (range removal), as a task: set updating to false, queue update and updateend.
        let this = Trusted::new(self);
        let generation = self.remove_generation.get();
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_remove_end: move || {
                let this = this.root();
                if this.remove_generation.get() != generation || !this.updating.get() {
                    return;
                }
                this.updating.set(false);
                this.queue_event("update");
                this.queue_event("updateend");
            }));
        Ok(())
    }
}
