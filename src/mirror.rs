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

/// An OpenGL context with no window, which the mirror texture is shared into.
unsafe fn gl_context() -> Result<Gl> {
    let lib = libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_NOW);
    if lib.is_null() {
        bail!("no libEGL");
    }
    let sym = |n: &CStr| -> Result<Ptr> {
        let p = libc::dlsym(lib, n.as_ptr());
        if p.is_null() {
            bail!("libEGL has no {}", n.to_string_lossy());
        }
        Ok(p)
    };
    let get_proc: unsafe extern "C" fn(*const std::ffi::c_char) -> Ptr = std::mem::transmute(sym(c"eglGetProcAddress")?);
    let get_platform_display: unsafe extern "C" fn(u32, Ptr, *const isize) -> Ptr = std::mem::transmute(sym(c"eglGetPlatformDisplay")?);
    let get_display: unsafe extern "C" fn(Ptr) -> Ptr = std::mem::transmute(sym(c"eglGetDisplay")?);
    let initialize: unsafe extern "C" fn(Ptr, *mut i32, *mut i32) -> u32 = std::mem::transmute(sym(c"eglInitialize")?);
    let bind_api: unsafe extern "C" fn(u32) -> u32 = std::mem::transmute(sym(c"eglBindAPI")?);
    let choose_config: unsafe extern "C" fn(Ptr, *const i32, *mut Ptr, i32, *mut i32) -> u32 = std::mem::transmute(sym(c"eglChooseConfig")?);
    let create_context: unsafe extern "C" fn(Ptr, Ptr, Ptr, *const i32) -> Ptr = std::mem::transmute(sym(c"eglCreateContext")?);
    let make_current: unsafe extern "C" fn(Ptr, Ptr, Ptr, Ptr) -> u32 = std::mem::transmute(sym(c"eglMakeCurrent")?);

    let none = [EGL_NONE as isize];
    let mut dpy = get_platform_display(EGL_PLATFORM_SURFACELESS_MESA, std::ptr::null_mut(), none.as_ptr());
    let (mut major, mut minor) = (0, 0);
    if dpy.is_null() || initialize(dpy, &mut major, &mut minor) == 0 {
        dpy = get_display(std::ptr::null_mut());
        if dpy.is_null() || initialize(dpy, &mut major, &mut minor) == 0 {
            bail!("EGL won't start");
        }
    }
    if bind_api(EGL_OPENGL_API) == 0 {
        bail!("no desktop OpenGL");
    }
    let attrs = [EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE];
    let (mut config, mut n) = (std::ptr::null_mut(), 0);
    if choose_config(dpy, attrs.as_ptr(), &mut config, 1, &mut n) == 0 || n == 0 {
        bail!("no OpenGL config");
    }
    let ctx = create_context(dpy, config, std::ptr::null_mut(), [EGL_NONE].as_ptr());
    if ctx.is_null() || make_current(dpy, std::ptr::null_mut(), std::ptr::null_mut(), ctx) == 0 {
        bail!("can't make an OpenGL context current");
    }
    Ok(Gl { get_proc })
}

pub fn probe(out_dir: &Path) -> Result<()> {
    let vr = OpenVr::connect_as(openvr::APP_OVERLAY).context("connecting to SteamVR")?;
    let compositor = vr.table(openvr::COMPOSITOR).context("this SteamVR has no IVRCompositor_029")?;
    unsafe {
        let gl = gl_context()?;
        let bind: unsafe extern "C" fn(u32, u32) = gl.f(c"glBindTexture")?;
        let level_param: unsafe extern "C" fn(u32, i32, u32, *mut i32) = gl.f(c"glGetTexLevelParameteriv")?;
        let get_image: unsafe extern "C" fn(u32, i32, u32, u32, Ptr) = gl.f(c"glGetTexImage")?;
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
            get_image(GL_TEXTURE_2D, 0, GL_RGBA, GL_UNSIGNED_BYTE, pixels.as_mut_ptr().cast());
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
}
