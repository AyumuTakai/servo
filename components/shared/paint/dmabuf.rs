/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Video frames as dmabufs, handed from a content process to the process that paints, so that
//! the pixels are never read back by the CPU.
//!
//! A content process that decodes a video on the GPU sends each decoded frame's dmabuf file
//! descriptor over a Unix socket (`SCM_RIGHTS`) and tells the painter, over the usual image
//! update message, only which frame it is ([`DmaBufRef`]). The painter imports the dmabuf into
//! an OpenGL texture (`EGLImage`) when WebRender asks for the image, and WebRender draws it as
//! an external texture. Reads and writes of the GPU memory are ordered by the kernel (implicit
//! synchronization of the dmabuf).
//!
//! The socket is `SERVO_MEDIA_DMABUF_SOCKET`, which the embedder makes reachable from the content
//! processes. What a content process can send is checked: the descriptor must be a dmabuf, and the
//! sizes must be sane.

#![allow(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::ffi::{CString, c_void};
use std::mem::{size_of, zeroed};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use webrender_api::{ExternalImageData, ExternalImageId, ExternalImageType, ImageBufferKind, ImageKey};

const MAGIC: u32 = 0x4b44_4d42;
const VERSION: u32 = 1;
/// The largest frame (in pixels, each way) accepted.
const MAX_SIDE: u32 = 16384;
/// How many received frames wait for the image update that names them.
const MAX_WAITING_FRAMES: usize = 128;
/// How many frames of one image are kept: WebRender asks for the texture when it renders, which
/// can be a frame or two after the update was sent.
const KEPT_PER_IMAGE: usize = 8;
/// Ids from here up name dmabuf images (the ids of other external images are smaller).
pub const DMABUF_ID_BASE: u64 = 1 << 61;
pub const DMABUF_ID_END: u64 = 1 << 62;

pub const SOCKET_ENV: &str = "SERVO_MEDIA_DMABUF_SOCKET";

/// A frame sent by a content process, to be named in an image update.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct DmaBufRef {
    pub nonce: u64,
    pub frame_id: u64,
    pub width: u32,
    pub height: u32,
}

/// A frame received: the descriptor of the dmabuf and how its pixels are laid out.
pub struct DmaBufFrame {
    fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub pitch: u32,
    pub offset: u32,
    pub modifier: u64,
}

/// What the wire carries before the descriptor.
#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    magic: u32,
    version: u32,
    nonce: u64,
    frame_id: u64,
    width: u32,
    height: u32,
    fourcc: u32,
    pitch: u32,
    offset: u32,
    _pad: u32,
    modifier: u64,
}

// ---- the content process (sender) ----

struct Client {
    socket: RawFd,
    nonce: u64,
    next_frame_id: u64,
}

static CLIENT: Mutex<Option<Client>> = Mutex::new(None);
static CLIENT_FAILED: AtomicBool = AtomicBool::new(false);

fn connect(path: &str) -> Option<RawFd> {
    unsafe {
        let socket = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if socket < 0 {
            return None;
        }
        let (address, length) = socket_address(path)?;
        if libc::connect(socket, &address as *const _ as *const libc::sockaddr, length) != 0 {
            libc::close(socket);
            return None;
        }
        Some(socket)
    }
}

fn socket_address(path: &str) -> Option<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_bytes();
    let mut address: libc::sockaddr_un = unsafe { zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return None;
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    Some((address, size_of::<libc::sockaddr_un>() as libc::socklen_t))
}

fn random_u64() -> u64 {
    let mut value = 0u64;
    unsafe {
        libc::getrandom(&mut value as *mut u64 as *mut c_void, 8, 0);
    }
    value
}

/// Sends the dmabuf `fd` (which stays the caller's) to the process that paints, and returns what
/// to put in the image update. `None` if there is no socket or the send failed.
pub fn send_frame(
    fd: RawFd,
    width: u32,
    height: u32,
    fourcc: u32,
    pitch: u32,
    offset: u32,
    modifier: u64,
) -> Option<DmaBufRef> {
    if CLIENT_FAILED.load(Ordering::Relaxed) {
        return None;
    }
    let mut client = CLIENT.lock().ok()?;
    if client.is_none() {
        let path = std::env::var(SOCKET_ENV).ok()?;
        let Some(socket) = connect(&path) else {
            CLIENT_FAILED.store(true, Ordering::Relaxed);
            return None;
        };
        *client = Some(Client {
            socket,
            nonce: random_u64(),
            next_frame_id: 1,
        });
    }
    let client = client.as_mut()?;
    let frame_id = client.next_frame_id;
    client.next_frame_id += 1;
    let header = Header {
        magic: MAGIC,
        version: VERSION,
        nonce: client.nonce,
        frame_id,
        width,
        height,
        fourcc,
        pitch,
        offset,
        _pad: 0,
        modifier,
    };
    unsafe {
        let mut io = libc::iovec {
            iov_base: &header as *const Header as *mut c_void,
            iov_len: size_of::<Header>(),
        };
        let mut control = [0u8; 64];
        let mut message: libc::msghdr = zeroed();
        message.msg_iov = &mut io;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr() as *mut c_void;
        message.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&message);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            &fd as *const RawFd as *const u8,
            libc::CMSG_DATA(cmsg),
            size_of::<RawFd>(),
        );
        if libc::sendmsg(client.socket, &message, libc::MSG_NOSIGNAL) < 0 {
            CLIENT_FAILED.store(true, Ordering::Relaxed);
            return None;
        }
    }
    Some(DmaBufRef {
        nonce: client.nonce,
        frame_id,
        width,
        height,
    })
}

// ---- the process that paints (receiver) ----

#[derive(Default)]
struct Registry {
    waiting: VecDeque<((u64, u64), Arc<DmaBufFrame>)>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    waiting: VecDeque::new(),
});
static ARRIVED: Condvar = Condvar::new();
static RECEIVER_STARTED: AtomicBool = AtomicBool::new(false);

/// Starts listening on `SERVO_MEDIA_DMABUF_SOCKET`, if the embedder set it (once).
pub fn start_receiver_from_env() {
    let Ok(path) = std::env::var(SOCKET_ENV) else {
        return;
    };
    if RECEIVER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some((address, length)) = socket_address(&path) else {
        return;
    };
    let listener = unsafe {
        let socket = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if socket < 0 {
            return;
        }
        if let Ok(c_path) = CString::new(path.clone()) {
            libc::unlink(c_path.as_ptr());
        }
        if libc::bind(socket, &address as *const _ as *const libc::sockaddr, length) != 0 ||
            libc::listen(socket, 16) != 0
        {
            libc::close(socket);
            log::warn!("dmabuf: cannot listen on {path}");
            return;
        }
        socket
    };
    let _ = std::thread::Builder::new()
        .name("dmabuf-accept".into())
        .spawn(move || loop {
            let connection = unsafe {
                libc::accept4(
                    listener,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if connection < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            let _ = std::thread::Builder::new()
                .name("dmabuf-recv".into())
                .spawn(move || receive_loop(connection));
        });
}

fn is_dmabuf(fd: RawFd) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .map(|target| target.to_string_lossy().contains("dmabuf"))
        .unwrap_or(false)
}

fn receive_loop(connection: RawFd) {
    loop {
        let mut header: Header = unsafe { zeroed() };
        let mut received_fd: RawFd = -1;
        let length = unsafe {
            let mut io = libc::iovec {
                iov_base: &mut header as *mut Header as *mut c_void,
                iov_len: size_of::<Header>(),
            };
            let mut control = [0u8; 64];
            let mut message: libc::msghdr = zeroed();
            message.msg_iov = &mut io;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr() as *mut c_void;
            message.msg_controllen = control.len() as _;
            let length = libc::recvmsg(connection, &mut message, libc::MSG_CMSG_CLOEXEC);
            if length > 0 {
                let mut cmsg = libc::CMSG_FIRSTHDR(&message);
                while !cmsg.is_null() {
                    if (*cmsg).cmsg_level == libc::SOL_SOCKET &&
                        (*cmsg).cmsg_type == libc::SCM_RIGHTS
                    {
                        std::ptr::copy_nonoverlapping(
                            libc::CMSG_DATA(cmsg),
                            &mut received_fd as *mut RawFd as *mut u8,
                            size_of::<RawFd>(),
                        );
                    }
                    cmsg = libc::CMSG_NXTHDR(&message, cmsg);
                }
            }
            length
        };
        if length <= 0 {
            if received_fd >= 0 {
                unsafe { libc::close(received_fd) };
            }
            unsafe { libc::close(connection) };
            return;
        }
        if received_fd < 0 {
            continue;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(received_fd) };
        if length as usize != size_of::<Header>() ||
            header.magic != MAGIC ||
            header.version != VERSION ||
            header.width == 0 ||
            header.height == 0 ||
            header.width > MAX_SIDE ||
            header.height > MAX_SIDE ||
            !is_dmabuf(received_fd)
        {
            continue;
        }
        let frame = Arc::new(DmaBufFrame {
            fd,
            width: header.width,
            height: header.height,
            fourcc: header.fourcc,
            pitch: header.pitch,
            offset: header.offset,
            modifier: header.modifier,
        });
        let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        registry
            .waiting
            .push_back(((header.nonce, header.frame_id), frame));
        while registry.waiting.len() > MAX_WAITING_FRAMES {
            registry.waiting.pop_front();
        }
        drop(registry);
        ARRIVED.notify_all();
    }
}

/// The frame a content process named in an image update (waits a little for the descriptor, which
/// travels on another socket).
pub fn take_frame(reference: &DmaBufRef, timeout: Duration) -> Option<Arc<DmaBufFrame>> {
    let deadline = Instant::now() + timeout;
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(index) = registry
            .waiting
            .iter()
            .position(|(key, _)| *key == (reference.nonce, reference.frame_id))
        {
            return registry.waiting.remove(index).map(|(_, frame)| frame);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        registry = ARRIVED
            .wait_timeout(registry, deadline - now)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

// ---- the frames WebRender can ask for ----

#[derive(Default)]
struct Published {
    next_id: u64,
    by_id: HashMap<u64, Arc<DmaBufFrame>>,
    by_key: HashMap<ImageKey, VecDeque<u64>>,
}

static PUBLISHED: Mutex<Option<Published>> = Mutex::new(None);

/// Keeps `frame` as the newest of image `key` and returns the external image to give WebRender.
pub fn publish(key: ImageKey, frame: Arc<DmaBufFrame>) -> ExternalImageData {
    let mut guard = PUBLISHED.lock().unwrap_or_else(|e| e.into_inner());
    let published = guard.get_or_insert_with(Published::default);
    published.next_id += 1;
    let id = DMABUF_ID_BASE + published.next_id;
    published.by_id.insert(id, frame);
    let ids = published.by_key.entry(key).or_default();
    ids.push_back(id);
    let mut dropped = vec![];
    while ids.len() > KEPT_PER_IMAGE {
        dropped.extend(ids.pop_front());
    }
    for id in dropped {
        published.by_id.remove(&id);
    }
    ExternalImageData {
        id: ExternalImageId(id),
        channel_index: 0,
        image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
        normalized_uvs: false,
    }
}

/// The image `key` was deleted.
pub fn forget(key: ImageKey) {
    let mut guard = PUBLISHED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(published) = guard.as_mut() {
        for id in published.by_key.remove(&key).unwrap_or_default() {
            published.by_id.remove(&id);
        }
    }
}

pub fn frame_of(id: u64) -> Option<Arc<DmaBufFrame>> {
    PUBLISHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .by_id
        .get(&id)
        .cloned()
}

// ---- import into OpenGL (the thread that renders, with the GL context current) ----

type GetProcAddress = unsafe extern "C" fn(*const libc::c_char) -> *const c_void;
type GetCurrentDisplay = unsafe extern "C" fn() -> *mut c_void;
type CreateImage =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type DestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type ImageTargetTexture2D = unsafe extern "C" fn(u32, *mut c_void);

struct Egl {
    get_current_display: GetCurrentDisplay,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_target_texture_2d: ImageTargetTexture2D,
}

fn egl() -> Option<&'static Egl> {
    static EGL: std::sync::OnceLock<Option<Egl>> = std::sync::OnceLock::new();
    EGL.get_or_init(|| unsafe {
        let name = CString::new("eglGetProcAddress").ok()?;
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
        if symbol.is_null() {
            return None;
        }
        let get_proc: GetProcAddress = std::mem::transmute(symbol);
        let load = |name: &str| -> Option<*const c_void> {
            let name = CString::new(name).ok()?;
            let pointer = get_proc(name.as_ptr());
            (!pointer.is_null()).then_some(pointer)
        };
        Some(Egl {
            get_current_display: std::mem::transmute(load("eglGetCurrentDisplay")?),
            create_image: std::mem::transmute(load("eglCreateImageKHR")?),
            destroy_image: std::mem::transmute(load("eglDestroyImageKHR")?),
            image_target_texture_2d: std::mem::transmute(load("glEGLImageTargetTexture2DOES")?),
        })
    })
    .as_ref()
}

/// A frame imported as a texture, until it is destroyed.
pub struct ImportedTexture {
    pub texture: u32,
    image: *mut c_void,
    display: *mut c_void,
}

const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_NONE: i32 = 0x3038;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Imports the frame as a GL texture (`GL_TEXTURE_2D`) of the current context. Call it on the thread
/// that renders, with the context current.
pub fn import(gl: &dyn gleam::gl::Gl, frame: &DmaBufFrame) -> Option<ImportedTexture> {
    use std::os::fd::AsRawFd;
    let egl = egl()?;
    unsafe {
        let display = (egl.get_current_display)();
        if display.is_null() {
            return None;
        }
        let mut attributes = vec![
            EGL_WIDTH,
            frame.width as i32,
            EGL_HEIGHT,
            frame.height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            frame.fourcc as i32,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            frame.fd.as_raw_fd(),
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            frame.offset as i32,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            frame.pitch as i32,
        ];
        if frame.modifier != DRM_FORMAT_MOD_INVALID {
            attributes.extend([
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (frame.modifier & 0xffff_ffff) as i32,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (frame.modifier >> 32) as i32,
            ]);
        }
        attributes.push(EGL_NONE);
        let image = (egl.create_image)(
            display,
            std::ptr::null_mut(),
            EGL_LINUX_DMA_BUF_EXT,
            std::ptr::null_mut(),
            attributes.as_ptr(),
        );
        if image.is_null() {
            log::warn!(
                "dmabuf: cannot import {}x{} fourcc {:#x} pitch {} offset {} modifier {:#x}",
                frame.width,
                frame.height,
                frame.fourcc,
                frame.pitch,
                frame.offset,
                frame.modifier
            );
            return None;
        }
        let texture = gl.gen_textures(1).first().copied()?;
        gl.bind_texture(gleam::gl::TEXTURE_2D, texture);
        (egl.image_target_texture_2d)(gleam::gl::TEXTURE_2D, image);
        gl.tex_parameter_i(
            gleam::gl::TEXTURE_2D,
            gleam::gl::TEXTURE_MIN_FILTER,
            gleam::gl::LINEAR as i32,
        );
        gl.tex_parameter_i(
            gleam::gl::TEXTURE_2D,
            gleam::gl::TEXTURE_MAG_FILTER,
            gleam::gl::LINEAR as i32,
        );
        gl.tex_parameter_i(
            gleam::gl::TEXTURE_2D,
            gleam::gl::TEXTURE_WRAP_S,
            gleam::gl::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i(
            gleam::gl::TEXTURE_2D,
            gleam::gl::TEXTURE_WRAP_T,
            gleam::gl::CLAMP_TO_EDGE as i32,
        );
        gl.bind_texture(gleam::gl::TEXTURE_2D, 0);
        Some(ImportedTexture {
            texture,
            image,
            display,
        })
    }
}

/// Deletes the texture and the `EGLImage` of an imported frame.
pub fn destroy(gl: &dyn gleam::gl::Gl, imported: ImportedTexture) {
    gl.delete_textures(&[imported.texture]);
    if let Some(egl) = egl() {
        unsafe {
            (egl.destroy_image)(imported.display, imported.image);
        }
    }
}
