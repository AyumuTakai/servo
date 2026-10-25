/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ops::Range;
use std::time::Duration;

extern crate servo_media_streams as streams;
extern crate servo_media_traits;

pub mod audio;
pub mod context;
pub mod metadata;
pub mod video;

use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{self, GenericSender};
use servo_media_traits::MediaInstance;
use streams::registry::MediaStreamId;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PlaybackState {
    Stopped,
    Buffering,
    Paused,
    Playing,
}

#[derive(Debug, PartialEq)]
pub enum PlayerError {
    /// Backend specific error.
    Backend(String),
    /// Could not push buffer contents to the player.
    BufferPushFailed,
    /// The player cannot consume more data.
    EnoughData,
    /// Setting End Of Stream failed.
    EOSFailed,
    /// The media stream is not seekable.
    NonSeekableStream,
    /// Tried to seek out of range.
    SeekOutOfRange,
    /// Setting an audio or video stream failed.
    /// Possibly because the type of source is not PlayerSource::Stream.
    SetStreamFailed,
    // Setting an audio or video track failed.
    SetTrackFailed,
    /// The backend has no Media Source Extensions, or the player was not created for a media source.
    MediaSourceNotSupported,
    /// A Media Source Extensions operation failed (the message comes from the backend).
    MediaSource(String),
    /// The source buffer has no room for the appended data
    /// (<https://w3c.github.io/media-source/#dfn-buffer-full-flag>).
    MediaSourceQuotaExceeded,
}

pub type SeekLockMsg = (bool, GenericSender<()>);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SeekLock {
    pub lock_channel: GenericSender<SeekLockMsg>,
}

impl SeekLock {
    pub fn unlock(&self, result: bool) {
        let (ack_sender, ack_recv) =
            generic_channel::channel::<()>().expect("Could not create IPC channel");
        self.lock_channel.send((result, ack_sender)).unwrap();
        ack_recv.recv().unwrap()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PlayerEvent {
    EndOfStream,
    /// The player has enough data. The client should stop pushing data into.
    EnoughData,
    Error(String),
    VideoFrameUpdated,
    MetadataUpdated(metadata::Metadata),
    // The `None` value means the duration is unknown, in which case this is likely a live stream.
    DurationChanged(Option<Duration>),
    /// The internal player queue is running out of data. The client should start
    /// pushing more data.
    NeedData,
    PositionChanged(f64),
    /// The player needs the data to perform a seek to the given offset in bytes.
    /// The next push_data should get the buffers from the new offset.
    /// The player will be blocked until the user unlocks it through
    /// the given SeekLock instance.
    /// This event is only received for seekable stream types.
    SeekData(u64, SeekLock),
    /// The player has performed a seek to the given time offset in seconds.
    SeekDone(f64),
    StateChanged(PlaybackState),
    /// Media Source Extensions: the media source was attached to the pipeline and
    /// is ready to accept source buffers.
    MediaSourceOpen,
    /// Media Source Extensions: an append or a removal on the source buffer with
    /// the given id finished. `error` is true when the media segment could not be
    /// parsed (the append error algorithm).
    SourceBufferUpdateEnd {
        id: usize,
        error: bool,
    },
}

/// The error given to `MediaSource.endOfStream()`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum MediaSourceEndOfStreamError {
    None,
    Network,
    Decode,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum StreamType {
    /// No seeking is supported in the stream, such as a live stream.
    Stream,
    /// The stream is seekable.
    Seekable,
    /// The data comes from a `MediaSource` (Media Source Extensions): the page
    /// appends media segments to source buffers.
    MediaSource,
}

pub trait Player: Send + MediaInstance {
    fn play(&self) -> Result<(), PlayerError>;
    fn pause(&self) -> Result<(), PlayerError>;
    fn paused(&self) -> bool;
    fn can_resume(&self) -> bool;
    fn stop(&self) -> Result<(), PlayerError>;
    fn seek(&self, time: f64) -> Result<(), PlayerError>;
    fn seekable(&self) -> Vec<Range<f64>>;
    fn set_mute(&self, muted: bool) -> Result<(), PlayerError>;
    fn muted(&self) -> bool;
    fn set_volume(&self, volume: f64) -> Result<(), PlayerError>;
    fn volume(&self) -> f64;
    fn set_input_size(&self, size: u64) -> Result<(), PlayerError>;
    fn set_seekable(&self, seekable: bool) -> Result<(), PlayerError>;
    fn set_download_buffering_enabled(&self, enabled: bool) -> Result<(), PlayerError>;
    fn set_playback_rate(&self, playback_rate: f64) -> Result<(), PlayerError>;
    fn playback_rate(&self) -> f64;
    fn push_data(&self, data: Vec<u8>) -> Result<(), PlayerError>;
    fn end_of_stream(&self) -> Result<(), PlayerError>;
    /// Get the list of time ranges in seconds that have been buffered.
    fn buffered(&self) -> Vec<Range<f64>>;
    /// Set the stream to be played by the player.
    /// Only a single stream of the same type (audio or video) can be set.
    /// Subsequent calls with a stream of the same type will override the previously
    /// set stream.
    /// This method requires the player to be constructed with StreamType::Stream.
    /// It is important to give the correct value of `only_stream` indicating
    /// that the audio or video stream being set is the only one expected.
    /// Subsequent calls to `set_stream` after the `only_stream` flag has been
    /// set to true will fail.
    fn set_stream(&self, stream: &MediaStreamId, only_stream: bool) -> Result<(), PlayerError>;
    /// If player's rendering draws using GL textures
    fn render_use_gl(&self) -> bool;
    fn set_audio_track(&self, stream_index: i32, enabled: bool) -> Result<(), PlayerError>;
    fn set_video_track(&self, stream_index: i32, enabled: bool) -> Result<(), PlayerError>;

    // Media Source Extensions. Only for players created with `StreamType::MediaSource`.
    // Source buffers are identified by the id `media_source_add_source_buffer` returns.
    // Appends and removals are asynchronous: they end with `PlayerEvent::SourceBufferUpdateEnd`.

    fn media_source_add_source_buffer(&self, _mime_type: &str) -> Result<usize, PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn media_source_remove_source_buffer(&self, _id: usize) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn media_source_set_duration(&self, _duration: f64) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn media_source_end_of_stream(
        &self,
        _error: MediaSourceEndOfStreamError,
    ) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn source_buffer_append(&self, _id: usize, _data: Vec<u8>) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn source_buffer_abort(&self, _id: usize) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn source_buffer_remove(&self, _id: usize, _start: f64, _end: f64) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn source_buffer_buffered(&self, _id: usize) -> Vec<Range<f64>> {
        vec![]
    }
    fn source_buffer_set_timestamp_offset(
        &self,
        _id: usize,
        _offset: f64,
    ) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn source_buffer_set_append_window(
        &self,
        _id: usize,
        _start: f64,
        _end: f64,
    ) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn source_buffer_set_sequence_mode(
        &self,
        _id: usize,
        _sequence: bool,
    ) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
    fn source_buffer_change_type(&self, _id: usize, _mime_type: &str) -> Result<(), PlayerError> {
        Err(PlayerError::MediaSourceNotSupported)
    }
}
