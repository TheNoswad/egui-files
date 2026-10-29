//! The video thread: owns the GL context and all mpv instances.

use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender},
    },
};

use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use glow::HasContext;

use super::{
    Command, Message, Mode, Status,
    egl::GlContext,
    mpv::{self, Event, Mpv, PropertyValue, RenderContext, RenderParam},
};

const PROPERTY_POSITION: u64 = 1;
const PROPERTY_DURATION: u64 = 2;
const PROPERTY_PAUSE: u64 = 3;
const PROPERTY_EOF: u64 = 6;
const PROPERTY_MUTE: u64 = 7;
const PROPERTY_VOLUME: u64 = 8;

pub(super) fn spawn(
    ctx: Context,
    receiver: Receiver<Message>,
    sender: Sender<Message>,
    error: Arc<OnceLock<String>>,
) {
    std::thread::Builder::new()
        .name("video".into())
        .spawn(move || {
            if let Err(err) = run(&ctx, &receiver, &sender) {
                let _ = error.set(err);
                ctx.request_repaint();
            }
        })
        .expect("failed to spawn video thread");
}

fn run(
    ctx: &Context,
    receiver: &Receiver<Message>,
    sender: &Sender<Message>,
) -> Result<(), String> {
    mpv::available()?;
    // Boxed: mpv keeps a pointer to it for `get_proc_address`.
    let gl = Box::new(GlContext::new()?);
    let mut players: HashMap<u64, VideoPlayer> = HashMap::new();
    while let Ok(message) = receiver.recv() {
        match message {
            Message::Open {
                id,
                mode,
                texture,
                status,
                volume,
            } => match VideoPlayer::open(
                &gl,
                id,
                mode,
                volume,
                texture,
                status.clone(),
                sender.clone(),
            ) {
                Ok(player) => {
                    players.insert(id, player);
                }
                Err(err) => {
                    status.lock().expect("video status poisoned").error = Some(err);
                    ctx.request_repaint();
                }
            },
            Message::Close(id) => {
                if let Some(player) = players.remove(&id) {
                    player.destroy(&gl);
                }
            }
            Message::Command(id, command) => {
                if let Some(player) = players.get_mut(&id) {
                    player.command(&gl, command, ctx);
                }
            }
            Message::Wake(id) => {
                if let Some(player) = players.get_mut(&id) {
                    player.wake(&gl, ctx);
                }
            }
        }
    }
    for (_, player) in players.drain() {
        player.destroy(&gl);
    }
    Ok(())
}

/// Passed to mpv's callbacks; wakes the video thread for one player.
struct Waker {
    sender: Sender<Message>,
    id: u64,
    /// Coalesces callbacks until the thread has handled the last one.
    pending: AtomicBool,
}

unsafe extern "C" fn wake(data: *mut c_void) {
    // SAFETY: `data` is the player's boxed Waker, which outlives mpv.
    let waker = unsafe { &*(data as *const Waker) };
    if !waker.pending.swap(true, Ordering::AcqRel) {
        let _ = waker.sender.send(Message::Wake(waker.id));
    }
}

struct Target {
    framebuffer: glow::NativeFramebuffer,
    texture: glow::NativeTexture,
    size: [u32; 2],
}

struct VideoPlayer {
    // Field order is drop order: the render context goes before mpv, and
    // everything mpv points to goes after it.
    render: RenderContext,
    mpv: Mpv,
    waker: Box<Waker>,
    _drm: Option<Box<mpv::DrmParamsV2>>,
    mode: Mode,
    target: Option<Target>,
    size: [u32; 2],
    pixels: Vec<u8>,
    texture: TextureHandle,
    /// mpv's state as last reported, published to `status` whole so the UI
    /// never sees half an update. mpv only reports changes, so this is kept
    /// across files.
    state: Status,
    width: u32,
    height: u32,
    /// The current file has loaded.
    file_loaded: bool,
    /// The current file's video is set up: frames and sizes are no longer the previous file's.
    loaded: bool,
    status: Arc<Mutex<Status>>,
}

impl VideoPlayer {
    fn open(
        gl: &GlContext,
        id: u64,
        mode: Mode,
        volume: f64,
        texture: TextureHandle,
        status: Arc<Mutex<Status>>,
        sender: Sender<Message>,
    ) -> Result<Self, String> {
        let mpv = Mpv::new()?;
        for (name, value) in [
            ("config", "no"),
            ("terminal", "no"),
            ("vo", "libmpv"),
            ("hwdec", "auto-safe"),
            ("idle", "yes"),
            ("keep-open", "yes"),
            ("input-default-bindings", "no"),
            ("osc", "no"),
            ("ytdl", "no"),
            ("load-scripts", "no"),
            ("audio-display", "no"),
        ] {
            mpv.set_option(name, value)?;
        }
        // mpv loads every interop by default; CUDA's prints "Cannot load
        // libcuda.so.1" on machines without NVIDIA's driver.
        if let Some(interop) = gl.hwdec_interop() {
            mpv.set_option("gpu-hwdec-interop", interop)?;
        }
        // e.g. EGUI_FILES_MPV_LOG=vd=v,vo=v to see which hardware decoder is used.
        if let Ok(levels) = std::env::var("EGUI_FILES_MPV_LOG") {
            mpv.set_option("terminal", "yes")?;
            mpv.set_option("msg-level", &levels)?;
        }
        match mode {
            Mode::Preview => {
                mpv.set_option("pause", "yes")?;
                mpv.set_option("volume", &volume.to_string())?;
            }
            Mode::Hover => {
                mpv.set_option("aid", "no")?;
                mpv.set_option("sid", "no")?;
                mpv.set_option("loop-file", "inf")?;
            }
        }
        mpv.initialize()?;
        for (id, name, format) in [
            (PROPERTY_POSITION, "time-pos", mpv::FORMAT_DOUBLE),
            (PROPERTY_DURATION, "duration", mpv::FORMAT_DOUBLE),
            (PROPERTY_PAUSE, "pause", mpv::FORMAT_FLAG),
            (PROPERTY_EOF, "eof-reached", mpv::FORMAT_FLAG),
            (PROPERTY_MUTE, "mute", mpv::FORMAT_FLAG),
            (PROPERTY_VOLUME, "volume", mpv::FORMAT_DOUBLE),
        ] {
            mpv.observe(id, name, format)?;
        }

        let mut init = mpv::OpenGlInitParams {
            get_proc_address: GlContext::get_proc_address,
            get_proc_address_ctx: (gl as *const GlContext).cast_mut().cast(),
        };
        // mpv opens VA-API on this render node for zero-copy hardware decoding.
        let mut drm = gl.render_node_fd().map(|render_fd| {
            Box::new(mpv::DrmParamsV2 {
                fd: -1,
                crtc_id: -1,
                connector_id: -1,
                atomic_request_ptr: std::ptr::null_mut(),
                render_fd,
            })
        });
        let mut params = vec![
            RenderParam {
                kind: mpv::RENDER_PARAM_API_TYPE,
                data: c"opengl".as_ptr().cast_mut().cast(),
            },
            RenderParam {
                kind: mpv::RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&raw mut init).cast(),
            },
        ];
        if let Some(drm) = &mut drm {
            params.push(RenderParam {
                kind: mpv::RENDER_PARAM_DRM_DISPLAY_V2,
                data: (&raw mut **drm).cast(),
            });
        }
        params.push(RenderParam {
            kind: 0,
            data: std::ptr::null_mut(),
        });
        // SAFETY: the GL context is current on this thread; the list is terminated.
        let render = unsafe { RenderContext::new(&mpv, &mut params)? };

        let waker = Box::new(Waker {
            sender,
            id,
            pending: AtomicBool::new(false),
        });
        let data = (&raw const *waker).cast_mut().cast();
        // SAFETY: the waker is dropped after both mpv objects.
        unsafe {
            mpv.set_wakeup_callback(wake, data);
            render.set_update_callback(wake, data);
        }
        let state = status.lock().expect("video status poisoned").clone();
        Ok(Self {
            render,
            mpv,
            waker,
            _drm: drm,
            mode,
            target: None,
            size: [0, 0],
            pixels: Vec::new(),
            texture,
            state,
            width: 0,
            height: 0,
            file_loaded: false,
            loaded: false,
            status,
        })
    }

    fn destroy(mut self, gl: &GlContext) {
        if let Some(target) = self.target.take() {
            // SAFETY: objects of this (current) context.
            unsafe {
                gl.gl.delete_framebuffer(target.framebuffer);
                gl.gl.delete_texture(target.texture);
            }
        }
    }

    fn publish(&mut self, ctx: &Context) {
        self.state.video_size =
            (self.loaded && self.width > 0 && self.height > 0).then_some([self.width, self.height]);
        *self.status.lock().expect("video status poisoned") = self.state.clone();
        ctx.request_repaint();
    }

    fn report(&mut self, result: Result<(), String>, ctx: &Context) {
        if let Err(err) = result {
            self.state.error = Some(err);
            self.publish(ctx);
        }
    }

    fn command(&mut self, gl: &GlContext, command: Command, ctx: &Context) {
        let result = match command {
            Command::Load { path, generation } => {
                // Events still queued are about the previous file.
                self.handle_events();
                self.state.generation = generation;
                self.state.has_frame = false;
                self.state.error = None;
                self.file_loaded = false;
                self.loaded = false;
                self.width = 0;
                self.height = 0;
                let result = if self.mode == Mode::Preview {
                    self.state.paused = true;
                    self.mpv.set_property("pause", "yes")
                } else {
                    Ok(())
                };
                let result = result.and_then(|()| self.mpv.load_file(&path));
                self.publish(ctx);
                result
            }
            Command::Resize(size) => {
                self.size = size;
                // Paused videos get no new frames; redraw the current one.
                if self.state.has_frame {
                    self.draw(gl, ctx);
                }
                Ok(())
            }
            // The state is updated right away so controls don't jump back
            // until mpv confirms.
            Command::Pause(paused) => {
                self.state.paused = paused;
                self.mpv
                    .set_property("pause", if paused { "yes" } else { "no" })
            }
            Command::Seek(seconds) => {
                self.state.position = seconds;
                self.state.eof = false;
                self.mpv
                    .command(&["seek", &seconds.to_string(), "absolute"])
            }
            Command::Mute(muted) => {
                self.state.muted = muted;
                self.mpv
                    .set_property("mute", if muted { "yes" } else { "no" })
            }
            Command::Volume(volume) => {
                self.state.volume = volume;
                self.mpv.set_property("volume", &volume.to_string())
            }
        };
        self.report(result, ctx);
    }

    fn wake(&mut self, gl: &GlContext, ctx: &Context) {
        self.waker.pending.store(false, Ordering::Release);
        let changed = self.handle_events();
        if self.render.update() & mpv::RENDER_UPDATE_FRAME != 0 {
            self.draw(gl, ctx);
        } else if changed {
            self.publish(ctx);
        }
    }

    /// Applies pending mpv events to `state`; returns whether there were any.
    fn handle_events(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.mpv.poll_event() {
            changed = true;
            let status = &mut self.state;
            match event {
                Event::Property { id, value } => {
                    let double = match value {
                        PropertyValue::Double(value) => Some(value),
                        _ => None,
                    };
                    let flag = matches!(value, PropertyValue::Flag(true));
                    match id {
                        PROPERTY_POSITION => status.position = double.unwrap_or(0.0),
                        PROPERTY_DURATION => status.duration = double.unwrap_or(0.0),
                        PROPERTY_PAUSE => status.paused = flag,
                        PROPERTY_EOF => status.eof = flag,
                        PROPERTY_MUTE => status.muted = flag,
                        PROPERTY_VOLUME => status.volume = double.unwrap_or(status.volume),
                        _ => {}
                    }
                }
                Event::FileLoaded => {
                    self.file_loaded = true;
                    status.error = None;
                }
                // Sizes are read here rather than observed: mpv reports
                // width and height changes separately, and not at all when the
                // next file has the same size, so observed values can be the
                // previous file's.
                Event::VideoReconfig if self.file_loaded => {
                    let side = |name| {
                        self.mpv
                            .get_int(name)
                            .and_then(|side| u32::try_from(side).ok())
                            .unwrap_or(0)
                    };
                    self.width = side("dwidth");
                    self.height = side("dheight");
                    self.loaded = self.width > 0 && self.height > 0;
                }
                Event::VideoReconfig => {}
                Event::EndFile { error } => {
                    if error.is_some() {
                        status.error = error;
                    }
                }
                Event::Shutdown => {}
            }
        }
        changed
    }

    /// Renders the current frame at `self.size` and copies it into the texture.
    ///
    /// mpv stalls playback until each new frame is rendered, so this also runs
    /// before the UI has picked a size, using the video's own size (capped).
    fn draw(&mut self, gl: &GlContext, ctx: &Context) {
        let size = if self.size.contains(&0) {
            let [w, h] = if self.width > 0 && self.height > 0 {
                [self.width, self.height]
            } else {
                [16, 16]
            };
            let scale = (512.0 / w.max(h) as f32).min(1.0);
            [w, h].map(|side| ((side as f32 * scale) as u32).max(1))
        } else {
            self.size
        };
        let [width, height] = size;
        let target = match self.target.take() {
            Some(target) if target.size == size => target,
            old => {
                // SAFETY: the context is current; objects are deleted only once.
                match unsafe { create_target(&gl.gl, size, old) } {
                    Ok(target) => target,
                    Err(err) => return self.report(Err(err), ctx),
                }
            }
        };
        let mut fbo = mpv::OpenGlFbo {
            fbo: target.framebuffer.0.get() as i32,
            w: width as i32,
            h: height as i32,
            internal_format: 0,
        };
        let mut flip_y: i32 = 0;
        let mut block: i32 = 0;
        let mut params = [
            RenderParam {
                kind: mpv::RENDER_PARAM_OPENGL_FBO,
                data: (&raw mut fbo).cast(),
            },
            RenderParam {
                kind: mpv::RENDER_PARAM_FLIP_Y,
                data: (&raw mut flip_y).cast(),
            },
            RenderParam {
                kind: mpv::RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: (&raw mut block).cast(),
            },
            RenderParam {
                kind: 0,
                data: std::ptr::null_mut(),
            },
        ];
        // SAFETY: the context is current; the list is terminated.
        if let Err(err) = unsafe { self.render.render(&mut params) } {
            self.target = Some(target);
            return self.report(Err(err), ctx);
        }

        self.pixels.resize(width as usize * height as usize * 4, 0);
        // SAFETY: the buffer holds exactly width × height RGBA pixels.
        unsafe {
            gl.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
            gl.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.gl.read_pixels(
                0,
                0,
                width as i32,
                height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut self.pixels)),
            );
            gl.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        self.target = Some(target);
        let image =
            ColorImage::from_rgba_premultiplied([width as usize, height as usize], &self.pixels);
        self.texture.set(image, TextureOptions::LINEAR);
        // Until then it may be the previous file's last frame.
        self.state.has_frame = self.loaded;
        self.publish(ctx);
    }
}

/// Creates an RGBA framebuffer of `size`, deleting `old`.
unsafe fn create_target(
    gl: &glow::Context,
    size: [u32; 2],
    old: Option<Target>,
) -> Result<Target, String> {
    // SAFETY: forwarded to the caller.
    unsafe {
        if let Some(old) = old {
            gl.delete_framebuffer(old.framebuffer);
            gl.delete_texture(old.texture);
        }
        let texture = gl.create_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            size[0] as i32,
            size[1] as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
        let framebuffer = gl.create_framebuffer()?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        if status != glow::FRAMEBUFFER_COMPLETE {
            gl.delete_framebuffer(framebuffer);
            gl.delete_texture(texture);
            return Err(format!("incomplete framebuffer ({status:#x})"));
        }
        Ok(Target {
            framebuffer,
            texture,
            size,
        })
    }
}
