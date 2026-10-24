/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// A channel from page script to the embedder (not a web standard). Only the documents that the
// embedder names (`UserContentManager::set_host_channel`: an origin, top-level or not) have it;
// other documents, including iframes and pages of other origins in the same WebView, do not see
// `navigator.hostChannel` at all.
[Exposed=Window,
Func="HostChannel::is_exposed"]
interface HostChannel {
    undefined postMessage(DOMString message);
};

partial interface Navigator {
    [Func="HostChannel::is_exposed"]
    readonly attribute HostChannel hostChannel;
};
