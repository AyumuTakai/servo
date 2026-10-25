/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use malloc_size_of::MallocSizeOfOps;
use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};
use url::Url;

static USER_CONTENT_MANAGER_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct UserContentManagerId(u32);

impl UserContentManagerId {
    pub fn next() -> Self {
        Self(USER_CONTENT_MANAGER_ID.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Debug, Default, Deserialize, MallocSizeOf, Serialize)]
pub struct UserContents {
    pub scripts: Vec<UserScript>,
    pub stylesheets: Vec<UserStyleSheet>,
    /// The channel from page script to the embedder (`navigator.hostChannel`), if any.
    pub host_channel: Option<HostChannel>,
}

/// Which documents get `navigator.hostChannel.postMessage(message)`, a channel from page script
/// to the embedder (`WebViewDelegate::notify_host_channel_message`). Documents that do not match
/// have no `navigator.hostChannel` at all.
#[derive(Clone, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct HostChannel {
    /// Only documents with this origin get the channel (its ASCII serialization, for example
    /// `http://app.localhost`). `None`: documents of any origin.
    pub origin: Option<String>,
    /// Only top-level documents get the channel (not those in iframes).
    pub top_level_only: bool,
}

/// The document that sent a message through `navigator.hostChannel`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostChannelSender {
    /// The URL of the document.
    pub url: Url,
    /// The ASCII serialization of the document's origin (`null` for an opaque origin).
    pub origin: String,
    /// Whether the document is a top-level document (not in an iframe).
    pub top_level: bool,
}

impl UserContents {
    pub fn new() -> Self {
        UserContents::default()
    }
}

static USER_STYLE_SHEET_ID: AtomicU32 = AtomicU32::new(1);

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, MallocSizeOf, PartialEq, Serialize)]
pub struct UserStyleSheetId(u32);

impl UserStyleSheetId {
    fn next() -> Self {
        UserStyleSheetId(USER_STYLE_SHEET_ID.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct UserStyleSheet {
    id: UserStyleSheetId,
    source: String,
    url: Url,
}

impl UserStyleSheet {
    /// Create a new `UserStyleSheet` for the given source and url representing its location. The
    /// `url` can be a local file url.
    pub fn new(source: String, url: Url) -> Self {
        Self {
            id: UserStyleSheetId::next(),
            source,
            url,
        }
    }

    #[doc(hidden)]
    pub fn id(&self) -> UserStyleSheetId {
        self.id
    }

    /// Return a reference to the source string of this `UserStyleSheet`.
    pub fn source(&self) -> &String {
        &self.source
    }

    /// Return the source url of this `UserStyleSheet`.
    pub fn url(&self) -> Url {
        self.url.clone()
    }
}

static USER_SCRIPT_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, MallocSizeOf, PartialEq, Serialize)]
pub struct UserScriptId(u32);

impl UserScriptId {
    fn next() -> Self {
        UserScriptId(USER_SCRIPT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UserScript {
    id: UserScriptId,
    script: String,
    source_file: Option<PathBuf>,
}

impl UserScript {
    /// Create a new `UserStyleSheet` for the given source string and an optional `PathBuf` representing
    /// the location of the script on disk.
    pub fn new(script: String, source_file: Option<PathBuf>) -> Self {
        Self {
            id: UserScriptId::next(),
            script,
            source_file,
        }
    }

    #[doc(hidden)]
    pub fn id(&self) -> UserScriptId {
        self.id
    }

    /// Returns the optional path that represents the location of this `UserScript`.
    pub fn source_file(&self) -> Option<PathBuf> {
        self.source_file.clone()
    }

    // Returns a reference to the source string of this `UserScript`.
    pub fn script(&self) -> &String {
        &self.script
    }
}

// Maybe we should implement `MallocSizeOf` for `PathBuf` in `malloc_size_of` crate?
impl malloc_size_of::MallocSizeOf for UserScript {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut sum = 0;
        sum += self.id.0.size_of(ops);
        sum += self.script.size_of(ops);
        if let Some(path) = &self.source_file {
            sum += unsafe { ops.malloc_size_of(path.as_path()) };
        }
        sum
    }
}

impl<T: Into<String>> From<T> for UserScript {
    fn from(script: T) -> Self {
        UserScript::new(script.into(), None)
    }
}
