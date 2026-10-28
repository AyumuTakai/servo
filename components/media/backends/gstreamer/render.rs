/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use glib::prelude::*;
use servo_media_gstreamer_render::Render;
use servo_media_player::PlayerError;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::video::{Buffer, VideoFrame, VideoFrameData};

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

struct GStreamerBuffer {
    frame: gstreamer_video::VideoFrame<gstreamer_video::video_frame::Readable>,
}

impl Buffer for GStreamerBuffer {
    fn to_vec(&self) -> Option<VideoFrameData> {
        let data = self.frame.plane_data(0).ok()?;
        Some(VideoFrameData::Raw(Arc::new(data.to_vec())))
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        self.frame.plane_data(0).ok()
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
        let build = || -> Option<gstreamer::Element> {
            let post = gstreamer::ElementFactory::make("vapostproc").build().ok()?;
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
