/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Limits what web content can reach in the GStreamer registry.
//!
//! `playbin3`, `decodebin` and `parsebin` pick demuxers, parsers and decoders by rank from
//! whatever is installed, after typefinding the bytes a page supplies (the response's
//! `Content-Type` plays no part). On a desktop system that is hundreds of parsers for formats no
//! web page needs. This module:
//!
//! 1. Always removes the element factories that fetch URIs by themselves (`souphttpsrc`,
//!    `filesrc`, `rtspsrc`, ...). Media must come through Servo's fetch (`servosrc`), which applies
//!    CORS, CSP and cookies; an element that opens sockets or local files on its own bypasses all
//!    of that (for example a playlist demuxer requesting segments from the local network).
//! 2. When the embedder sets `SERVO_MEDIA_AUTOPLUG_ALLOW` (a comma-separated list of element
//!    factory names, where a trailing `*` matches a prefix), gives every other demuxer, parser,
//!    depayloader, decryptor and decoder the rank `NONE`, so the autopluggers never choose it.
//!    Bins (`decodebin`, `parsebin`, ...) are left alone: Servo creates them by name.

use gstreamer::prelude::*;
use log::info;

/// URI protocols whose source elements stay: Servo's own sources (pages reach media only
/// through them), and `appsrc`, which Servo creates by name.
const INTERNAL_URI_PROTOCOLS: &[&str] = &["servosrc", "appsrc", "kyousei-mse", "mse"];

/// Environment variable with the embedder's allowlist of autoplugged element factories.
pub(crate) const ALLOWLIST_ENV: &str = "SERVO_MEDIA_AUTOPLUG_ALLOW";

/// `allow` is the value of `ALLOWLIST_ENV`, if set.
pub(crate) fn restrict_registry(allow: Option<&str>) {
    let registry = gstreamer::Registry::get();
    let allow: Option<Vec<&str>> = allow.map(|list| {
        list.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect()
    });

    let mut removed = 0;
    let mut demoted = 0;
    for feature in registry.features(gstreamer::ElementFactory::static_type()) {
        let Ok(factory) = feature.clone().downcast::<gstreamer::ElementFactory>() else {
            continue;
        };
        if fetches_by_itself(&factory) {
            registry.remove_feature(&feature);
            removed += 1;
            continue;
        }
        let Some(allow) = &allow else {
            continue;
        };
        if !factory.has_type(gstreamer::ElementFactoryType::DECODABLE) ||
            factory.klass().contains("Bin") ||
            feature.rank() == gstreamer::Rank::NONE
        {
            continue;
        }
        let name = factory.name();
        if !allow.iter().any(|pattern| matches(pattern, &name)) {
            feature.set_rank(gstreamer::Rank::NONE);
            demoted += 1;
        }
    }
    info!(
        "media: removed {removed} URI sources; {demoted} autoplugged elements outside {ALLOWLIST_ENV}"
    );
}

/// A source element that reads a URI by itself (any protocol but Servo's own).
fn fetches_by_itself(factory: &gstreamer::ElementFactory) -> bool {
    factory.uri_type() == gstreamer::URIType::Src &&
        factory
            .uri_protocols()
            .iter()
            .any(|protocol| !INTERNAL_URI_PROTOCOLS.contains(&protocol.as_str()))
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

#[cfg(test)]
mod tests {
    use gstreamer::prelude::*;

    use super::{matches, restrict_registry};

    #[test]
    fn only_allowed_elements_are_autoplugged() {
        gstreamer::init().unwrap();
        restrict_registry(Some("qtdemux, h264parse"));
        let registry = gstreamer::Registry::get();
        let rank = |name: &str| registry.lookup_feature(name).map(|feature| feature.rank());
        // Elements that fetch by themselves are gone; Servo's own sources are not touched.
        assert_eq!(rank("souphttpsrc"), None);
        assert_eq!(rank("filesrc"), None);
        assert!(gstreamer::ElementFactory::find("appsrc").is_some());
        // Allowed elements keep their rank; other decodable ones are never autoplugged.
        if let Some(rank) = rank("qtdemux") {
            assert_ne!(rank, gstreamer::Rank::NONE);
        }
        if let Some(rank) = rank("matroskademux") {
            assert_eq!(rank, gstreamer::Rank::NONE);
        }
        // Bins are created by name and keep working.
        assert!(gstreamer::ElementFactory::make("decodebin").build().is_ok());
    }

    #[test]
    fn allowlist_patterns() {
        assert!(matches("qtdemux", "qtdemux"));
        assert!(!matches("qtdemux", "qtdemux2"));
        assert!(matches("va*", "vah264dec"));
        assert!(!matches("va*", "avdec_h264"));
    }
}
