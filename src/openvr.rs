//! The few SteamVR calls SFRecorder needs, through OpenVR's C function tables.
//! libopenvr_api.so is loaded at run time so the recorder still runs (in Raw)
//! without it.

use std::ffi::{c_char, c_void, CStr, CString};

use anyhow::{bail, Context, Result};

// EVRApplicationType_VRApplication_Background: fails instead of starting
// SteamVR, which must only ever be started by Steam itself.
const APP_BACKGROUND: i32 = 3;
// Slots in the C function tables (openvr_capi.h). The first slots of
// IVRSystem are the same in every version we accept.
const SYSTEM_VERSIONS: [&str; 6] = ["IVRSystem_026", "IVRSystem_025", "IVRSystem_023", "IVRSystem_022", "IVRSystem_021", "IVRSystem_020"];
const SYSTEM_PROJECTION_RAW: usize = 2;
const SYSTEM_COMPUTE_DISTORTION: usize = 3;
const SYSTEM_EYE_TO_HEAD: usize = 4;
const SETTINGS: &str = "IVRSettings_003";
const SETTINGS_GET_INT32: usize = 6;
const HEADSET_VIEW: &str = "IVRHeadsetView_001";
const HEADSET_VIEW_GET_MODE: usize = 3;
pub const COMPOSITOR: &str = "IVRCompositor_029";
// EVRApplicationType_VRApplication_Overlay: what the mirror test connects as.
pub const APP_OVERLAY: i32 = 2;

pub const IDENTITY: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

type InitFn = unsafe extern "C" fn(*mut i32, i32) -> isize;
type GetInterfaceFn = unsafe extern "C" fn(*const c_char, *mut i32) -> isize;
type ShutdownFn = unsafe extern "C" fn();

/// One eye's view, as tangents of the half-angles (OpenVR's convention:
/// left and top are negative).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frustum {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

pub struct OpenVr {
    lib: *mut c_void,
    system: *const *const c_void,
    /// The IVRSystem version in use, for the C++ interface of the same version.
    system_version: &'static str,
    get_interface: GetInterfaceFn,
}

impl OpenVr {
    /// Connects to a SteamVR that is already running.
    pub fn connect() -> Result<Self> {
        Self::connect_as(APP_BACKGROUND)
    }

    /// Connects as the given application type (SteamVR must already run).
    pub fn connect_as(app_type: i32) -> Result<Self> {
        let lib = open_library().context("libopenvr_api.so not found, run the installer again")?;
        unsafe {
            let init: InitFn = symbol(lib, c"VR_InitInternal")?;
            let get_interface: GetInterfaceFn = symbol(lib, c"VR_GetGenericInterface")?;
            let mut err = 0;
            init(&mut err, app_type);
            if err != 0 {
                libc::dlclose(lib);
                bail!("SteamVR isn't running (OpenVR error {err})");
            }
            let mut vr = Self { lib, system: std::ptr::null(), system_version: "", get_interface };
            (vr.system, vr.system_version) = SYSTEM_VERSIONS
                .iter()
                .find_map(|v| Some((vr.table(v)?, *v)))
                .context("this SteamVR doesn't offer a known IVRSystem version")?;
            Ok(vr)
        }
    }

    pub fn table(&self, version: &str) -> Option<*const *const c_void> {
        let name = CString::new(format!("FnTable:{version}")).ok()?;
        let mut err = 0;
        let table = unsafe { (self.get_interface)(name.as_ptr(), &mut err) };
        (err == 0 && table != 0).then_some(table as *const *const c_void)
    }

    pub fn frustum(&self, eye: usize) -> Frustum {
        let (mut l, mut r, mut t, mut b) = (0f32, 0f32, 0f32, 0f32);
        unsafe {
            let f: unsafe extern "C" fn(i32, *mut f32, *mut f32, *mut f32, *mut f32) = slot(self.system, SYSTEM_PROJECTION_RAW);
            f(eye as i32, &mut l, &mut r, &mut t, &mut b);
        }
        Frustum { left: l as f64, right: r as f64, top: t as f64, bottom: b as f64 }
    }

    /// How the eye's rendered image is turned against the head (rows of a
    /// rotation matrix, head = R * eye). Canted panels show up here.
    ///
    /// This call returns a matrix by value, and on the Frame it crashed the
    /// process, so the recorder only makes it from a child process (see
    /// `eye_rotation_isolated`). `cpp` asks the C++ interface instead of the
    /// C function table.
    pub fn eye_rotation(&self, eye: usize, cpp: bool) -> Result<[[f64; 3]; 3]> {
        #[repr(C)]
        struct Matrix34 {
            m: [[f32; 4]; 3],
        }
        let m = unsafe {
            if cpp {
                let name = CString::new(self.system_version)?;
                let mut err = 0;
                let object = (self.get_interface)(name.as_ptr(), &mut err) as *mut c_void;
                if err != 0 || object.is_null() {
                    bail!("no C++ {} (OpenVR error {err})", self.system_version);
                }
                let vtable = *(object as *const *const *const c_void);
                let f: unsafe extern "C" fn(*mut c_void, i32) -> Matrix34 = slot(vtable, SYSTEM_EYE_TO_HEAD);
                f(object, eye as i32)
            } else {
                let f: unsafe extern "C" fn(i32) -> Matrix34 = slot(self.system, SYSTEM_EYE_TO_HEAD);
                f(eye as i32)
            }
        };
        let r = |i: usize| [m.m[i][0] as f64, m.m[i][1] as f64, m.m[i][2] as f64];
        let rot = [r(0), r(1), r(2)];
        if rot.iter().flatten().all(|v| v.is_finite()) && rot.iter().flatten().any(|v| *v != 0.0) {
            Ok(rot)
        } else {
            bail!("SteamVR gave no usable eye rotation ({rot:?})")
        }
    }

    /// For a point on one eye's panel (0..1 across that eye), the point of the
    /// eye's rendered image it shows, per colour channel (red, green, blue).
    pub fn distortion(&self, eye: usize, u: f32, v: f32) -> Option<[[f32; 2]; 3]> {
        let mut out = [[0f32; 2]; 3];
        let ok = unsafe {
            let f: unsafe extern "C" fn(i32, f32, f32, *mut [[f32; 2]; 3]) -> bool = slot(self.system, SYSTEM_COMPUTE_DISTORTION);
            f(eye as i32, u, v, &mut out)
        };
        ok.then_some(out)
    }

    /// The eye SteamVR's VR View shows (0 left, 1 right), if it says.
    pub fn view_eye(&self) -> Option<usize> {
        if let Some(t) = self.table(HEADSET_VIEW) {
            let mode = unsafe {
                let f: unsafe extern "C" fn() -> i32 = slot(t, HEADSET_VIEW_GET_MODE);
                f()
            };
            if mode == 0 || mode == 1 {
                return Some(mode as usize);
            }
        }
        let t = self.table(SETTINGS)?;
        let mut err = 0;
        let eye = unsafe {
            let f: unsafe extern "C" fn(*const c_char, *const c_char, *mut i32) -> i32 = slot(t, SETTINGS_GET_INT32);
            f(c"steamvr".as_ptr(), c"mirrorViewEye".as_ptr(), &mut err)
        };
        (err == 0 && (eye == 0 || eye == 1)).then_some(eye as usize)
    }
}

impl Drop for OpenVr {
    fn drop(&mut self) {
        unsafe {
            if let Ok(shutdown) = symbol::<ShutdownFn>(self.lib, c"VR_ShutdownInternal") {
                shutdown();
            }
            libc::dlclose(self.lib);
        }
    }
}

/// `eye_rotation` in a child process (`sfrecorder --eye-rotation EYE c|cpp`),
/// trying the C function table and then the C++ interface, so a crash in
/// SteamVR costs the levelling instead of the recording.
pub fn eye_rotation_isolated(eye: usize) -> Result<[[f64; 3]; 3]> {
    let exe = std::env::current_exe().context("finding the recorder's own program")?;
    let mut failures = Vec::new();
    for how in ["c", "cpp"] {
        match child_rotation(&exe, eye, how) {
            Ok(r) => return Ok(r),
            Err(e) => failures.push(format!("{how}: {e:#}")),
        }
    }
    bail!("{}", failures.join("; "))
}

fn child_rotation(exe: &std::path::Path, eye: usize, how: &str) -> Result<[[f64; 3]; 3]> {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;
    let mut child = std::process::Command::new(exe)
        .args(["--eye-rotation", &eye.to_string(), how])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("no answer in 5 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut out = String::new();
    child.stdout.take().context("no output")?.read_to_string(&mut out)?;
    if let Some(sig) = status.signal() {
        bail!("SteamVR crashed the call (signal {sig})");
    }
    if !status.success() {
        bail!("{}", out.trim());
    }
    parse_rotation(&out)
}

/// What `sfrecorder --eye-rotation` prints: nine numbers, row by row.
pub fn format_rotation(r: &[[f64; 3]; 3]) -> String {
    r.iter().flatten().map(|v| format!("{v:e}")).collect::<Vec<_>>().join(" ")
}

pub fn parse_rotation(s: &str) -> Result<[[f64; 3]; 3]> {
    let v: Vec<f64> = s.split_whitespace().map(str::parse).collect::<Result<_, _>>().context("unreadable eye rotation")?;
    if v.len() != 9 {
        bail!("unreadable eye rotation {s:?}");
    }
    Ok([[v[0], v[1], v[2]], [v[3], v[4], v[5]], [v[6], v[7], v[8]]])
}

fn open_library() -> Option<*mut c_void> {
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(std::path::PathBuf::from(home).join(".local/lib/sfrecorder/libopenvr_api.so"));
    }
    candidates.push("libopenvr_api.so".into());
    candidates.into_iter().find_map(|p| {
        let c = CString::new(p.into_os_string().into_encoded_bytes()).ok()?;
        let lib = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        (!lib.is_null()).then_some(lib)
    })
}

unsafe fn symbol<T: Copy>(lib: *mut c_void, name: &CStr) -> Result<T> {
    let p = libc::dlsym(lib, name.as_ptr());
    if p.is_null() {
        bail!("libopenvr_api.so has no {}", name.to_string_lossy());
    }
    Ok(std::mem::transmute_copy(&p))
}

/// # Safety
/// `table` must be a live OpenVR function table with at least `index + 1`
/// entries, and `T` the function type of that entry.
pub unsafe fn slot<T: Copy>(table: *const *const c_void, index: usize) -> T {
    std::mem::transmute_copy(&*table.add(index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_survives_the_trip_from_the_child() {
        let r = [[0.98, -0.17, 0.0], [0.17, 0.98, 1e-7], [0.0, 0.0, 1.0]];
        assert_eq!(parse_rotation(&format_rotation(&r)).unwrap(), r);
        assert!(parse_rotation("1 2 3").is_err());
    }
}
