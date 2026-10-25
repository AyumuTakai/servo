/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/media-capabilities/
// Without the members Servo cannot answer (HDR, scalability, key systems: Servo has no EME).

dictionary MediaConfiguration {
  VideoConfiguration video;
  AudioConfiguration audio;
};

dictionary MediaDecodingConfiguration : MediaConfiguration {
  required MediaDecodingType type;
};

dictionary MediaEncodingConfiguration : MediaConfiguration {
  required MediaEncodingType type;
};

enum MediaDecodingType { "file", "media-source", "webrtc" };

enum MediaEncodingType { "record", "webrtc" };

dictionary VideoConfiguration {
  required DOMString contentType;
  required [EnforceRange] unsigned long width;
  required [EnforceRange] unsigned long height;
  required [EnforceRange] unsigned long long bitrate;
  required double framerate;
  boolean hasAlphaChannel;
};

dictionary AudioConfiguration {
  required DOMString contentType;
  DOMString channels;
  [EnforceRange] unsigned long long bitrate;
  [EnforceRange] unsigned long samplerate;
  boolean spatialRendering;
};

dictionary MediaCapabilitiesInfo {
  required boolean supported;
  required boolean smooth;
  required boolean powerEfficient;
};

dictionary MediaCapabilitiesDecodingInfo : MediaCapabilitiesInfo {
};

[Pref="dom_media_capabilities_enabled", Exposed=Window]
interface MediaCapabilities {
  [NewObject] Promise<MediaCapabilitiesDecodingInfo> decodingInfo(
      MediaDecodingConfiguration configuration);
  [NewObject] Promise<MediaCapabilitiesInfo> encodingInfo(
      MediaEncodingConfiguration configuration);
};

partial interface Navigator {
  [SameObject, Pref="dom_media_capabilities_enabled"] readonly attribute MediaCapabilities mediaCapabilities;
};
