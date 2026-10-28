/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use malloc_size_of_derive::MallocSizeOf;

#[derive(Clone, MallocSizeOf)]
pub enum VideoFrameData {
    Raw(#[conditional_malloc_size_of] Arc<Vec<u8>>),
    /// The pixels stay in the backend's buffer (`Buffer::as_bytes`); no copy is made until they are used.
    Mapped,
    /// NV12 (a luma plane, then an interleaved chroma plane at half the width and height); the planes
    /// stay in the backend's buffer (`Buffer::nv12_planes`).
    Nv12(Nv12Info),
    Texture(u32),
    OESTexture(u32),
}

/// Which matrix converts the YUV values of a frame to RGB.
#[derive(Clone, Copy, Debug, Eq, MallocSizeOf, PartialEq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// How the planes of an NV12 frame are laid out and what its values mean.
#[derive(Clone, Copy, Debug, MallocSizeOf)]
pub struct Nv12Info {
    /// Bytes from the start of one luma row to the next.
    pub y_stride: i32,
    /// Bytes from the start of one chroma row to the next.
    pub uv_stride: i32,
    pub matrix: YuvMatrix,
    /// Whether the values use the full 0..255 range (otherwise 16..235 / 16..240).
    pub full_range: bool,
}

pub trait Buffer: Send + Sync {
    fn to_vec(&self) -> Option<VideoFrameData>;

    /// The pixels of a raw frame, if the backend can lend them without copying.
    fn as_bytes(&self) -> Option<&[u8]> {
        None
    }

    /// The luma plane and the chroma plane, if the buffer is an NV12 frame.
    fn nv12_planes(&self) -> Option<(&[u8], &[u8], Nv12Info)> {
        None
    }
}

#[derive(Clone, MallocSizeOf)]
pub struct VideoFrame {
    width: i32,
    height: i32,
    data: VideoFrameData,
    #[ignore_malloc_size_of = "Difficult"]
    _buffer: Arc<dyn Buffer>,
}

impl VideoFrame {
    pub fn new(width: i32, height: i32, buffer: Arc<dyn Buffer>) -> Option<Self> {
        let data = if let Some((_, _, info)) = buffer.nv12_planes() {
            VideoFrameData::Nv12(info)
        } else if buffer.as_bytes().is_some() {
            VideoFrameData::Mapped
        } else {
            buffer.to_vec()?
        };
        Some(VideoFrame {
            width,
            height,
            data,
            _buffer: buffer,
        })
    }

    pub fn get_width(&self) -> i32 {
        self.width
    }

    pub fn get_height(&self) -> i32 {
        self.height
    }

    pub fn get_data(&self) -> Arc<Vec<u8>> {
        match self.data {
            VideoFrameData::Raw(ref data) => data.clone(),
            VideoFrameData::Mapped => {
                Arc::new(self._buffer.as_bytes().unwrap_or_default().to_vec())
            },
            VideoFrameData::Nv12(_) => Arc::new(self.nv12_to_bgra()),
            _ => unreachable!("invalid raw data request for texture frame"),
        }
    }

    /// The pixels of a raw frame without copying (frames the backend keeps mapped only).
    pub fn mapped_bytes(&self) -> Option<&[u8]> {
        match self.data {
            VideoFrameData::Mapped => self._buffer.as_bytes(),
            _ => None,
        }
    }

    /// The two planes of an NV12 frame (luma, chroma) and how they are laid out.
    pub fn nv12_planes(&self) -> Option<(&[u8], &[u8], Nv12Info)> {
        match self.data {
            VideoFrameData::Nv12(_) => self._buffer.nv12_planes(),
            _ => None,
        }
    }

    /// Converts an NV12 frame to BGRA on the CPU (for the paths that need pixels: snapshots, canvas).
    fn nv12_to_bgra(&self) -> Vec<u8> {
        let (width, height) = (self.width.max(0) as usize, self.height.max(0) as usize);
        let mut out = vec![0u8; width * height * 4];
        let Some((y_plane, uv_plane, info)) = self._buffer.nv12_planes() else {
            return out;
        };
        // (kr, kb) of the matrix: R = Y + 2(1-kr)Cr, B = Y + 2(1-kb)Cb, G from the rest.
        let (kr, kb) = match info.matrix {
            YuvMatrix::Bt601 => (0.299f32, 0.114f32),
            YuvMatrix::Bt709 => (0.2126, 0.0722),
            YuvMatrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        let (y_off, y_scale, c_scale) = if info.full_range {
            (0.0, 1.0, 1.0)
        } else {
            (16.0, 255.0 / 219.0, 255.0 / 224.0)
        };
        for row in 0..height {
            let y_row = &y_plane[row * info.y_stride as usize..];
            let uv_row = &uv_plane[(row / 2) * info.uv_stride as usize..];
            for col in 0..width {
                let y = (y_row[col] as f32 - y_off) * y_scale;
                let cb = (uv_row[(col / 2) * 2] as f32 - 128.0) * c_scale;
                let cr = (uv_row[(col / 2) * 2 + 1] as f32 - 128.0) * c_scale;
                let r = y + 2.0 * (1.0 - kr) * cr;
                let b = y + 2.0 * (1.0 - kb) * cb;
                let g = (y - kr * r - kb * b) / kg;
                let o = (row * width + col) * 4;
                out[o] = b.clamp(0.0, 255.0) as u8;
                out[o + 1] = g.clamp(0.0, 255.0) as u8;
                out[o + 2] = r.clamp(0.0, 255.0) as u8;
                out[o + 3] = 255;
            }
        }
        out
    }

    pub fn get_texture_id(&self) -> u32 {
        match self.data {
            VideoFrameData::Texture(data) | VideoFrameData::OESTexture(data) => data,
            _ => unreachable!("invalid texture id request for raw data frame"),
        }
    }

    pub fn is_gl_texture(&self) -> bool {
        matches!(
            self.data,
            VideoFrameData::Texture(_) | VideoFrameData::OESTexture(_)
        )
    }

    pub fn is_external_oes(&self) -> bool {
        matches!(self.data, VideoFrameData::OESTexture(_))
    }
}

pub trait VideoFrameRenderer: Send + 'static {
    fn render(&mut self, frame: VideoFrame);
}
