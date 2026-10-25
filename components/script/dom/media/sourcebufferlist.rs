/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::SourceBufferListBinding::SourceBufferListMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::sourcebuffer::SourceBuffer;
use crate::dom::window::Window;

/// <https://w3c.github.io/media-source/#sourcebufferlist>
#[dom_struct]
pub(crate) struct SourceBufferList {
    eventtarget: EventTarget,
    buffers: DomRefCell<Vec<Dom<SourceBuffer>>>,
}

impl SourceBufferList {
    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<SourceBufferList> {
        reflect_dom_object(
            cx,
            Box::new(SourceBufferList {
                eventtarget: EventTarget::new_inherited(),
                buffers: DomRefCell::new(vec![]),
            }),
            window,
        )
    }

    fn queue_event(&self, name: &str) {
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue_simple_event(self.upcast(), Atom::from(name));
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = DomRoot<SourceBuffer>> {
        let buffers: Vec<_> = self
            .buffers
            .borrow()
            .iter()
            .map(|buffer| DomRoot::from_ref(&**buffer))
            .collect();
        buffers.into_iter()
    }

    pub(crate) fn contains(&self, buffer: &SourceBuffer) -> bool {
        self.buffers.borrow().iter().any(|b| &**b == buffer)
    }

    pub(crate) fn push(&self, buffer: &SourceBuffer) {
        self.buffers.borrow_mut().push(Dom::from_ref(buffer));
        self.queue_event("addsourcebuffer");
    }

    pub(crate) fn remove(&self, buffer: &SourceBuffer) {
        let mut buffers = self.buffers.borrow_mut();
        let Some(index) = buffers.iter().position(|b| &**b == buffer) else {
            return;
        };
        buffers.remove(index);
        drop(buffers);
        self.queue_event("removesourcebuffer");
    }

    /// Empties the list without events (detaching the media source).
    pub(crate) fn take_all(&self) -> Vec<DomRoot<SourceBuffer>> {
        let buffers = self.iter().collect();
        self.buffers.borrow_mut().clear();
        buffers
    }
}

impl SourceBufferListMethods<crate::DomTypeHolder> for SourceBufferList {
    /// <https://w3c.github.io/media-source/#dom-sourcebufferlist-length>
    fn Length(&self) -> u32 {
        self.buffers.borrow().len() as u32
    }

    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onaddsourcebuffer
    event_handler!(addsourcebuffer, GetOnaddsourcebuffer, SetOnaddsourcebuffer);

    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onremovesourcebuffer
    event_handler!(
        removesourcebuffer,
        GetOnremovesourcebuffer,
        SetOnremovesourcebuffer
    );

    /// <https://w3c.github.io/media-source/#dfn-sourcebufferlist-getter>
    fn IndexedGetter(&self, index: u32) -> Option<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .get(index as usize)
            .map(|buffer| DomRoot::from_ref(&**buffer))
    }
}
