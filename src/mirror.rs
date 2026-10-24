//! `sfrecorder --probe-mirror`: a one-off test of SteamVR's mirror texture,
//! which is documented as "the undistorted composited image for each eye".
//! If it includes Steam overlays and the HUD, it can replace panel capture,
//! which needs an admin permission. Saves one frame per eye as a BMP next to
//! the recordings and prints what it found.

use std::ffi::{c_void, CStr};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::openvr::{self, OpenVr};

const GET_MIRROR_TEXTURE_GL: usize = 37;
const RELEASE_SHARED_GL_TEXTURE: usize = 38;
const LOCK_GL_SHARED_TEXTURE: usize = 39;
const UNLOCK_GL_SHARED_TEXTURE: usize = 40;

const EGL_NONE: i32 = 0x3038;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_BIT: i32 = 0x0008;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_OPENGL_ES3_BIT: i32 = 0x0040;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_WIDTH: u32 = 0x1000;
const GL_TEXTURE_HEIGHT: u32 = 0x1001;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;

type Ptr = *mut c_void;

struct Gl {
    get_proc: unsafe extern "C" fn(*const std::ffi::c_char) -> Ptr,
}

impl Gl {
    unsafe fn f<T: Copy>(&self, name: &CStr) -> Result<T> {
        let p = (self.get_proc)(name.as_ptr().cast());
        if p.is_null() {
            bail!("no GL function {}", name.to_string_lossy());
        }
        Ok(std::mem::transmute_copy(&p))
    }
}

struct Egl {
    get_proc: unsafe extern "C" fn(*const std::ffi::c_char) -> Ptr,
    get_platform_display: unsafe extern "C" fn(u32, Ptr, *const isize) -> Ptr,
    get_display: unsafe extern "C" fn(Ptr) -> Ptr,
    initialize: unsafe extern "C" fn(Ptr, *mut i32, *mut i32) -> u32,
    bind_api: unsafe extern "C" fn(u32) -> u32,
    choose_config: unsafe extern "C" fn(Ptr, *const i32, *mut Ptr, i32, *mut i32) -> u32,
    create_context: unsafe extern "C" fn(Ptr, Ptr, Ptr, *const i32) -> Ptr,
    make_current: unsafe extern "C" fn(Ptr, Ptr, Ptr, Ptr) -> u32,
    get_error: unsafe extern "C" fn() -> i32,
}

unsafe fn load_egl() -> Result<Egl> {
    let lib = libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_NOW);
    if lib.is_null() {
        bail!("no libEGL.so.1");
    }
    let sym = |n: &CStr| -> Result<Ptr> {
        let p = libc::dlsym(lib, n.as_ptr());
        if p.is_null() {
            bail!("libEGL has no {}", n.to_string_lossy());
        }
        Ok(p)
    };
    Ok(Egl {
        get_proc: as_fn(sym(c"eglGetProcAddress")?),
        get_platform_display: as_fn(sym(c"eglGetPlatformDisplay")?),
        get_display: as_fn(sym(c"eglGetDisplay")?),
        initialize: as_fn(sym(c"eglInitialize")?),
        bind_api: as_fn(sym(c"eglBindAPI")?),
        choose_config: as_fn(sym(c"eglChooseConfig")?),
        create_context: as_fn(sym(c"eglCreateContext")?),
        make_current: as_fn(sym(c"eglMakeCurrent")?),
        get_error: as_fn(sym(c"eglGetError")?),
    })
}

/// A function pointer from dlsym as its real type.
unsafe fn as_fn<T: Copy>(p: Ptr) -> T {
    std::mem::transmute_copy(&p)
}

/// A GBM device on the GPU's render node, for EGL's GBM platform.
unsafe fn gbm_device() -> Option<Ptr> {
    let lib = libc::dlopen(c"libgbm.so.1".as_ptr(), libc::RTLD_NOW);
    if lib.is_null() {
        return None;
    }
    let create = libc::dlsym(lib, c"gbm_create_device".as_ptr());
    if create.is_null() {
        return None;
    }
    let create: unsafe extern "C" fn(i32) -> Ptr = as_fn(create);
    let node = std::fs::read_dir("/dev/dri").ok()?.flatten().map(|e| e.path()).find(|p| p.to_string_lossy().contains("renderD"))?;
    let path = std::ffi::CString::new(node.to_string_lossy().as_bytes()).ok()?;
    let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
    if fd < 0 {
        return None;
    }
    let dev = create(fd);
    (!dev.is_null()).then_some(dev)
}

/// An OpenGL (or OpenGL ES) context with no window, which the mirror
/// texture is shared into. Tries every way the system might offer one and
/// says what failed if none works.
unsafe fn gl_context() -> Result<(Gl, &'static str)> {
    let egl = load_egl()?;
    let none = [EGL_NONE as isize];
    let mut displays: Vec<(&str, Ptr)> = Vec::new();
    displays.push(("surfaceless", (egl.get_platform_display)(EGL_PLATFORM_SURFACELESS_MESA, std::ptr::null_mut(), none.as_ptr())));
    if let Some(gbm) = gbm_device() {
        displays.push(("gbm", (egl.get_platform_display)(EGL_PLATFORM_GBM_KHR, gbm, none.as_ptr())));
    }
    displays.push(("default", (egl.get_display)(std::ptr::null_mut())));

    let apis = [
        ("OpenGL", EGL_OPENGL_API, EGL_OPENGL_BIT, vec![EGL_NONE]),
        ("OpenGL ES 3", EGL_OPENGL_ES_API, EGL_OPENGL_ES3_BIT, vec![EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE]),
    ];
    let mut tried = Vec::new();
    for (dname, dpy) in displays {
        let (mut major, mut minor) = (0, 0);
        if dpy.is_null() || (egl.initialize)(dpy, &mut major, &mut minor) == 0 {
            tried.push(format!("{dname}: no display ({:#x})", (egl.get_error)()));
            continue;
        }
        for (aname, api, bit, ctx_attrs) in &apis {
            if (egl.bind_api)(*api) == 0 {
                tried.push(format!("{dname}/{aname}: API not available"));
                continue;
            }
            // Without a config first (EGL_KHR_no_config_context), then with one.
            let mut ctx = (egl.create_context)(dpy, std::ptr::null_mut(), std::ptr::null_mut(), ctx_attrs.as_ptr());
            if ctx.is_null() {
                let attrs = [EGL_RENDERABLE_TYPE, *bit, EGL_NONE];
                let (mut config, mut n) = (std::ptr::null_mut(), 0);
                if (egl.choose_config)(dpy, attrs.as_ptr(), &mut config, 1, &mut n) != 0 && n > 0 {
                    ctx = (egl.create_context)(dpy, config, std::ptr::null_mut(), ctx_attrs.as_ptr());
                }
            }
            if ctx.is_null() {
                tried.push(format!("{dname}/{aname}: no context ({:#x})", (egl.get_error)()));
                continue;
            }
            if (egl.make_current)(dpy, std::ptr::null_mut(), std::ptr::null_mut(), ctx) == 0 {
                tried.push(format!("{dname}/{aname}: can't make current ({:#x})", (egl.get_error)()));
                continue;
            }
            let label: &'static str = Box::leak(format!("{dname}/{aname}").into_boxed_str());
            return Ok((Gl { get_proc: egl.get_proc }, label));
        }
    }
    bail!("no OpenGL context: {}", tried.join("; "))
}

pub fn probe(out_dir: &Path) -> Result<()> {
    let vr = OpenVr::connect_as(openvr::APP_OVERLAY).context("connecting to SteamVR")?;
    let compositor = vr.table(openvr::COMPOSITOR).context("this SteamVR has no IVRCompositor_029")?;
    unsafe {
        let (gl, how) = gl_context()?;
        println!("OpenGL context: {how}");
        let bind: unsafe extern "C" fn(u32, u32) = gl.f(c"glBindTexture")?;
        let level_param: unsafe extern "C" fn(u32, i32, u32, *mut i32) = gl.f(c"glGetTexLevelParameteriv")?;
        let gen_fbs: unsafe extern "C" fn(i32, *mut u32) = gl.f(c"glGenFramebuffers")?;
        let bind_fb: unsafe extern "C" fn(u32, u32) = gl.f(c"glBindFramebuffer")?;
        let fb_tex: unsafe extern "C" fn(u32, u32, u32, u32, i32) = gl.f(c"glFramebufferTexture2D")?;
        let read_pixels: unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, Ptr) = gl.f(c"glReadPixels")?;
        let mut fbo = 0;
        gen_fbs(1, &mut fbo);
        bind_fb(GL_FRAMEBUFFER, fbo);
        let get_error: unsafe extern "C" fn() -> u32 = gl.f(c"glGetError")?;
        let get_mirror: unsafe extern "C" fn(i32, *mut u32, *mut Ptr) -> i32 = openvr::slot(compositor, GET_MIRROR_TEXTURE_GL);
        let release: unsafe extern "C" fn(u32, Ptr) -> bool = openvr::slot(compositor, RELEASE_SHARED_GL_TEXTURE);
        let lock: unsafe extern "C" fn(Ptr) = openvr::slot(compositor, LOCK_GL_SHARED_TEXTURE);
        let unlock: unsafe extern "C" fn(Ptr) = openvr::slot(compositor, UNLOCK_GL_SHARED_TEXTURE);

        for (eye, name) in [(0, "left"), (1, "right")] {
            let (mut tex, mut handle) = (0u32, std::ptr::null_mut());
            let err = get_mirror(eye, &mut tex, &mut handle);
            if err != 0 || tex == 0 {
                println!("{name} eye: no mirror texture (compositor error {err})");
                continue;
            }
            let (mut w, mut h) = (0, 0);
            bind(GL_TEXTURE_2D, tex);
            level_param(GL_TEXTURE_2D, 0, GL_TEXTURE_WIDTH, &mut w);
            level_param(GL_TEXTURE_2D, 0, GL_TEXTURE_HEIGHT, &mut h);
            if w <= 0 || h <= 0 {
                println!("{name} eye: mirror texture has no size");
                release(tex, handle);
                continue;
            }
            let mut pixels = vec![0u8; w as usize * h as usize * 4];
            let start = Instant::now();
            lock(handle);
            fb_tex(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
            read_pixels(0, 0, w, h, GL_RGBA, GL_UNSIGNED_BYTE, pixels.as_mut_ptr().cast());
            unlock(handle);
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let gl_err = get_error();
            release(tex, handle);
            let path = out_dir.join(format!("mirror-test-{name}.bmp"));
            write_bmp(&path, w as u32, h as u32, &pixels)?;
            println!("{name} eye: {w}x{h}, read in {ms:.1} ms (GL error {gl_err:#x}), saved {}", path.display());
        }
    }
    println!("Look at the pictures: are Steam overlays and the FPS overlay in them, and is the image undistorted?");
    Ok(())
}

/// 32-bit BMP; OpenGL rows are bottom-up, which is BMP's own order.
fn write_bmp(path: &PathBuf, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    let size = 54 + rgba.len() as u32;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(b"BM");
    for v in [size, 0, 54, 40] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    for v in [0u32, rgba.len() as u32, 2835, 2835, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    std::fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_a_bmp_with_the_right_size() {
        let path = std::env::temp_dir().join("sfrecorder-test.bmp");
        super::write_bmp(&path, 2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 54 + 8);
        assert_eq!(&data[54..58], &[0, 0, 255, 255]);
    }

    /// Needs a GPU or Mesa's software renderer: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn opens_an_opengl_context() {
        use super::*;
        unsafe {
            let (gl, how) = gl_context().unwrap();
            println!("{how}");
            // Read back a texture the same way the probe does.
            let gen_tex: unsafe extern "C" fn(i32, *mut u32) = gl.f(c"glGenTextures").unwrap();
            let bind: unsafe extern "C" fn(u32, u32) = gl.f(c"glBindTexture").unwrap();
            let tex_image: unsafe extern "C" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void) = gl.f(c"glTexImage2D").unwrap();
            let gen_fbs: unsafe extern "C" fn(i32, *mut u32) = gl.f(c"glGenFramebuffers").unwrap();
            let bind_fb: unsafe extern "C" fn(u32, u32) = gl.f(c"glBindFramebuffer").unwrap();
            let fb_tex: unsafe extern "C" fn(u32, u32, u32, u32, i32) = gl.f(c"glFramebufferTexture2D").unwrap();
            let read_pixels: unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, Ptr) = gl.f(c"glReadPixels").unwrap();
            let src = [10u8, 20, 30, 255, 40, 50, 60, 255];
            let (mut tex, mut fbo) = (0, 0);
            gen_tex(1, &mut tex);
            bind(GL_TEXTURE_2D, tex);
            tex_image(GL_TEXTURE_2D, 0, GL_RGBA as i32, 2, 1, 0, GL_RGBA, GL_UNSIGNED_BYTE, src.as_ptr().cast());
            gen_fbs(1, &mut fbo);
            bind_fb(GL_FRAMEBUFFER, fbo);
            fb_tex(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
            let mut out = [0u8; 8];
            read_pixels(0, 0, 2, 1, GL_RGBA, GL_UNSIGNED_BYTE, out.as_mut_ptr().cast());
            assert_eq!(out, src);
        }
    }
}
