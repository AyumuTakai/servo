/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::{Mutex, OnceLock, PoisonError};

use gstreamer;
use gstreamer::caps::NoFeature;
use gstreamer::prelude::*;
use servo_media_streams::MediaStreamType;
use servo_media_streams::capture::*;
use servo_media_streams::registry::MediaStreamId;

use crate::media_stream::GStreamerMediaStream;

trait AddToCaps {
    type Bound;
    fn add_to_caps(
        &self,
        name: &str,
        min: Self::Bound,
        max: Self::Bound,
        builder: gstreamer::caps::Builder<NoFeature>,
    ) -> Option<gstreamer::caps::Builder<NoFeature>>;
}

impl AddToCaps for Constrain<u32> {
    type Bound = u32;
    fn add_to_caps(
        &self,
        name: &str,
        min: u32,
        max: u32,
        builder: gstreamer::caps::Builder<NoFeature>,
    ) -> Option<gstreamer::caps::Builder<NoFeature>> {
        match self {
            Constrain::Value(v) => Some(builder.field(name, into_i32(*v))),
            Constrain::Range(r) => {
                let min = into_i32(r.min.unwrap_or(min));
                let max = into_i32(r.max.unwrap_or(max));
                let range = gstreamer::IntRange::<i32>::new(min, max);

                // TODO: Include the ideal caps value in the caps, needs a refactor
                //       of the AddToCaps trait
                Some(builder.field(name, range))
            },
        }
    }
}

fn into_i32(x: u32) -> i32 {
    if x > i32::MAX as u32 {
        i32::MAX
    } else {
        x as i32
    }
}

impl AddToCaps for Constrain<f64> {
    type Bound = i32;
    fn add_to_caps<'a>(
        &self,
        name: &str,
        min: i32,
        max: i32,
        builder: gstreamer::caps::Builder<NoFeature>,
    ) -> Option<gstreamer::caps::Builder<NoFeature>> {
        match self {
            Constrain::Value(v) => {
                Some(builder.field(name, gstreamer::Fraction::approximate_f64(*v)?))
            },
            Constrain::Range(r) => {
                let min = r
                    .min
                    .and_then(gstreamer::Fraction::approximate_f64)
                    .unwrap_or(gstreamer::Fraction::new(min, 1));
                let max = r
                    .max
                    .and_then(gstreamer::Fraction::approximate_f64)
                    .unwrap_or(gstreamer::Fraction::new(max, 1));
                let range = gstreamer::FractionRange::new(min, max);
                // TODO: Include the ideal caps value in the caps, needs a refactor
                //       of the AddToCaps trait
                Some(builder.field(name, range))
            },
        }
    }
}

// TODO(Manishearth): Should support a set of constraints
fn into_caps(set: MediaTrackConstraintSet, format: &str) -> Option<gstreamer::Caps> {
    let mut builder = gstreamer::Caps::builder(format);
    if let Some(w) = set.width {
        builder = w.add_to_caps("width", 0, 1000000, builder)?;
    }
    if let Some(h) = set.height {
        builder = h.add_to_caps("height", 0, 1000000, builder)?;
    }
    if let Some(aspect) = set.aspect {
        builder = aspect.add_to_caps("pixel-aspect-ratio", 0, 1000000, builder)?;
    }
    if let Some(fr) = set.frame_rate {
        builder = fr.add_to_caps("framerate", 0, 1000000, builder)?;
    }
    if let Some(sr) = set.sample_rate {
        builder = sr.add_to_caps("rate", 0, 1000000, builder)?;
    }
    Some(builder.build())
}

struct GstMediaDevices;

/// 入力・出力の機器の一覧を、調べ直して返す(`enumerateDevices` と `getUserMedia` が使う)。
///
/// 開始していない `DeviceMonitor` は、`devices()` が機器を返すのが、**プロセスで1回目だけ**で、2回目以降は(新しい監視を作っても)空になる
/// (GStreamer 1.26 で確認。起動の直後に `enumerateDevices` を呼ぶと、続く `getUserMedia` が、機器なしになった)。
/// そこで、監視を1つだけ作って持ち、調べるたびに、`start()` → `devices()` → `stop()` する(開始のたびに調べ直すので、抜き差しにも追従する)。
pub(crate) fn probe_devices() -> Vec<gstreamer::Device> {
    static MONITOR: OnceLock<gstreamer::DeviceMonitor> = OnceLock::new();
    static PROBE_LOCK: Mutex<()> = Mutex::new(());
    let _guard = PROBE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let monitor = MONITOR.get_or_init(|| {
        let monitor = gstreamer::DeviceMonitor::new();
        let audio = gstreamer::Caps::builder("audio/x-raw").build();
        let video = gstreamer::Caps::builder("video/x-raw").build();
        monitor.add_filter(Some("Audio/Source"), Some(&audio));
        monitor.add_filter(Some("Audio/Sink"), Some(&audio));
        monitor.add_filter(Some("Video/Source"), Some(&video));
        monitor
    });
    if monitor.start().is_err() {
        return Vec::new();
    }
    let devices = monitor.devices().into_iter().collect();
    monitor.stop();
    devices
}

impl GstMediaDevices {
    pub fn new() -> Self {
        Self
    }

    pub fn get_track(
        &self,
        video: bool,
        constraints: MediaTrackConstraintSet,
    ) -> Option<GstMediaTrack> {
        let (format, class) = if video {
            ("video/x-raw", "Video/Source")
        } else {
            ("audio/x-raw", "Audio/Source")
        };
        let caps = into_caps(constraints, format)?;
        let device = probe_devices().into_iter().find(|d| {
            d.device_class().as_str() == class &&
                d.caps().is_none_or(|device_caps| device_caps.can_intersect(&caps))
        })?;
        let element = device.create_element(None).ok()?;
        Some(GstMediaTrack { element })
    }
}

pub struct GstMediaTrack {
    element: gstreamer::Element,
}

fn create_input_stream(
    stream_type: MediaStreamType,
    constraint_set: MediaTrackConstraintSet,
) -> Option<MediaStreamId> {
    let devices = GstMediaDevices::new();
    devices
        .get_track(stream_type == MediaStreamType::Video, constraint_set)
        .map(|track| {
            let f = match stream_type {
                MediaStreamType::Audio => GStreamerMediaStream::create_audio_from,
                MediaStreamType::Video => GStreamerMediaStream::create_video_from,
            };
            f(track.element)
        })
}

pub fn create_audioinput_stream(constraint_set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
    create_input_stream(MediaStreamType::Audio, constraint_set)
}

pub fn create_videoinput_stream(constraint_set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
    create_input_stream(MediaStreamType::Video, constraint_set)
}
