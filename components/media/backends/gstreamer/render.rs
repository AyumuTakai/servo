/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use glib::prelude::*;
use servo_media_gstreamer_render::Render;
use servo_media_player::PlayerError;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::video::{
    Buffer, DmaBufInfo, Nv12Info, VideoFrame, VideoFrameData, YuvMatrix,
};

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
mod platform {
    extern crate servo_media_gstreamer_render_unix;
    pub use self::servo_media_gstreamer_render_unix::RenderUnix as Render;
    use super::*;

    pub fn create_render(gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new(gl_context)
    }
}

#[cfg(target_os = "android")]
mod platform {
    extern crate servo_media_gstreamer_render_android;
    pub use self::servo_media_gstreamer_render_android::RenderAndroid as Render;
    use super::*;

    pub fn create_render(gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new(gl_context)
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "android",
)))]
mod platform {
    use servo_media_gstreamer_render::Render as RenderTrait;
    use servo_media_player::PlayerError;
    use servo_media_player::context::PlayerGLContext;
    use servo_media_player::video::VideoFrame;

    pub struct RenderDummy();
    pub type Render = RenderDummy;

    pub fn create_render(_: Box<dyn PlayerGLContext>) -> Option<RenderDummy> {
        None
    }

    impl RenderTrait for RenderDummy {
        fn is_gl(&self) -> bool {
            false
        }

        fn build_frame(&self, _: gstreamer::Sample) -> Option<VideoFrame> {
            None
        }

        fn build_video_sink(
            &self,
            _: &gstreamer::Element,
            _: &gstreamer::Element,
        ) -> Result<(), PlayerError> {
            Err(PlayerError::Backend(
                "Not available videosink decorator".to_owned(),
            ))
        }
    }
}

#[link(name = "gstallocators-1.0")]
unsafe extern "C" {
    fn gst_is_dmabuf_memory(mem: *mut gstreamer::ffi::GstMemory) -> glib::ffi::gboolean;
    fn gst_dmabuf_memory_get_fd(mem: *mut gstreamer::ffi::GstMemory) -> std::os::raw::c_int;
}

/// A decoded frame in GPU memory (`video/x-raw(memory:DMABuf)`), which is never mapped.
struct GStreamerDmaBuf {
    #[allow(dead_code)]
    buffer: gstreamer::Buffer,
    info: DmaBufInfo,
}

impl Buffer for GStreamerDmaBuf {
    fn to_vec(&self) -> Option<VideoFrameData> {
        Some(VideoFrameData::DmaBuf)
    }

    fn dmabuf(&self) -> Option<DmaBufInfo> {
        Some(self.info)
    }
}

/// `drm-format` of the caps (`AR24:0x0100000000000002`): the fourcc and the modifier.
fn parse_drm_format(text: &str) -> Option<(u32, u64)> {
    let (fourcc, modifier) = match text.split_once(':') {
        Some((fourcc, modifier)) => (
            fourcc,
            u64::from_str_radix(modifier.trim_start_matches("0x"), 16).ok()?,
        ),
        None => (text, 0),
    };
    let bytes = fourcc.as_bytes();
    if bytes.len() != 4 {
        return None;
    }
    Some((
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        modifier,
    ))
}

fn dmabuf_frame_from_sample(sample: &gstreamer::Sample) -> Option<VideoFrame> {
    use gstreamer_video::prelude::*;
    let caps = sample.caps()?;
    if !caps
        .features(0)
        .is_some_and(|features| features.contains("memory:DMABuf"))
    {
        return None;
    }
    let structure = caps.structure(0)?;
    let width = structure.get::<i32>("width").ok()?;
    let height = structure.get::<i32>("height").ok()?;
    let (fourcc, modifier) = structure
        .get::<&str>("drm-format")
        .ok()
        .and_then(parse_drm_format)
        // ARGB8888: what `vapostproc` makes of BGRA.
        .unwrap_or((u32::from_le_bytes(*b"AR24"), 0));
    let buffer = sample.buffer_owned()?;
    let fd = unsafe {
        let memory = buffer.peek_memory(0).as_mut_ptr();
        if gst_is_dmabuf_memory(memory) == 0 {
            return None;
        }
        gst_dmabuf_memory_get_fd(memory)
    };
    if fd < 0 {
        return None;
    }
    let (pitch, offset) = match buffer.meta::<gstreamer_video::VideoMeta>() {
        Some(meta) => (meta.stride()[0] as u32, meta.offset()[0] as u32),
        None => (width as u32 * 4, 0),
    };
    VideoFrame::new(
        width,
        height,
        Arc::new(GStreamerDmaBuf {
            buffer,
            info: DmaBufInfo {
                fd,
                fourcc,
                pitch,
                offset,
                modifier,
            },
        }),
    )
}

struct GStreamerBuffer {
    frame: gstreamer_video::VideoFrame<gstreamer_video::video_frame::Readable>,
}

impl Buffer for GStreamerBuffer {
    fn to_vec(&self) -> Option<VideoFrameData> {
        let data = self.frame.plane_data(0).ok()?;
        Some(VideoFrameData::Raw(Arc::new(data.to_vec())))
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        use gstreamer_video::prelude::VideoFrameExt;
        if self.frame.format() == gstreamer_video::VideoFormat::Nv12 {
            return None;
        }
        self.frame.plane_data(0).ok()
    }

    fn nv12_planes(&self) -> Option<(&[u8], &[u8], Nv12Info)> {
        use gstreamer_video::prelude::VideoFrameExt;
        if self.frame.format() != gstreamer_video::VideoFormat::Nv12 {
            return None;
        }
        let colorimetry = self.frame.info().colorimetry();
        let matrix = match colorimetry.matrix() {
            gstreamer_video::VideoColorMatrix::Bt601 => YuvMatrix::Bt601,
            gstreamer_video::VideoColorMatrix::Bt709 => YuvMatrix::Bt709,
            gstreamer_video::VideoColorMatrix::Bt2020 => YuvMatrix::Bt2020,
            // Not said: the usual choice (HD is BT.709, SD is BT.601).
            _ if self.frame.height() >= 720 => YuvMatrix::Bt709,
            _ => YuvMatrix::Bt601,
        };
        let info = Nv12Info {
            y_stride: self.frame.plane_stride()[0],
            uv_stride: self.frame.plane_stride()[1],
            matrix,
            full_range: colorimetry.range() == gstreamer_video::VideoColorRange::Range0_255,
        };
        Some((self.frame.plane_data(0).ok()?, self.frame.plane_data(1).ok()?, info))
    }
}

pub struct GStreamerRender {
    render: Option<platform::Render>,
}

impl GStreamerRender {
    pub fn new(gl_context: Box<dyn PlayerGLContext>) -> Self {
        GStreamerRender {
            render: platform::create_render(gl_context),
        }
    }

    pub fn is_gl(&self) -> bool {
        if let Some(render) = self.render.as_ref() {
            render.is_gl()
        } else {
            false
        }
    }

    pub fn get_frame_from_sample(&self, sample: gstreamer::Sample) -> Option<VideoFrame> {
        if let Some(render) = self.render.as_ref() {
            render.build_frame(sample)
        } else {
            if let Some(frame) = dmabuf_frame_from_sample(&sample) {
                return Some(frame);
            }
            let buffer = sample.buffer_owned()?;
            let caps = sample.caps()?;
            let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
            let frame = gstreamer_video::VideoFrame::from_buffer_readable(buffer, &info).ok()?;

            VideoFrame::new(
                info.width() as i32,
                info.height() as i32,
                Arc::new(GStreamerBuffer { frame }),
            )
        }
    }

    /// With hardware decoding kept (`SERVO_MEDIA_KEEP_VA`), converts the decoded frames to BGRA
    /// on the GPU (`vapostproc ! appsink`) instead of `playsink`'s `videoconvert`, which converts on
    /// the CPU (about 14 ms per 1080p frame on a low-end CPU, and the limit of the frame rate).
    /// Returns false (nothing changed) if that is not possible.
    fn set_va_converting_sink(pipeline: &gstreamer::Element, appsink: &gstreamer_app::AppSink) -> bool {
        if std::env::var_os("SERVO_MEDIA_KEEP_VA").is_none() ||
            std::env::var_os("SERVO_MEDIA_VA_CONVERT").is_some_and(|v| v == "0")
        {
            return false;
        }
        use gstreamer::prelude::*;
        // `SERVO_MEDIA_NV12=1`: NV12 (1.5 bytes a pixel) goes on to WebRender, which converts it on the GPU.
        // Off by default: on Intel (Alder Lake-N), downloading the decoded NV12 costs the CPU more than
        // downloading the BGRA that `vapostproc` converts (child 69% -> 114%, parent 42% -> 55%).
        if std::env::var_os("SERVO_MEDIA_NV12").is_some_and(|v| v == "1") {
            let caps = gstreamer::Caps::builder("video/x-raw")
                .field("format", gstreamer_video::VideoFormat::Nv12.to_str())
                .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
                .build();
            appsink.set_caps(Some(&caps));
        }
        let build = || -> Option<gstreamer::Element> {
            let post = gstreamer::ElementFactory::make("vapostproc").build().ok()?;
            // `SERVO_MEDIA_DMABUF=1`: the frames stay in GPU memory as dmabufs, which the process that paints
            // imports (nothing is read back by the CPU). Needs `SERVO_MEDIA_DMABUF_SOCKET` (the embedder sets
            // it). The sink asks for the 4-byte RGB formats (with the modifiers) that `vapostproc` offers as
            // dmabufs (it would give NV12, which has two planes); if there are none, or the negotiation fails,
            // it gives BGRA in system memory as before.
            if std::env::var_os("SERVO_MEDIA_DMABUF").is_some_and(|v| v == "1") &&
                std::env::var_os("SERVO_MEDIA_DMABUF_SOCKET").is_some()
            {
                let mut formats: Vec<String> = vec![];
                let offered = post.static_pad("src")?.query_caps(None);
                for (structure, features) in offered.iter_with_features() {
                    if !features.contains("memory:DMABuf") {
                        continue;
                    }
                    if let Ok(list) = structure.get::<gstreamer::List>("drm-format") {
                        for value in list.iter() {
                            if let Ok(text) = value.get::<&str>() {
                                if ["AR24", "XR24", "AB24", "XB24"]
                                    .iter()
                                    .any(|prefix| text.starts_with(prefix))
                                {
                                    formats.push(text.to_owned());
                                }
                            }
                        }
                    } else if let Ok(text) = structure.get::<&str>("drm-format") {
                        if ["AR24", "XR24", "AB24", "XB24"]
                            .iter()
                            .any(|prefix| text.starts_with(prefix))
                        {
                            formats.push(text.to_owned());
                        }
                    }
                }
                if !formats.is_empty() {
                    let mut caps = gstreamer::Caps::new_empty();
                    {
                        let caps = caps.get_mut().unwrap();
                        caps.append_structure_full(
                            gstreamer::Structure::builder("video/x-raw")
                                .field("format", "DMA_DRM")
                                .field(
                                    "drm-format",
                                    gstreamer::List::new(formats.iter().map(String::as_str)),
                                )
                                .build(),
                            Some(gstreamer::CapsFeatures::new(["memory:DMABuf"])),
                        );
                        caps.append_structure(
                            gstreamer::Structure::builder("video/x-raw")
                                .field("format", gstreamer_video::VideoFormat::Bgra.to_str())
                                .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
                                .build(),
                        );
                    }
                    appsink.set_caps(Some(&caps));
                    // `vapostproc` gives dmabufs only to a sink that says it can take `VideoMeta` (the strides).
                    if let Some(pad) = appsink.static_pad("sink") {
                        pad.add_probe(gstreamer::PadProbeType::QUERY_DOWNSTREAM, |_, info| {
                            if let Some(gstreamer::PadProbeData::Query(query)) = info.data.as_mut() {
                                if let gstreamer::QueryViewMut::Allocation(allocation) =
                                    query.view_mut()
                                {
                                    allocation.add_allocation_meta::<gstreamer_video::VideoMeta>(None);
                                }
                            }
                            gstreamer::PadProbeReturn::Ok
                        });
                    }
                }
            }
            let bin = gstreamer::Bin::new();
            bin.add_many([&post, appsink.upcast_ref::<gstreamer::Element>()]).ok()?;
            post.link(appsink).ok()?;
            let pad = gstreamer::GhostPad::with_target(&post.static_pad("sink")?).ok()?;
            bin.add_pad(&pad).ok()?;
            Some(bin.upcast())
        };
        let Some(bin) = build() else {
            return false;
        };
        pipeline.set_property("video-sink", &bin);
        // `playsink` would put its own `videoconvert` in front of the sink without this.
        let flags = pipeline.property_value("flags");
        if let Some(class) = glib::FlagsClass::with_type(flags.type_()) {
            if let Some(flags) = class
                .builder_with_value(flags)
                .and_then(|builder| builder.set_by_nick("native-video").build())
            {
                pipeline.set_property_from_value("flags", &flags);
            }
        }
        true
    }

    pub fn setup_video_sink(
        &self,
        pipeline: &gstreamer::Element,
    ) -> Result<gstreamer_app::AppSink, PlayerError> {
        let appsink = gstreamer::ElementFactory::make("appsink")
            .build()
            .map_err(|error| PlayerError::Backend(format!("appsink creation failed: {error:?}")))?
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();

        if let Some(render) = self.render.as_ref() {
            render.build_video_sink(appsink.upcast_ref::<gstreamer::Element>(), pipeline)?
        } else {
            let caps = gstreamer::Caps::builder("video/x-raw")
                .field("format", gstreamer_video::VideoFormat::Bgra.to_str())
                .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
                .build();

            appsink.set_caps(Some(&caps));
            if !Self::set_va_converting_sink(pipeline, &appsink) {
                pipeline.set_property("video-sink", &appsink);
            }
        };

        Ok(appsink)
    }
}
