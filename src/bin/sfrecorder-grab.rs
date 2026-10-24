//! The panel helper: the only part of SFRecorder with a capability
//! (cap_sys_admin), which it needs to hand out the panel's scanout buffer.
//! See sfrecorder::grab.

fn main() {
    let card = std::env::args_os().nth(1).unwrap_or_else(|| "/dev/dri/card0".into());
    if let Err(e) = sfrecorder::grab::serve(std::path::Path::new(&card)) {
        eprintln!("sfrecorder-grab: {e:#}");
        std::process::exit(1);
    }
}
