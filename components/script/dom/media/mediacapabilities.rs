/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_media::{ServoMedia, SupportsMediaType};

use crate::dom::bindings::codegen::Bindings::MediaCapabilitiesBinding::{
    MediaCapabilitiesDecodingInfo, MediaCapabilitiesInfo, MediaCapabilitiesMethods,
    MediaConfiguration, MediaDecodingConfiguration, MediaDecodingType, MediaEncodingConfiguration,
};
use crate::dom::bindings::error::Error;
use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise};

/// <https://w3c.github.io/media-capabilities/#media-capabilities-interface>
///
/// The answers come from the media backend: a configuration is supported when the backend can
/// play its types (`canPlayType`, or `MediaSource.isTypeSupported` for "media-source"). Servo
/// cannot tell whether the playback would be smooth or power efficient, so `smooth` follows
/// `supported` and `powerEfficient` is false. Encoding is not supported.
#[dom_struct]
pub(crate) struct MediaCapabilities {
    reflector_: Reflector,
}

impl MediaCapabilities {
    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<MediaCapabilities> {
        reflect_dom_object_with_cx(
            Box::new(MediaCapabilities {
                reflector_: Reflector::new(),
            }),
            global,
            cx,
        )
    }
}

/// <https://w3c.github.io/media-capabilities/#valid-mime-type>: a type and a subtype, and (for
/// one track) at most one codec.
fn is_valid_mime_type(content_type: &str) -> bool {
    let Ok(mime) = content_type.parse::<mime::Mime>() else {
        return false;
    };
    mime.get_param("codecs")
        .is_none_or(|codecs| !codecs.as_str().contains(','))
}

/// <https://w3c.github.io/media-capabilities/#valid-mediaconfiguration>
fn is_valid_configuration(configuration: &MediaConfiguration) -> bool {
    if configuration.video.is_none() && configuration.audio.is_none() {
        return false;
    }
    configuration
        .video
        .as_ref()
        .is_none_or(|video| is_valid_mime_type(&video.contentType.str()) && *video.framerate > 0.) &&
        configuration
            .audio
            .as_ref()
            .is_none_or(|audio| is_valid_mime_type(&audio.contentType.str()))
}

fn is_type_supported(decoding_type: MediaDecodingType, content_type: &str) -> bool {
    let media = ServoMedia::get();
    match decoding_type {
        MediaDecodingType::File => media.can_play_type(content_type) != SupportsMediaType::No,
        MediaDecodingType::Media_source => media.is_media_source_type_supported(content_type),
        // Servo plays WebRTC streams, but cannot tell the codecs.
        MediaDecodingType::Webrtc => false,
    }
}

impl MediaCapabilitiesMethods<crate::DomTypeHolder> for MediaCapabilities {
    /// <https://w3c.github.io/media-capabilities/#dom-mediacapabilities-decodinginfo>
    fn DecodingInfo(
        &self,
        cx: &mut JSContext,
        configuration: &MediaDecodingConfiguration,
    ) -> RootedPromise {
        let mut realm = CurrentRealm::assert(cx);
        let promise = Promise::new_in_realm(&mut realm);
        // Step 1. If configuration is not a valid MediaDecodingConfiguration, return a Promise
        // rejected with a newly created TypeError.
        if !is_valid_configuration(&configuration.parent) {
            promise.reject_error(
                cx,
                Error::Type(c"The configuration is not valid".to_owned()),
            );
            return promise;
        }
        // Steps 4-7. Create a MediaCapabilitiesDecodingInfo with the configuration, in parallel
        // (the backend answers at once).
        let decoding_type = configuration.type_;
        let supported =
            configuration
                .parent
                .video
                .as_ref()
                .is_none_or(|video| is_type_supported(decoding_type, &video.contentType.str())) &&
                configuration.parent.audio.as_ref().is_none_or(|audio| {
                    is_type_supported(decoding_type, &audio.contentType.str())
                });
        let info = MediaCapabilitiesDecodingInfo {
            parent: MediaCapabilitiesInfo {
                supported,
                smooth: supported,
                powerEfficient: false,
            },
        };
        promise.resolve_native(cx, &info);
        promise
    }

    /// <https://w3c.github.io/media-capabilities/#dom-mediacapabilities-encodinginfo>
    fn EncodingInfo(
        &self,
        cx: &mut JSContext,
        configuration: &MediaEncodingConfiguration,
    ) -> RootedPromise {
        let mut realm = CurrentRealm::assert(cx);
        let promise = Promise::new_in_realm(&mut realm);
        if !is_valid_configuration(&configuration.parent) {
            promise.reject_error(
                cx,
                Error::Type(c"The configuration is not valid".to_owned()),
            );
            return promise;
        }
        let info = MediaCapabilitiesInfo {
            supported: false,
            smooth: false,
            powerEfficient: false,
        };
        promise.resolve_native(cx, &info);
        promise
    }
}
