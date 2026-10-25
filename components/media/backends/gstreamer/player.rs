/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Once};
use std::time;

use byte_slice_cast::AsSliceOf;
use glib;
use glib::prelude::*;
use gstreamer;
use gstreamer_app;
use gstreamer_play;
use gstreamer_play::prelude::*;
use servo_base::generic_channel::{self, GenericCallback, GenericReceiver};
use servo_media::MediaInstanceError;
use servo_media_player::audio::AudioRenderer;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::metadata::Metadata;
use servo_media_player::video::VideoFrameRenderer;
use servo_media_player::{
    MediaSourceEndOfStreamError, PlaybackState, Player, PlayerError, PlayerEvent, SeekLock,
    SeekLockMsg, StreamType,
};
use servo_media_streams::registry::{MediaStreamId, get_stream};
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance};

use super::BACKEND_BASE_TIME;
use crate::media_stream::GStreamerMediaStream;
use crate::media_stream_source::{ServoMediaStreamSrc, register_servo_media_stream_src};
use crate::mse::MediaSource;
use crate::render::GStreamerRender;
use crate::source::{ServoSrc, register_servo_src};

const DEFAULT_MUTED: bool = false;
const DEFAULT_PAUSED: bool = true;
const DEFAULT_CAN_RESUME: bool = false;
const DEFAULT_PLAYBACK_RATE: f64 = 1.0;
const DEFAULT_VOLUME: f64 = 1.0;
const DEFAULT_TIME_RANGES: Vec<Range<f64>> = vec![];

const MAX_BUFFER_SIZE: i32 = 500 * 1024 * 1024;

fn metadata_from_media_info(media_info: &gstreamer_play::PlayMediaInfo) -> Result<Metadata, ()> {
    let dur = media_info.duration();
    let duration = if let Some(dur) = dur {
        let mut nanos = dur.nseconds();
        nanos %= 1_000_000_000;
        let seconds = dur.seconds();
        Some(time::Duration::new(seconds, nanos as u32))
    } else {
        None
    };

    let mut audio_tracks = Vec::new();
    let mut video_tracks = Vec::new();

    let format = media_info
        .container_format()
        .unwrap_or_else(|| glib::GString::from(""))
        .to_string();

    for stream_info in media_info.stream_list() {
        let stream_type = stream_info.stream_type();
        match stream_type.as_str() {
            "audio" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                audio_tracks.push(codec);
            },
            "video" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                video_tracks.push(codec);
            },
            _ => {},
        }
    }

    let mut width: u32 = 0;
    let height: u32 = if media_info.number_of_video_streams() > 0 {
        let first_video_stream = &media_info.video_streams()[0];
        width = first_video_stream.width() as u32;
        first_video_stream.height() as u32
    } else {
        0
    };

    let is_seekable = media_info.is_seekable();
    let is_live = media_info.is_live();
    let title = media_info.title().map(|s| s.as_str().to_string());

    Ok(Metadata {
        duration,
        width,
        height,
        format,
        is_seekable,
        audio_tracks,
        video_tracks,
        is_live,
        title,
    })
}

/// Clears one of the `flags` of playbin3 (by its nick).
fn unset_playbin_flag(pipeline: &gstreamer::Element, nick: &str) -> Result<(), PlayerError> {
    let error = || PlayerError::Backend("FlagsClass creation failed".to_owned());
    let flags = pipeline.property_value("flags");
    let class = glib::FlagsClass::with_type(flags.type_()).ok_or_else(error)?;
    let flags = class
        .builder_with_value(flags)
        .and_then(|builder| builder.unset_by_nick(nick).build())
        .ok_or_else(error)?;
    pipeline.set_property_from_value("flags", &flags);
    Ok(())
}

pub struct GStreamerAudioChunk(gstreamer::buffer::MappedBuffer<gstreamer::buffer::Readable>);
impl AsRef<[f32]> for GStreamerAudioChunk {
    fn as_ref(&self) -> &[f32] {
        self.0.as_ref().as_slice_of::<f32>().unwrap_or_default()
    }
}

#[derive(PartialEq)]
enum PlayerSource {
    Seekable(ServoSrc),
    Stream(ServoMediaStreamSrc),
    /// The `msesrc` element `playbin3` made for `mse://`.
    MediaSource(gstreamer::Element),
}

struct PlayerInner {
    player: gstreamer_play::Play,
    signal_adapter: gstreamer_play::PlaySignalAdapter,
    source: Option<PlayerSource>,
    video_sink: gstreamer_app::AppSink,
    input_size: u64,
    seekable: bool,
    play_state: gstreamer_play::PlayState,
    paused: Cell<bool>,
    can_resume: Cell<bool>,
    playback_rate: Cell<f64>,
    muted: Cell<bool>,
    volume: Cell<f64>,
    stream_type: StreamType,
    last_metadata: Option<Metadata>,
    cat: gstreamer::DebugCategory,
    enough_data: Arc<AtomicBool>,
}

impl PlayerInner {
    pub fn set_input_size(&mut self, size: u64) -> Result<(), PlayerError> {
        // Set input_size to proxy its value, since it
        // could be set by the user before calling .setup().
        self.input_size = size;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_size(if size > 0 {
                size as i64
            } else {
                -1 // live source
            });
        }
        Ok(())
    }

    pub fn set_seekable(&mut self, seekable: bool) -> Result<(), PlayerError> {
        self.seekable = seekable;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_seekable(seekable);
        }
        Ok(())
    }

    pub fn set_mute(&mut self, muted: bool) -> Result<(), PlayerError> {
        if self.muted.get() == muted {
            return Ok(());
        }

        self.muted.set(muted);
        self.player.set_mute(muted);
        Ok(())
    }

    pub fn muted(&self) -> bool {
        self.muted.get()
    }

    pub fn set_playback_rate(&mut self, playback_rate: f64) -> Result<(), PlayerError> {
        if self.stream_type == StreamType::Stream {
            return Err(PlayerError::NonSeekableStream);
        }

        if self.playback_rate.get() == playback_rate {
            return Ok(());
        }

        self.playback_rate.set(playback_rate);

        // The new playback rate will not be passed to the pipeline if the
        // current GstPlay state is less than GST_STATE_PAUSED, which will be
        // set immediately before the initial gstreamer_play_MESSAGE_MEDIA_INFO_UPDATED
        // message is posted to bus.
        if self.last_metadata.is_some() {
            self.player.set_rate(playback_rate);
        }
        Ok(())
    }

    pub fn playback_rate(&self) -> f64 {
        self.playback_rate.get()
    }

    pub fn play(&mut self) -> Result<(), PlayerError> {
        if !self.paused.get() {
            return Ok(());
        }

        self.paused.set(false);
        self.can_resume.set(false);
        self.player.play();
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), PlayerError> {
        self.player.stop();
        self.paused.set(true);
        self.can_resume.set(false);
        self.last_metadata = None;
        self.source = None;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), PlayerError> {
        if self.paused.get() {
            return Ok(());
        }
        if self.stream_type == StreamType::MediaSource {}

        self.paused.set(true);
        self.can_resume.set(true);
        self.player.pause();
        Ok(())
    }

    pub fn paused(&self) -> bool {
        self.paused.get()
    }

    pub fn can_resume(&self) -> bool {
        self.can_resume.get()
    }

    pub fn end_of_stream(&mut self) -> Result<(), PlayerError> {
        match self.source {
            Some(ref mut source) => {
                if let PlayerSource::Seekable(source) = source {
                    source
                        .push_end_of_stream()
                        .map(|_| ())
                        .map_err(|_| PlayerError::EOSFailed)
                } else {
                    Ok(())
                }
            },
            _ => Ok(()),
        }
    }

    pub fn seek(&mut self, time: f64) -> Result<(), PlayerError> {
        if self.stream_type == StreamType::Stream {
            return Err(PlayerError::NonSeekableStream);
        }
        if let Some(ref metadata) = self.last_metadata &&
            let Some(ref duration) = metadata.duration &&
            duration < &time::Duration::new(time as u64, 0)
        {
            gstreamer::warning!(self.cat, obj = &self.player, "Trying to seek out of range");
            return Err(PlayerError::SeekOutOfRange);
        }

        let time = time * 1_000_000_000.;
        self.player
            .seek(gstreamer::ClockTime::from_nseconds(time as u64));
        Ok(())
    }

    pub fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError> {
        if self.volume.get() == volume {
            return Ok(());
        }

        self.volume.set(volume);
        self.player.set_volume(volume);
        Ok(())
    }

    pub fn volume(&self) -> f64 {
        self.volume.get()
    }

    pub fn push_data(&mut self, data: Vec<u8>) -> Result<(), PlayerError> {
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            if self.enough_data.load(Ordering::Relaxed) {
                return Err(PlayerError::EnoughData);
            }
            return source
                .push_buffer(data)
                .map(|_| ())
                .map_err(|_| PlayerError::BufferPushFailed);
        }
        Err(PlayerError::BufferPushFailed)
    }

    pub fn set_src(&mut self, source: PlayerSource) {
        self.source = Some(source);
    }

    pub fn buffered(&self) -> Vec<Range<f64>> {
        let mut buffered_ranges = vec![];

        let Some(duration) = self
            .last_metadata
            .as_ref()
            .and_then(|metadata| metadata.duration)
        else {
            return buffered_ranges;
        };

        let pipeline = self.player.pipeline();
        let mut buffering = gstreamer::query::Buffering::new(gstreamer::Format::Percent);
        if pipeline.query(&mut buffering) {
            let ranges = buffering.ranges();
            for (start, end) in ranges {
                let start = (if let gstreamer::GenericFormattedValue::Percent(start) = start {
                    start.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64 *
                    duration.as_secs_f64();
                let end = (if let gstreamer::GenericFormattedValue::Percent(end) = end {
                    end.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64 *
                    duration.as_secs_f64();
                buffered_ranges.push(Range { start, end });
            }
        }

        buffered_ranges
    }

    pub fn seekable(&self) -> Vec<Range<f64>> {
        // if the servosrc is seekable, we should return the duration of the media
        if let Some(metadata) = self.last_metadata.as_ref() &&
            metadata.is_seekable &&
            let Some(duration) = metadata.duration
        {
            return vec![Range {
                start: 0.0,
                end: duration.as_secs_f64(),
            }];
        }

        // if the servosrc is not seekable, we should return the buffered range
        self.buffered()
    }

    fn set_stream(&mut self, stream: &MediaStreamId, only_stream: bool) -> Result<(), PlayerError> {
        debug_assert!(self.stream_type == StreamType::Stream);
        let Some(PlayerSource::Stream(ref source)) = self.source else {
            return Err(PlayerError::SetStreamFailed);
        };

        let stream = get_stream(stream).expect("Media streams registry does not contain such ID");
        let mut stream = stream.lock().unwrap();
        let Some(stream) = stream.as_mut_any().downcast_mut::<GStreamerMediaStream>() else {
            return Err(PlayerError::SetStreamFailed);
        };

        let playbin = self
            .player
            .pipeline()
            .dynamic_cast::<gstreamer::Pipeline>()
            .unwrap();
        let clock = gstreamer::SystemClock::obtain();
        playbin.set_base_time(*BACKEND_BASE_TIME);
        playbin.set_start_time(gstreamer::ClockTime::NONE);
        playbin.use_clock(Some(&clock));
        source
            .set_stream(stream, only_stream)
            .map_err(|_| PlayerError::SetStreamFailed)
    }

    fn set_audio_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_audio_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_audio_track_enabled(enabled);
        Ok(())
    }

    fn set_video_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_video_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_video_track_enabled(enabled);
        Ok(())
    }
}

macro_rules! notify(
    ($observer:expr_2021, $event:expr_2021) => {
        $observer.lock().unwrap().send($event)
    };
);

struct SeekChannel {
    sender: SeekLock,
    recv: GenericReceiver<SeekLockMsg>,
}

impl SeekChannel {
    fn new() -> Self {
        let (sender, recv) =
            generic_channel::channel::<SeekLockMsg>().expect("Couldn't create IPC channel");
        Self {
            sender: SeekLock {
                lock_channel: sender,
            },
            recv,
        }
    }

    fn sender(&self) -> SeekLock {
        self.sender.clone()
    }

    fn _await(&self) -> SeekLockMsg {
        self.recv.recv().unwrap()
    }
}

pub struct GStreamerPlayer {
    /// The player unique ID.
    id: usize,
    /// The ID of the client context this player belongs to.
    context_id: ClientContextId,
    /// Channel to communicate with the owner GStreamerBackend instance.
    backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
    inner: RefCell<Option<Arc<Mutex<PlayerInner>>>>,
    observer: Arc<Mutex<GenericCallback<PlayerEvent>>>,
    audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
    /// Indicates whether the setup was succesfully performed and
    /// we are ready to consume a/v data.
    is_ready: Arc<Once>,
    /// Indicates whether the type of media stream to be played is a live stream.
    stream_type: StreamType,
    /// Decorator used to setup the video sink and process the produced frames.
    render: Arc<Mutex<GStreamerRender>>,
    /// Whether to enable download buffering. This is only enabled heuristically for some media,
    /// because it causes issues in certain situations. See <https://github.com/servo/servo/issues/23241>.
    download_buffering_enabled: AtomicBool,
    /// The media source of a player for `StreamType::MediaSource` (Media Source Extensions).
    media_source: Option<Arc<MediaSource>>,
}

impl GStreamerPlayer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: usize,
        context_id: &ClientContextId,
        backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
        stream_type: StreamType,
        observer: GenericCallback<PlayerEvent>,
        video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
        gl_context: Box<dyn PlayerGLContext>,
    ) -> GStreamerPlayer {
        let _ = gstreamer::DebugCategory::new(
            "servoplayer",
            gstreamer::DebugColorFlags::empty(),
            Some("Servo player"),
        );

        let observer = Arc::new(Mutex::new(observer));
        let media_source = (stream_type == StreamType::MediaSource)
            .then(|| Arc::new(MediaSource::new(observer.clone())));
        Self {
            id,
            context_id: *context_id,
            backend_chan,
            inner: RefCell::new(None),
            observer,
            audio_renderer,
            video_renderer,
            is_ready: Arc::new(Once::new()),
            stream_type,
            render: Arc::new(Mutex::new(GStreamerRender::new(gl_context))),
            download_buffering_enabled: AtomicBool::new(false),
            media_source,
        }
    }

    fn media_source(&self) -> Result<&MediaSource, PlayerError> {
        self.media_source
            .as_deref()
            .ok_or(PlayerError::MediaSourceNotSupported)
    }

    fn setup(&self) -> Result<(), PlayerError> {
        if self.inner.borrow().is_some() {
            return Ok(());
        }

        // Check that we actually have the elements that we
        // need to make this work.
        for element in ["playbin3", "decodebin3", "queue"] {
            if gstreamer::ElementFactory::find(element).is_none() {
                return Err(PlayerError::Backend(format!(
                    "Missing dependency: {}",
                    element
                )));
            }
        }

        let player = gstreamer_play::Play::default();
        let signal_adapter = gstreamer_play::PlaySignalAdapter::new_sync_emit(&player);
        let pipeline = player.pipeline();

        // FIXME(#282): The progressive downloading breaks playback on Windows and Android.
        // A media source keeps the media data itself.
        if !cfg!(any(target_os = "windows", target_os = "android")) &&
            self.stream_type != StreamType::MediaSource &&
            self.download_buffering_enabled.load(Ordering::Relaxed)
        {
            // Set player to perform progressive downloading. This will make the
            // player store the downloaded media in a local temporary file for
            // faster playback of already-downloaded chunks.
            let flags = pipeline.property_value("flags");
            let flags_class = match glib::FlagsClass::with_type(flags.type_()) {
                Some(flags) => flags,
                None => {
                    return Err(PlayerError::Backend(
                        "FlagsClass creation failed".to_owned(),
                    ));
                },
            };
            let flags_class = match flags_class.builder_with_value(flags) {
                Some(class) => class,
                None => {
                    return Err(PlayerError::Backend(
                        "FlagsClass creation failed".to_owned(),
                    ));
                },
            };
            let Some(flags) = flags_class.set_by_nick("download").build() else {
                return Err(PlayerError::Backend(
                    "FlagsClass creation failed".to_owned(),
                ));
            };
            pipeline.set_property_from_value("flags", &flags);
        }

        if self.stream_type == StreamType::MediaSource {
            // The media source keeps the media data and says when it has enough (gstmse). With
            // the buffering of playbin3, GstPlay paused the playback whenever the buffering level
            // was under 100%, and the media source never fills a 500MB buffer.
            unset_playbin_flag(&pipeline, "buffering")?;
            // msesrc posts a stream collection for each initialization segment (the source
            // buffers reach gstmse one after the other, `mse.rs`). GstPlay selects streams from
            // the collections on the bus; selecting the streams of msesrc's newer collection
            // before decodebin3 has it aborts the process (`handle_stream_switch: assertion
            // failed`, "Stream switch requested for future collection"). Drop msesrc's own
            // collections: GstPlay gets the ones decodebin3 posts once it uses them.
            if let Some(bus) = pipeline.bus() {
                bus.set_sync_handler(|_, message| {
                    let from_msesrc =
                        matches!(message.view(), gstreamer::MessageView::StreamCollection(_)) &&
                            message
                                .src()
                                .and_then(|source| source.downcast_ref::<gstreamer::Element>())
                                .and_then(|element| element.factory())
                                .is_some_and(|factory| factory.name() == "kyouseimsesrc");
                    if from_msesrc {
                        gstreamer::BusSyncReply::Drop
                    } else {
                        gstreamer::BusSyncReply::Pass
                    }
                });
            }
        } else {
            // Set max size for the player buffer.
            pipeline.set_property("buffer-size", MAX_BUFFER_SIZE);
        }

        // Set player position interval update to 0.5 seconds.
        let mut config = player.config();
        config.set_position_update_interval(500u32);
        player
            .set_config(config)
            .map_err(|e| PlayerError::Backend(e.to_string()))?;

        if let Some(ref audio_renderer) = self.audio_renderer {
            let audio_sink =
                gstreamer::ElementFactory::make("appsink")
                    .build()
                    .map_err(|error| {
                        PlayerError::Backend(format!("appsink creation failed: {error:?}"))
                    })?;

            pipeline.set_property("audio-sink", &audio_sink);

            let audio_sink = audio_sink.dynamic_cast::<gstreamer_app::AppSink>().unwrap();

            let weak_audio_renderer = Arc::downgrade(audio_renderer);

            audio_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll(|_| Ok(gstreamer::FlowSuccess::Ok))
                    .new_sample(move |audio_sink| {
                        let sample = audio_sink
                            .pull_sample()
                            .map_err(|_| gstreamer::FlowError::Eos)?;
                        let buffer = sample.buffer_owned().ok_or(gstreamer::FlowError::Error)?;
                        let audio_info = sample
                            .caps()
                            .and_then(|caps| gstreamer_audio::AudioInfo::from_caps(caps).ok())
                            .ok_or(gstreamer::FlowError::Error)?;
                        let positions =
                            audio_info.positions().ok_or(gstreamer::FlowError::Error)?;

                        let Some(audio_renderer) = weak_audio_renderer.upgrade() else {
                            return Err(gstreamer::FlowError::Flushing);
                        };

                        for position in positions.iter() {
                            let buffer = buffer.clone();
                            let map = match buffer.into_mapped_buffer_readable() {
                                Ok(map) => map,
                                _ => {
                                    return Err(gstreamer::FlowError::Error);
                                },
                            };
                            let chunk = Box::new(GStreamerAudioChunk(map));
                            let channel = position.to_mask() as u32;

                            audio_renderer.lock().unwrap().render(chunk, channel);
                        }
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        let video_sink = self.render.lock().unwrap().setup_video_sink(&pipeline)?;

        // There's a known bug in gstreamer that may cause a wrong transition
        // to the ready state while setting the uri property:
        // https://cgit.freedesktop.org/gstreamer/gst-plugins-bad/commit/?id=afbbc3a97ec391c6a582f3c746965fdc3eb3e1f3
        // This may affect things like setting the config, so until the bug is
        // fixed, make sure that state dependent code happens before this line.
        // The estimated version for the fix is 1.14.5 / 1.15.1.
        // https://github.com/servo/servo/issues/22010#issuecomment-432599657
        let uri = match self.stream_type {
            StreamType::Stream => {
                register_servo_media_stream_src().map_err(|error| {
                    PlayerError::Backend(format!(
                        "servomediastreamsrc registration error: {error:?}"
                    ))
                })?;
                "mediastream://".to_value()
            },
            StreamType::Seekable => {
                register_servo_src().map_err(|error| {
                    PlayerError::Backend(format!("servosrc registration error: {error:?}"))
                })?;
                "servosrc://".to_value()
            },
            StreamType::MediaSource => "kyousei-mse://".to_value(),
        };
        player.set_property("uri", &uri);

        // No video_renderers no video
        if self.video_renderer.is_none() {
            player.set_video_track_enabled(false);
        }

        *self.inner.borrow_mut() = Some(Arc::new(Mutex::new(PlayerInner {
            player,
            signal_adapter: signal_adapter.clone(),
            source: None,
            video_sink,
            input_size: 0,
            seekable: true,
            play_state: gstreamer_play::PlayState::Stopped,
            paused: Cell::new(DEFAULT_PAUSED),
            can_resume: Cell::new(DEFAULT_CAN_RESUME),
            playback_rate: Cell::new(DEFAULT_PLAYBACK_RATE),
            muted: Cell::new(DEFAULT_MUTED),
            volume: Cell::new(DEFAULT_VOLUME),
            stream_type: self.stream_type,
            last_metadata: None,
            cat: gstreamer::DebugCategory::get("servoplayer").unwrap(),
            enough_data: Arc::new(AtomicBool::new(false)),
        })));

        let inner = self.inner.borrow();
        let inner = inner.as_ref().unwrap();
        let observer = self.observer.clone();
        // Handle `end-of-stream` signal.
        signal_adapter.connect_end_of_stream(move |_| {
            let _ = notify!(observer, PlayerEvent::EndOfStream);
        });

        let observer = self.observer.clone();
        // Handle `error` signal
        signal_adapter.connect_error(move |_self, error, _details| {
            let _ = notify!(observer, PlayerEvent::Error(error.to_string()));
        });

        let inner_clone = inner.clone();
        let observer = self.observer.clone();
        // Handle `state-changed` signal.
        signal_adapter.connect_state_changed(move |_, play_state| {
            inner_clone.lock().unwrap().play_state = play_state;

            let state = match play_state {
                gstreamer_play::PlayState::Buffering => Some(PlaybackState::Buffering),
                gstreamer_play::PlayState::Stopped => Some(PlaybackState::Stopped),
                gstreamer_play::PlayState::Paused => Some(PlaybackState::Paused),
                gstreamer_play::PlayState::Playing => Some(PlaybackState::Playing),
                _ => None,
            };
            if let Some(v) = state {
                let _ = notify!(observer, PlayerEvent::StateChanged(v));
            }
        });

        let observer = self.observer.clone();
        // Handle `position-update` signal.
        signal_adapter.connect_position_updated(move |_, position| {
            if let Some(seconds) = position.map(|p| p.seconds_f64()) {
                let _ = notify!(observer, PlayerEvent::PositionChanged(seconds));
            }
        });

        let observer = self.observer.clone();
        // Handle `seek-done` signal.
        signal_adapter.connect_seek_done(move |_, position| {
            let _ = notify!(observer, PlayerEvent::SeekDone(position.seconds_f64()));
        });

        // Handle `media-info-updated` signal.
        let inner_clone = inner.clone();
        let observer = self.observer.clone();
        signal_adapter.connect_media_info_updated(move |_, info| {
            let Ok(metadata) = metadata_from_media_info(info) else {
                return;
            };

            let mut inner = inner_clone.lock().unwrap();

            if inner.last_metadata.as_ref() == Some(&metadata) {
                return;
            }

            // TODO: Workaround to generate expected `paused` state change event.
            // <https://github.com/servo/servo/issues/40740>
            let mut send_pause_event = false;

            if inner.last_metadata.is_none() {
                if metadata.is_seekable && inner.playback_rate.get() != DEFAULT_PLAYBACK_RATE {
                    // The `paused` state change event will be fired after the
                    // seek initiated by the playback rate change has
                    // completed.
                    inner.player.set_rate(inner.playback_rate.get());
                } else if inner.play_state == gstreamer_play::PlayState::Paused {
                    // We need to send the paused event for non seekable push streams.
                    send_pause_event = true;
                }
            }

            // A media source can add a stream after the first stream collection (the source
            // buffers reach gstmse one after the other, `mse.rs`): GstPlay selects the streams of
            // the first collection only, so a video or an audio stream added later stayed
            // unselected (no picture, `videoWidth` 0, or no sound). Select it.
            if inner.stream_type == StreamType::MediaSource {
                if !info.video_streams().is_empty() && inner.player.current_video_track().is_none()
                {
                    let _ = inner.player.set_video_track(0);
                    inner.player.set_video_track_enabled(true);
                }
                if !info.audio_streams().is_empty() && inner.player.current_audio_track().is_none()
                {
                    let _ = inner.player.set_audio_track(0);
                    inner.player.set_audio_track_enabled(true);
                }
            }

            inner.last_metadata = Some(metadata.clone());
            gstreamer::info!(
                inner.cat,
                obj = &inner.player,
                "Metadata updated: {:?}",
                metadata
            );
            let _ = notify!(observer, PlayerEvent::MetadataUpdated(metadata));

            if send_pause_event {
                let _ = notify!(observer, PlayerEvent::StateChanged(PlaybackState::Paused));
            }
        });

        // Handle `duration-changed` signal.
        let inner_clone = inner.clone();
        let observer = self.observer.clone();
        signal_adapter.connect_duration_changed(move |_, duration| {
            let duration = duration.map(|duration| {
                time::Duration::new(
                    duration.seconds(),
                    (duration.nseconds() % 1_000_000_000) as u32,
                )
            });

            let mut inner = inner_clone.lock().unwrap();
            if let Some(ref mut metadata) = inner.last_metadata &&
                metadata.duration != duration
            {
                metadata.duration = duration;
                gstreamer::info!(
                    inner.cat,
                    obj = &inner.player,
                    "Duration changed: {:?}",
                    duration
                );
                let _ = notify!(observer, PlayerEvent::DurationChanged(duration));
            }
        });

        if let Some(video_renderer) = self.video_renderer.clone() {
            // Creates a closure that renders a frame using the video_renderer
            // Used in the preroll and sample callbacks
            let render_sample = {
                let render = self.render.clone();
                let observer = self.observer.clone();
                let weak_video_renderer = Arc::downgrade(&video_renderer);

                move |sample: gstreamer::Sample| {
                    // A panic in another thread must not take this streaming thread down too.
                    let Some(frame) = render
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_frame_from_sample(sample)
                    else {
                        return Err(gstreamer::FlowError::Error);
                    };

                    match weak_video_renderer.upgrade() {
                        Some(video_renderer) => {
                            video_renderer
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .render(frame);
                        },
                        _ => {
                            return Err(gstreamer::FlowError::Flushing);
                        },
                    };

                    let _ = notify!(observer, PlayerEvent::VideoFrameUpdated);
                    Ok(gstreamer::FlowSuccess::Ok)
                }
            };

            // Set video_sink callbacks.
            inner.lock().unwrap().video_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll({
                        let render_sample = render_sample.clone();
                        move |video_sink| {
                            render_sample(
                                video_sink
                                    .pull_preroll()
                                    .map_err(|_| gstreamer::FlowError::Eos)?,
                            )
                        }
                    })
                    .new_sample(move |video_sink| {
                        render_sample(
                            video_sink
                                .pull_sample()
                                .map_err(|_| gstreamer::FlowError::Eos)?,
                        )
                    })
                    .build(),
            );
        };

        let (receiver, error_handler_id) = {
            let inner_clone = inner.clone();
            let inner = inner.lock().unwrap();
            let pipeline = inner.player.pipeline();

            let (sender, receiver) = mpsc::channel();

            let sender = Arc::new(Mutex::new(sender));
            let sender_clone = sender.clone();
            let is_ready_clone = self.is_ready.clone();
            let observer = self.observer.clone();
            let media_source = self.media_source.clone();
            pipeline.connect("source-setup", false, move |args| {
                let source = args[1].get::<gstreamer::Element>().unwrap();

                let mut inner = inner_clone.lock().unwrap();
                let source = match inner.stream_type {
                    StreamType::Seekable => {
                        let servosrc = source
                            .dynamic_cast::<ServoSrc>()
                            .expect("Source element is expected to be a ServoSrc!");

                        if inner.input_size > 0 {
                            servosrc.set_size(inner.input_size as i64);
                        }
                        servosrc.set_seekable(inner.seekable);

                        let sender_clone = sender.clone();
                        let is_ready = is_ready_clone.clone();
                        let observer_ = observer.clone();
                        let observer__ = observer.clone();
                        let observer___ = observer.clone();
                        let servosrc_ = servosrc.clone();
                        let enough_data_ = inner.enough_data.clone();
                        let enough_data__ = inner.enough_data.clone();
                        let seek_channel = Arc::new(Mutex::new(SeekChannel::new()));
                        servosrc.set_callbacks(
                            gstreamer_app::AppSrcCallbacks::builder()
                                .need_data(move |_, _| {
                                    // We block the caller of the setup method until we get
                                    // the first need-data signal, so we ensure that we
                                    // don't miss any data between the moment the client
                                    // calls setup and the player is actually ready to
                                    // get any data.
                                    is_ready.call_once(|| {
                                        let _ = sender_clone.lock().unwrap().send(Ok(()));
                                    });

                                    enough_data_.store(false, Ordering::Relaxed);
                                    let _ = notify!(observer_, PlayerEvent::NeedData);
                                })
                                .enough_data(move |_| {
                                    enough_data__.store(true, Ordering::Relaxed);
                                    let _ = notify!(observer__, PlayerEvent::EnoughData);
                                })
                                .seek_data(move |_, offset| {
                                    let (ret, ack_channel) = if servosrc_.set_seek_offset(offset) {
                                        let _ = notify!(
                                            observer___,
                                            PlayerEvent::SeekData(
                                                offset,
                                                seek_channel.lock().unwrap().sender()
                                            )
                                        );
                                        let (ret, ack_channel) =
                                            seek_channel.lock().unwrap()._await();
                                        (ret, Some(ack_channel))
                                    } else {
                                        (true, None)
                                    };

                                    servosrc_.set_seek_done();
                                    if let Some(ack_channel) = ack_channel {
                                        ack_channel.send(()).unwrap();
                                    }
                                    ret
                                })
                                .build(),
                        );

                        PlayerSource::Seekable(servosrc)
                    },
                    StreamType::Stream => {
                        let media_stream_src = source
                            .dynamic_cast::<ServoMediaStreamSrc>()
                            .expect("Source element is expected to be a ServoMediaStreamSrc!");
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        PlayerSource::Stream(media_stream_src)
                    },
                    StreamType::MediaSource => {
                        if let Some(ref media_source) = media_source {
                            media_source.attach(&source);
                        }
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        PlayerSource::MediaSource(source)
                    },
                };

                inner.set_src(source);

                None
            });

            let error_handler_id =
                signal_adapter.connect_error(move |signal_adapter, error, _details| {
                    let _ = notify!(sender_clone, Err(PlayerError::Backend(error.to_string())));
                    signal_adapter.play().stop();
                });

            inner.player.pause();

            (receiver, error_handler_id)
        };

        let result = receiver.recv().unwrap();
        glib::signal::signal_handler_disconnect(
            &inner.lock().unwrap().signal_adapter,
            error_handler_id,
        );
        result
    }
}

macro_rules! inner_player_proxy_getter {
    ($fn_name:ident, $return_type:ty, $default_value:expr_2021) => {
        fn $fn_name(&self) -> $return_type {
            if self.setup().is_err() {
                return $default_value;
            }

            let inner = self.inner.borrow();
            let inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };
}

macro_rules! inner_player_proxy {
    ($fn_name:ident, $return_type:ty) => {
        fn $fn_name(&self) -> Result<$return_type, PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1)
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty, $arg2:ident, $arg2_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type, $arg2: $arg2_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1, $arg2)
        }
    };
}

impl Player for GStreamerPlayer {
    inner_player_proxy!(play, ());
    inner_player_proxy!(pause, ());
    inner_player_proxy_getter!(paused, bool, DEFAULT_PAUSED);
    inner_player_proxy_getter!(can_resume, bool, DEFAULT_CAN_RESUME);
    inner_player_proxy!(stop, ());
    inner_player_proxy!(end_of_stream, ());
    inner_player_proxy!(set_input_size, size, u64);
    inner_player_proxy!(set_seekable, seekable, bool);
    inner_player_proxy!(set_mute, muted, bool);
    inner_player_proxy_getter!(muted, bool, DEFAULT_MUTED);
    inner_player_proxy!(set_playback_rate, playback_rate, f64);
    inner_player_proxy_getter!(playback_rate, f64, DEFAULT_PLAYBACK_RATE);
    inner_player_proxy!(push_data, data, Vec<u8>);
    inner_player_proxy!(seek, time, f64);
    inner_player_proxy!(set_volume, volume, f64);
    inner_player_proxy_getter!(volume, f64, DEFAULT_VOLUME);
    inner_player_proxy_getter!(buffered, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy_getter!(seekable, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy!(set_stream, stream, &MediaStreamId, only_stream, bool);
    inner_player_proxy!(set_audio_track, stream_index, i32, enabled, bool);
    inner_player_proxy!(set_video_track, stream_index, i32, enabled, bool);

    fn set_download_buffering_enabled(&self, enable: bool) -> Result<(), PlayerError> {
        // We can't use the default implementation because we need to set the
        // download_buffering_enabled flag before calling setup, which is where
        // the "download" flag is set on the pipeline. That has to happen before
        // the URI is set.
        self.download_buffering_enabled
            .store(enable, Ordering::Relaxed);
        self.setup()?;
        Ok(())
    }

    fn render_use_gl(&self) -> bool {
        self.render.lock().unwrap().is_gl()
    }

    fn media_source_add_source_buffer(&self, mime_type: &str) -> Result<usize, PlayerError> {
        self.media_source()?.add_source_buffer(mime_type)
    }

    fn media_source_remove_source_buffer(&self, id: usize) -> Result<(), PlayerError> {
        self.media_source()?.remove_source_buffer(id)
    }

    fn media_source_set_duration(&self, duration: f64) -> Result<(), PlayerError> {
        self.media_source()?.set_duration(duration)
    }

    fn media_source_end_of_stream(
        &self,
        error: MediaSourceEndOfStreamError,
    ) -> Result<(), PlayerError> {
        self.media_source()?.end_of_stream(error)
    }

    fn source_buffer_append(&self, id: usize, data: Vec<u8>) -> Result<(), PlayerError> {
        self.media_source()?.append(id, data)
    }

    fn source_buffer_abort(&self, id: usize) -> Result<(), PlayerError> {
        self.media_source()?.abort(id)
    }

    fn source_buffer_remove(&self, id: usize, start: f64, end: f64) -> Result<(), PlayerError> {
        self.media_source()?.remove(id, start, end)
    }

    fn source_buffer_buffered(&self, id: usize) -> Vec<Range<f64>> {
        self.media_source()
            .map(|media_source| media_source.buffered(id))
            .unwrap_or_default()
    }

    fn source_buffer_set_timestamp_offset(
        &self,
        id: usize,
        offset: f64,
    ) -> Result<(), PlayerError> {
        self.media_source()?.set_timestamp_offset(id, offset)
    }

    fn source_buffer_set_append_window(
        &self,
        id: usize,
        start: f64,
        end: f64,
    ) -> Result<(), PlayerError> {
        self.media_source()?.set_append_window(id, start, end)
    }

    fn source_buffer_set_sequence_mode(
        &self,
        id: usize,
        sequence: bool,
    ) -> Result<(), PlayerError> {
        self.media_source()?.set_sequence_mode(id, sequence)
    }

    fn source_buffer_change_type(&self, id: usize, mime_type: &str) -> Result<(), PlayerError> {
        self.media_source()?.change_type(id, mime_type)
    }
}

impl MediaInstance for GStreamerPlayer {
    fn get_id(&self) -> usize {
        self.id
    }

    fn mute(&self, val: bool) -> Result<(), MediaInstanceError> {
        self.set_mute(val).map_err(|_| MediaInstanceError)
    }

    fn suspend(&self) -> Result<(), MediaInstanceError> {
        self.pause().map_err(|_| MediaInstanceError)
    }

    fn resume(&self) -> Result<(), MediaInstanceError> {
        if !self.can_resume() {
            return Ok(());
        }

        self.play().map_err(|_| MediaInstanceError)
    }
}

impl Drop for GStreamerPlayer {
    fn drop(&mut self) {
        let _ = self.stop();
        let (tx_ack, rx_ack) = mpsc::channel();
        let _ = self
            .backend_chan
            .lock()
            .unwrap()
            .send(BackendMsg::Shutdown {
                context: self.context_id,
                id: self.id,
                tx_ack,
            });
        let _ = rx_ack.recv();
    }
}
