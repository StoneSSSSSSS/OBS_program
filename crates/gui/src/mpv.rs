//! Minimal libmpv binding (loaded at runtime from libmpv-2.dll) and a player
//! that renders into the egui window through mpv's OpenGL render API.

use anyhow::{anyhow, bail, Context, Result};
use eframe::egui;
use parking_lot::Mutex;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[repr(C)]
struct MpvHandle {
    _p: [u8; 0],
}
#[repr(C)]
struct MpvRenderCtx {
    _p: [u8; 0],
}

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvRenderParam {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvOpenglInitParams {
    get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct MpvOpenglFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

const FMT_FLAG: c_int = 3;
const FMT_INT64: c_int = 4;
const FMT_DOUBLE: c_int = 5;

const EV_SHUTDOWN: c_int = 1;
const EV_END_FILE: c_int = 7;
const EV_FILE_LOADED: c_int = 8;
const EV_PLAYBACK_RESTART: c_int = 21;
const EV_PROPERTY_CHANGE: c_int = 22;

const RP_API_TYPE: c_int = 1;
const RP_OPENGL_INIT: c_int = 2;
const RP_OPENGL_FBO: c_int = 3;
const RP_FLIP_Y: c_int = 4;
const RP_BLOCK_FOR_TARGET: c_int = 12;
const RENDER_UPDATE_FRAME: u64 = 1;

type WakeupCb = unsafe extern "C" fn(*mut c_void);

struct Api {
    _lib: libloading::Library,
    create: unsafe extern "C" fn() -> *mut MpvHandle,
    initialize: unsafe extern "C" fn(*mut MpvHandle) -> c_int,
    terminate_destroy: unsafe extern "C" fn(*mut MpvHandle),
    set_option_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int,
    set_property_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int,
    get_property_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> *mut c_char,
    command: unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int,
    command_async: unsafe extern "C" fn(*mut MpvHandle, u64, *const *const c_char) -> c_int,
    observe_property: unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int,
    wait_event: unsafe extern "C" fn(*mut MpvHandle, f64) -> *mut MpvEvent,
    set_wakeup_callback: unsafe extern "C" fn(*mut MpvHandle, Option<WakeupCb>, *mut c_void),
    free: unsafe extern "C" fn(*mut c_void),
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    rc_create: unsafe extern "C" fn(*mut *mut MpvRenderCtx, *mut MpvHandle, *mut MpvRenderParam) -> c_int,
    rc_render: unsafe extern "C" fn(*mut MpvRenderCtx, *mut MpvRenderParam) -> c_int,
    rc_update: unsafe extern "C" fn(*mut MpvRenderCtx) -> u64,
    rc_set_update_callback: unsafe extern "C" fn(*mut MpvRenderCtx, Option<WakeupCb>, *mut c_void),
    rc_free: unsafe extern "C" fn(*mut MpvRenderCtx),
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

impl Api {
    fn load(path: &Path) -> Result<Api> {
        unsafe {
            let lib = libloading::Library::new(path).with_context(|| format!("loading {}", path.display()))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes()).with_context(|| format!("libmpv is missing {}", $name))?
                };
            }
            Ok(Api {
                create: sym!("mpv_create"),
                initialize: sym!("mpv_initialize"),
                terminate_destroy: sym!("mpv_terminate_destroy"),
                set_option_string: sym!("mpv_set_option_string"),
                set_property_string: sym!("mpv_set_property_string"),
                get_property_string: sym!("mpv_get_property_string"),
                command: sym!("mpv_command"),
                command_async: sym!("mpv_command_async"),
                observe_property: sym!("mpv_observe_property"),
                wait_event: sym!("mpv_wait_event"),
                set_wakeup_callback: sym!("mpv_set_wakeup_callback"),
                free: sym!("mpv_free"),
                error_string: sym!("mpv_error_string"),
                rc_create: sym!("mpv_render_context_create"),
                rc_render: sym!("mpv_render_context_render"),
                rc_update: sym!("mpv_render_context_update"),
                rc_set_update_callback: sym!("mpv_render_context_set_update_callback"),
                rc_free: sym!("mpv_render_context_free"),
                _lib: lib,
            })
        }
    }

    fn err(&self, code: c_int) -> String {
        unsafe { CStr::from_ptr((self.error_string)(code)).to_string_lossy().to_string() }
    }
}

/// Find libmpv-2.dll: configured path, next to the exe, or the dev third_party folder.
pub fn find_libmpv(configured: &str) -> Option<PathBuf> {
    if !configured.trim().is_empty() {
        let p = PathBuf::from(configured.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    cv_core::paths::find_resource("libmpv-2.dll")
}

/// Observable playback state, updated from mpv events.
#[derive(Debug, Clone, Default)]
pub struct PlayState {
    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub eof: bool,
    pub seeking: bool,
    pub loaded: bool,
    pub idle: bool,
    pub speed: f64,
    pub video_w: i64,
    pub video_h: i64,
    pub cache_end: f64,
    pub file_generation: u64,
    pub last_error: Option<String>,
}

struct RenderRes {
    ctx: *mut MpvRenderCtx,
    fbo: Option<glow::Framebuffer>,
    tex: Option<glow::Texture>,
    size: (i32, i32),
    has_frame: bool,
}
unsafe impl Send for RenderRes {}

pub struct Mpv {
    api: Arc<Api>,
    h: *mut MpvHandle,
    render: Arc<Mutex<RenderRes>>,
    state: Mutex<PlayState>,
    get_proc: Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>,
    repaint: Box<egui::Context>,
    pending_af: Mutex<Option<(String, std::time::Instant)>>,
}

unsafe impl Send for Mpv {}
unsafe impl Sync for Mpv {}

unsafe extern "C" fn wakeup(ctx: *mut c_void) {
    let ctx = &*(ctx as *const egui::Context);
    ctx.request_repaint();
}

unsafe extern "C" fn get_proc_trampoline(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    let f = &*(ctx as *const Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>);
    f(CStr::from_ptr(name)) as *mut c_void
}

impl Mpv {
    pub fn new(
        dll: &Path,
        egui_ctx: &egui::Context,
        get_proc: Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>,
        cfg: &cv_core::config::PlayerConfig,
    ) -> Result<Arc<Mpv>> {
        let api = Arc::new(Api::load(dll)?);
        unsafe {
            let h = (api.create)();
            if h.is_null() {
                bail!("mpv_create failed");
            }
            let mut opts: Vec<(&str, String)> = vec![
                ("vo", "libmpv".into()),
                ("config", "no".into()),
                ("terminal", "no".into()),
                ("msg-level", "all=error".into()),
                ("idle", "yes".into()),
                ("keep-open", "yes".into()),
                ("osc", "no".into()),
                ("osd-level", "0".into()),
                ("input-default-bindings", "no".into()),
                ("input-vo-keyboard", "no".into()),
                ("load-scripts", "no".into()),
                ("ytdl", "no".into()),
                ("sub-auto", "no".into()),
                ("audio-file-auto", "no".into()),
                ("hwdec", cfg.hwdec.clone()),
                ("hr-seek", "yes".into()),
                ("hr-seek-framedrop", "yes".into()),
                ("cache", "yes".into()),
                ("demuxer-max-bytes", "400MiB".into()),
                ("demuxer-max-back-bytes", "200MiB".into()),
                ("volume", cfg.volume.to_string()),
                ("volume-max", "150".into()),
                ("background-color", "#000000".into()),
                ("video-sync", "audio".into()),
                ("audio-client-name", "ClipVault".into()),
                ("pause", "yes".into()),
            ];
            if let Some(f) = std::env::var_os("CLIPVAULT_MPV_LOG") {
                opts.push(("log-file", f.to_string_lossy().to_string()));
            }
            for (k, v) in opts {
                let (k, v) = (CString::new(k)?, CString::new(v)?);
                let r = (api.set_option_string)(h, k.as_ptr(), v.as_ptr());
                if r < 0 {
                    tracing::warn!("mpv option {:?}: {}", k, api.err(r));
                }
            }
            let r = (api.initialize)(h);
            if r < 0 {
                (api.terminate_destroy)(h);
                bail!("mpv_initialize: {}", api.err(r));
            }
            for (i, (name, fmt)) in [
                ("time-pos", FMT_DOUBLE),
                ("duration", FMT_DOUBLE),
                ("pause", FMT_FLAG),
                ("eof-reached", FMT_FLAG),
                ("seeking", FMT_FLAG),
                ("speed", FMT_DOUBLE),
                ("idle-active", FMT_FLAG),
                ("width", FMT_INT64),
                ("height", FMT_INT64),
                ("demuxer-cache-time", FMT_DOUBLE),
            ]
            .iter()
            .enumerate()
            {
                let n = CString::new(*name)?;
                (api.observe_property)(h, i as u64 + 1, n.as_ptr(), *fmt);
            }
            let repaint = Box::new(egui_ctx.clone());
            (api.set_wakeup_callback)(h, Some(wakeup), &*repaint as *const egui::Context as *mut c_void);
            Ok(Arc::new(Mpv {
                api,
                h,
                render: Arc::new(Mutex::new(RenderRes { ctx: std::ptr::null_mut(), fbo: None, tex: None, size: (0, 0), has_frame: false })),
                state: Mutex::new(PlayState { speed: 1.0, paused: true, idle: true, ..Default::default() }),
                get_proc,
                repaint,
                pending_af: Mutex::new(None),
            }))
        }
    }

    pub fn command(&self, args: &[&str]) -> Result<()> {
        let cs: Vec<CString> = args.iter().map(|a| CString::new(*a)).collect::<std::result::Result<_, _>>()?;
        let mut ptrs: Vec<*const c_char> = cs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let r = unsafe { (self.api.command)(self.h, ptrs.as_ptr()) };
        if r < 0 {
            return Err(anyhow!("mpv {}: {}", args.first().unwrap_or(&""), self.api.err(r)));
        }
        Ok(())
    }

    /// Fire-and-forget command (doesn't block the UI thread on slow seeks).
    pub fn command_async(&self, args: &[&str]) {
        let Ok(cs) = args.iter().map(|a| CString::new(*a)).collect::<std::result::Result<Vec<_>, _>>() else { return };
        let mut ptrs: Vec<*const c_char> = cs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        unsafe {
            (self.api.command_async)(self.h, 0, ptrs.as_ptr());
        }
    }

    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        let (n, v) = (CString::new(name)?, CString::new(value)?);
        let r = unsafe { (self.api.set_property_string)(self.h, n.as_ptr(), v.as_ptr()) };
        if r < 0 {
            return Err(anyhow!("mpv set {name}: {}", self.api.err(r)));
        }
        Ok(())
    }

    pub fn get_string(&self, name: &str) -> Option<String> {
        let n = CString::new(name).ok()?;
        unsafe {
            let p = (self.api.get_property_string)(self.h, n.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().to_string();
            (self.api.free)(p as *mut c_void);
            Some(s)
        }
    }

    pub fn state(&self) -> PlayState {
        self.state.lock().clone()
    }

    /// Drain pending mpv events into `state`. Call once per frame.
    pub fn pump(&self) {
        loop {
            let ev = unsafe { &*(self.api.wait_event)(self.h, 0.0) };
            if ev.event_id == 0 {
                break;
            }
            let mut st = self.state.lock();
            match ev.event_id {
                EV_PROPERTY_CHANGE => unsafe {
                    let p = &*(ev.data as *const MpvEventProperty);
                    let name = CStr::from_ptr(p.name).to_string_lossy();
                    let has = !p.data.is_null();
                    let f = || if has { *(p.data as *const f64) } else { 0.0 };
                    let b = || has && *(p.data as *const c_int) != 0;
                    let i = || if has { *(p.data as *const i64) } else { 0 };
                    match name.as_ref() {
                        "time-pos" if p.format == FMT_DOUBLE => st.time_pos = f(),
                        "duration" if p.format == FMT_DOUBLE => st.duration = f(),
                        "pause" => st.paused = b(),
                        "eof-reached" => st.eof = b(),
                        "seeking" => st.seeking = b(),
                        "speed" if p.format == FMT_DOUBLE => st.speed = f(),
                        "idle-active" => st.idle = b(),
                        "width" => st.video_w = i(),
                        "height" => st.video_h = i(),
                        "demuxer-cache-time" if p.format == FMT_DOUBLE => st.cache_end = f(),
                        _ => {}
                    }
                },
                EV_FILE_LOADED => {
                    tracing::info!("mpv: file loaded");
                    st.loaded = true;
                    st.eof = false;
                    st.last_error = None;
                    st.file_generation += 1;
                }
                EV_END_FILE => {
                    // data: mpv_event_end_file { int reason; int error; ... }
                    unsafe {
                        if !ev.data.is_null() {
                            let reason = *(ev.data as *const c_int);
                            let err = *(ev.data as *const c_int).add(1);
                            tracing::info!("mpv: end file (reason {reason}, error {})", self.api.err(err));
                            if reason == 4 {
                                st.last_error = Some(self.api.err(err));
                                st.loaded = false;
                            }
                        }
                    }
                }
                EV_PLAYBACK_RESTART => st.seeking = false,
                EV_SHUTDOWN => break,
                _ => {}
            }
        }
        // Apply a debounced filter-string update (see `set_gains`).
        let due = {
            let p = self.pending_af.lock();
            p.as_ref().map(|(_, t)| t.elapsed() > std::time::Duration::from_millis(350)).unwrap_or(false)
        };
        if due {
            if let Some((af, _)) = self.pending_af.lock().take() {
                let _ = self.set("af", &af);
            }
        }
    }

    /// Load a file. `tracks` is the known audio track count (0 = unknown).
    /// `exprs` = per-track gain expressions (see `cv_core::mixer::track_expr`).
    pub fn load(&self, path: &str, start: f64, tracks: usize, exprs: &[String], live: bool, pause: bool) -> Result<()> {
        {
            let mut st = self.state.lock();
            st.loaded = false;
            st.eof = false;
            st.time_pos = start;
            st.duration = 0.0;
            st.last_error = None;
        }
        *self.pending_af.lock() = None;
        let lc = cv_core::mixer::mpv_lavfi_complex(tracks).unwrap_or_default();
        self.set("lavfi-complex", &lc)?;
        self.set("af", &cv_core::mixer::mpv_af(exprs))?;
        self.set("pause", if pause { "yes" } else { "no" })?;
        let target = if live { format!("appending://{path}") } else { path.to_string() };
        let opts = format!("start={start:.3}");
        tracing::info!("mpv: loadfile {target} start={start:.1} tracks={tracks} live={live}");
        self.command(&["loadfile", &target, "replace", "-1", &opts])
    }

    /// Change per-track gain expressions live: the changed track (or all, if
    /// `changed` is None) is updated instantly via af-command, and the full
    /// filter string is re-applied shortly after so seeks keep the new values.
    pub fn set_exprs(&self, exprs: &[String], changed: Option<usize>) {
        let targets: Vec<usize> = match changed {
            Some(i) => vec![i],
            None => (0..exprs.len()).collect(),
        };
        for i in targets {
            if let Some(e) = exprs.get(i) {
                let cmd = cv_core::mixer::mpv_gain_command(i, e);
                let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
                let _ = self.command(&refs);
            }
        }
        *self.pending_af.lock() = Some((cv_core::mixer::mpv_af(exprs), std::time::Instant::now()));
    }

    pub fn seek_abs(&self, t: f64, exact: bool) {
        let t = format!("{:.3}", t.max(0.0));
        self.command_async(&["seek", &t, if exact { "absolute+exact" } else { "absolute+keyframes" }]);
    }

    pub fn set_pause(&self, p: bool) {
        let _ = self.set("pause", if p { "yes" } else { "no" });
    }

    pub fn stop(&self) {
        let _ = self.command(&["stop"]);
        let mut st = self.state.lock();
        st.loaded = false;
        st.time_pos = 0.0;
        st.duration = 0.0;
        self.render.lock().has_frame = false;
    }

    /// Paint callback that renders the current frame into `rect`.
    pub fn paint_callback(self: &Arc<Self>, rect: egui::Rect) -> egui::PaintCallback {
        let me = self.clone();
        egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
                me.render_into(info, painter.gl());
            })),
        }
    }

    fn render_into(&self, info: egui::PaintCallbackInfo, gl: &Arc<glow::Context>) {
        use glow::HasContext;
        let vp = info.viewport_in_pixels();
        let (w, h) = (vp.width_px.max(1), vp.height_px.max(1));
        let mut rr = self.render.lock();
        unsafe {
            if rr.ctx.is_null() {
                let api_type = CString::new("opengl").unwrap();
                let gp: *const Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync> = &self.get_proc;
                let mut init = MpvOpenglInitParams { get_proc_address: Some(get_proc_trampoline), get_proc_address_ctx: gp as *mut c_void };
                let mut block = 0 as c_int;
                let mut params = [
                    MpvRenderParam { kind: RP_API_TYPE, data: api_type.as_ptr() as *mut c_void },
                    MpvRenderParam { kind: RP_OPENGL_INIT, data: &mut init as *mut _ as *mut c_void },
                    MpvRenderParam { kind: RP_BLOCK_FOR_TARGET, data: &mut block as *mut _ as *mut c_void },
                    MpvRenderParam { kind: 0, data: std::ptr::null_mut() },
                ];
                let mut ctx: *mut MpvRenderCtx = std::ptr::null_mut();
                let r = (self.api.rc_create)(&mut ctx, self.h, params.as_mut_ptr());
                if r < 0 {
                    tracing::error!("mpv render context: {}", self.api.err(r));
                    return;
                }
                (self.api.rc_set_update_callback)(ctx, Some(wakeup), &*self.repaint as *const egui::Context as *mut c_void);
                tracing::info!("mpv: render context created");
                rr.ctx = ctx;
            }
            // (Re)create our framebuffer when the size changes.
            if rr.size != (w, h) || rr.fbo.is_none() {
                if let Some(f) = rr.fbo.take() {
                    gl.delete_framebuffer(f);
                }
                if let Some(t) = rr.tex.take() {
                    gl.delete_texture(t);
                }
                let tex = gl.create_texture().ok();
                gl.bind_texture(glow::TEXTURE_2D, tex);
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                let fbo = gl.create_framebuffer().ok();
                gl.bind_framebuffer(glow::FRAMEBUFFER, fbo);
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, tex, 0);
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                rr.fbo = fbo;
                rr.tex = tex;
                rr.size = (w, h);
                rr.has_frame = false;
            }
            let prev_draw = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
            let flags = (self.api.rc_update)(rr.ctx);
            if flags & RENDER_UPDATE_FRAME != 0 || !rr.has_frame {
                let fbo_id = rr.fbo.map(|f| f.0.get() as c_int).unwrap_or(0);
                let mut fbo = MpvOpenglFbo { fbo: fbo_id, w, h, internal_format: 0 };
                let mut flip: c_int = 1;
                let mut block: c_int = 0;
                let mut params = [
                    MpvRenderParam { kind: RP_OPENGL_FBO, data: &mut fbo as *mut _ as *mut c_void },
                    MpvRenderParam { kind: RP_FLIP_Y, data: &mut flip as *mut _ as *mut c_void },
                    MpvRenderParam { kind: RP_BLOCK_FOR_TARGET, data: &mut block as *mut _ as *mut c_void },
                    MpvRenderParam { kind: 0, data: std::ptr::null_mut() },
                ];
                gl.disable(glow::SCISSOR_TEST);
                (self.api.rc_render)(rr.ctx, params.as_mut_ptr());
                rr.has_frame = true;
            }
            // Blit our frame into egui's framebuffer at the widget rect.
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, rr.fbo);
            let draw = std::num::NonZeroU32::new(prev_draw as u32).map(glow::NativeFramebuffer);
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, draw);
            gl.enable(glow::SCISSOR_TEST);
            let clip = info.clip_rect_in_pixels();
            gl.scissor(clip.left_px, clip.from_bottom_px, clip.width_px, clip.height_px);
            gl.blit_framebuffer(
                0,
                0,
                w,
                h,
                vp.left_px,
                vp.from_bottom_px,
                vp.left_px + w,
                vp.from_bottom_px + h,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, draw);
        }
    }

    /// Must be called with the GL context current (on exit).
    pub fn destroy_gl(&self, gl: &glow::Context) {
        use glow::HasContext;
        let mut rr = self.render.lock();
        unsafe {
            if !rr.ctx.is_null() {
                (self.api.rc_free)(rr.ctx);
                rr.ctx = std::ptr::null_mut();
            }
            if let Some(f) = rr.fbo.take() {
                gl.delete_framebuffer(f);
            }
            if let Some(t) = rr.tex.take() {
                gl.delete_texture(t);
            }
        }
    }

    /// Number of audio tracks in the loaded file.
    pub fn audio_track_count(&self) -> usize {
        let n: usize = self.get_string("track-list/count").and_then(|s| s.parse().ok()).unwrap_or(0);
        (0..n).filter(|i| self.get_string(&format!("track-list/{i}/type")).as_deref() == Some("audio")).count()
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        unsafe {
            (self.api.set_wakeup_callback)(self.h, None, std::ptr::null_mut());
            let rr = self.render.lock();
            if !rr.ctx.is_null() {
                (self.api.rc_set_update_callback)(rr.ctx, None, std::ptr::null_mut());
            }
            drop(rr);
            (self.api.terminate_destroy)(self.h);
        }
    }
}
