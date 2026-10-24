/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use embedder_traits::EmbedderMsg;
use embedder_traits::user_contents::HostChannelSender;
use js::context::JSContext;
use js::realm::CurrentRealm;
use js::rust::HandleObject;
use script_bindings::interfaces::HostChannelHelpers;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use script_bindings::str::DOMString;

use crate::dom::bindings::codegen::Bindings::HostChannelBinding::HostChannelMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;
use crate::dom::window::Window;

/// `navigator.hostChannel`: a channel from page script to the embedder, for the documents that
/// the embedder names (`UserContentManager::set_host_channel`).
#[dom_struct]
pub(crate) struct HostChannel {
    reflector_: Reflector,
}

impl HostChannel {
    fn new_inherited() -> HostChannel {
        HostChannel {
            reflector_: Reflector::new(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<HostChannel> {
        reflect_dom_object_with_cx(Box::new(HostChannel::new_inherited()), global, cx)
    }
}

impl HostChannelMethods<crate::DomTypeHolder> for HostChannel {
    /// Send `message` to the embedder (`WebViewDelegate::notify_host_channel_message`), with the
    /// URL, the origin, and whether this is a top-level document.
    fn PostMessage(&self, message: DOMString) {
        let global = self.global();
        let Some(window) = global.downcast::<Window>() else {
            return;
        };
        // The document may no longer be the one the channel is for (checked again on each
        // message, not only when `navigator.hostChannel` was first looked up).
        if !window.has_host_channel() {
            return;
        }
        let sender = HostChannelSender {
            url: window.get_url().into_url(),
            origin: global.origin().immutable().ascii_serialization().into_owned(),
            top_level: window.is_top_level(),
        };
        global.send_to_embedder(EmbedderMsg::HostChannelMessage(
            window.webview_id(),
            sender,
            String::from(message.str()),
        ));
    }
}

impl HostChannelHelpers for HostChannel {
    /// Exposed only to the documents that the embedder named for this `WebView`.
    fn is_exposed(cx: &mut JSContext, _global: HandleObject) -> bool {
        let mut realm = CurrentRealm::assert(cx);
        let global_scope = GlobalScope::from_current_realm(&mut realm);
        global_scope
            .downcast::<Window>()
            .is_some_and(|window| window.has_host_channel())
    }
}
