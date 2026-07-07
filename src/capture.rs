use std::collections::HashMap;
use std::os::fd::AsFd;

use image::imageops;
use image::RgbaImage;
use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};
use wayland_client::{
    backend::ObjectId,
    event_created_child,
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

pub struct CapturedThumb {
    pub kind: String,
    pub identifier: String,
    pub caption: String,
    pub app_id: String,
    pub haystack: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

struct ToplevelInfo {
    handle: ExtForeignToplevelHandleV1,
    identifier: Option<String>,
    title: Option<String>,
    app_id: Option<String>,
    closed: bool,
}

struct OutputInfo {
    output: WlOutput,
    name: Option<String>,
    model: Option<String>,
}

#[derive(Default)]
struct Capture {
    size: Option<(u32, u32)>,
    formats: Vec<u32>,
    constraints_done: bool,
    transform: Option<wl_output::Transform>,
    ready: bool,
    failed: bool,
    fail_reason: Option<String>,
}

#[derive(Default)]
struct State {
    shm: Option<WlShm>,
    copy_mgr: Option<ExtImageCopyCaptureManagerV1>,
    output_src_mgr: Option<ExtOutputImageCaptureSourceManagerV1>,
    toplevel_src_mgr: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,

    tl_order: Vec<ObjectId>,
    toplevels: HashMap<ObjectId, ToplevelInfo>,
    out_order: Vec<ObjectId>,
    outputs: HashMap<ObjectId, OutputInfo>,

    cap: Capture,
}

// ------------------------------------------------------------- registry -----

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        let bind_at = |iface_ver: u32, cap: u32| version.min(iface_ver).min(cap);

        if interface == ExtForeignToplevelListV1::interface().name {
            let v = bind_at(ExtForeignToplevelListV1::interface().version, u32::MAX);
            registry.bind::<ExtForeignToplevelListV1, _, _>(name, v, qh, ());
        } else if interface == WlOutput::interface().name {
            let v = bind_at(WlOutput::interface().version, 4);
            let output = registry.bind::<WlOutput, _, _>(name, v, qh, ());
            let id = output.id();
            state.out_order.push(id.clone());
            state.outputs.insert(id, OutputInfo { output, name: None, model: None });
        } else if interface == WlShm::interface().name {
            let v = bind_at(WlShm::interface().version, u32::MAX);
            state.shm = Some(registry.bind::<WlShm, _, _>(name, v, qh, ()));
        } else if interface == ExtImageCopyCaptureManagerV1::interface().name {
            let v = bind_at(ExtImageCopyCaptureManagerV1::interface().version, u32::MAX);
            state.copy_mgr = Some(registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(name, v, qh, ()));
        } else if interface == ExtOutputImageCaptureSourceManagerV1::interface().name {
            let v = bind_at(ExtOutputImageCaptureSourceManagerV1::interface().version, u32::MAX);
            state.output_src_mgr =
                Some(registry.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(name, v, qh, ()));
        } else if interface == ExtForeignToplevelImageCaptureSourceManagerV1::interface().name {
            let v = bind_at(ExtForeignToplevelImageCaptureSourceManagerV1::interface().version, u32::MAX);
            state.toplevel_src_mgr = Some(
                registry.bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(name, v, qh, ()),
            );
        }
    }
}

// --------------------------------------------------------- enumeration -----

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            let id = toplevel.id();
            state.tl_order.push(id.clone());
            state.toplevels.insert(
                id,
                ToplevelInfo {
                    handle: toplevel,
                    identifier: None,
                    title: None,
                    app_id: None,
                    closed: false,
                },
            );
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.toplevels.get_mut(&handle.id()) else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                info.identifier = Some(identifier);
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => info.title = Some(title),
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => info.app_id = Some(app_id),
            ext_foreign_toplevel_handle_v1::Event::Closed => info.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.outputs.get_mut(&output.id()) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => info.name = Some(name),
            wl_output::Event::Geometry { model, .. } => {
                let m = model.trim();
                if !m.is_empty() && !m.eq_ignore_ascii_case("unknown") {
                    info.model = Some(m.to_string());
                }
            }
            _ => {}
        }
    }
}

// -------------------------------------------------------- capture events -----

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.cap.size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                // wlroots advertises shm formats as DRM fourccs, which don't
                // map onto wl_shm's special XRGB/ARGB values, so decoding to
                // the enum loses them. Keep the raw number.
                let raw = match format {
                    WEnum::Value(v) => v.into(),
                    WEnum::Unknown(u) => u,
                };
                state.cap.formats.push(raw);
            }
            ext_image_copy_capture_session_v1::Event::Done => state.cap.constraints_done = true,
            ext_image_copy_capture_session_v1::Event::Stopped => state.cap.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Transform { transform } => {
                if let WEnum::Value(t) = transform {
                    state.cap.transform = Some(t);
                }
            }
            ext_image_copy_capture_frame_v1::Event::Ready => state.cap.ready = true,
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.cap.failed = true;
                state.cap.fail_reason = Some(format!("{reason:?}"));
            }
            _ => {}
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),+ $(,)?) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )+};
}
ignore_events!(
    WlShm,
    WlShmPool,
    WlBuffer,
    ExtImageCaptureSourceV1,
    ExtImageCopyCaptureManagerV1,
    ExtOutputImageCaptureSourceManagerV1,
    ExtForeignToplevelImageCaptureSourceManagerV1,
);

// --------------------------------------------------------------- helpers -----

fn debug_on() -> bool {
    std::env::var_os("SNIB_DEBUG").is_some()
}

fn scaled_dims(w: u32, h: u32, fit_height: bool, max: u32) -> (u32, u32) {
    let (wf, hf, maxf) = (w as f32, h as f32, max as f32);
    if fit_height && h > max {
        ((wf * maxf / hf).round() as u32, max)
    } else if !fit_height && w > max {
        (max, (hf * maxf / wf).round() as u32)
    } else {
        (w, h)
    }
}

/// wl_shm XRGB/ARGB8888 are little-endian B,G,R,A in memory. Swizzle to RGBA.
fn bgra_to_rgba(src: &[u8], opaque: bool) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    for (o, i) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        o[0] = i[2];
        o[1] = i[1];
        o[2] = i[0];
        o[3] = if opaque { 255 } else { i[3] };
    }
    out
}

fn deblock(state: &mut State, queue: &mut EventQueue<State>, done: fn(&Capture) -> bool) -> bool {
    for _ in 0..512 {
        if done(&state.cap) {
            return true;
        }
        if queue.blocking_dispatch(state).is_err() {
            return false;
        }   
    }
    false
}

fn capture_source(
    queue: &mut EventQueue<State>,
    state: &mut State,
    copy_mgr: &ExtImageCopyCaptureManagerV1,
    shm: &WlShm,
    source: &ExtImageCaptureSourceV1,
    max: u32,
    fit_height: bool,
) -> Option<(u32, u32, Vec<u8>)> {
    let qh = queue.handle();
    state.cap = Capture::default();

    let session = copy_mgr.create_session(source, Options::empty(), &qh, ());
    let got = deblock(state, queue, |c| c.constraints_done || c.failed);
    if debug_on() {
        eprintln!(
            "[snib]  session done={} failed={} size={:?} formats={:?}",
            state.cap.constraints_done, state.cap.failed, state.cap.size, state.cap.formats
        );
    }
    if !got || state.cap.failed {
        session.destroy();
        return None;
    }

    let (w, h) = state.cap.size?;
    // Prefer opaque XRGB, else ARGB. Both are BGRA byte order.
    // XRGB/ARGB8888 come through as either wl_shm's special values (1/0) or
    // the equivalent DRM fourccs ('XR24'/'AR24'), depending on the compositor.
    const XR24: u32 = 0x3432_5258;
    const AR24: u32 = 0x3432_5241;
    let has = |a: u32, b: u32| state.cap.formats.iter().any(|&f| f == a || f == b);
    let (format, opaque) = if has(u32::from(wl_shm::Format::Xrgb8888), XR24) {
        (wl_shm::Format::Xrgb8888, true)
    } else if has(u32::from(wl_shm::Format::Argb8888), AR24) {
        (wl_shm::Format::Argb8888, false)
    } else {
        (wl_shm::Format::Xrgb8888, true)
    };

    let stride = (w * 4) as usize;
    let size = stride * h as usize;

    let fd = memfd_create("snib", MemfdFlags::CLOEXEC).ok()?;
    ftruncate(&fd, size as u64).ok()?;
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            size,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &fd,
            0,
        )
    }
    .ok()? as *mut u8;

    let pool = shm.create_pool(fd.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &qh, ());

    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, w as i32, h as i32);
    frame.capture();

    let ok = deblock(state, queue, |c| c.ready || c.failed) && state.cap.ready;
    if debug_on() {
        eprintln!(
            "[snib]  frame ready={} failed={} reason={:?}",
            state.cap.ready, state.cap.failed, state.cap.fail_reason
        );
    }

    let result = if ok {
        let raw = unsafe { std::slice::from_raw_parts(ptr, size) };
        let rgba = bgra_to_rgba(raw, opaque);
        let mut img = RgbaImage::from_raw(w, h, rgba)?;
        img = apply_transform(img, state.cap.transform.unwrap_or(wl_output::Transform::Normal));
        let (iw, ih) = img.dimensions();
        let (tw, th) = scaled_dims(iw, ih, fit_height, max);
        let small = imageops::thumbnail(&img, tw.max(1), th.max(1));
        Some((tw.max(1), th.max(1), small.into_raw()))
    } else {
        None
    };

    frame.destroy();
    buffer.destroy();
    pool.destroy();
    session.destroy();
    unsafe {
        let _ = munmap(ptr as *mut _, size);
    }
    result
}

fn apply_transform(img: RgbaImage, t: wl_output::Transform) -> RgbaImage {
    use wl_output::Transform::*;
    match t {
        Normal => img,
        _90 | Flipped90 => imageops::rotate90(&img),
        _180 | Flipped180 => imageops::rotate180(&img),
        _270 | Flipped270 => imageops::rotate270(&img),
        Flipped => imageops::flip_horizontal(&img),
        _ => img,
    }
}

// ------------------------------------------------------------------ api -----

struct Job {
    kind: String,
    identifier: String,
    app_id: String,
    caption: String,
    haystack: String,
    source: ExtImageCaptureSourceV1,
}


pub fn capture_thumbnails(max: u32, fit_height: bool) -> Vec<CapturedThumb> {
    let Ok(conn) = Connection::connect_to_env() else {
        return Vec::new();
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::default();

    // #1 globals + binds, #2 toplevel handles + output props.
    if queue.roundtrip(&mut state).is_err() || queue.roundtrip(&mut state).is_err() {
        return Vec::new();
    }

    if debug_on() {
        eprintln!(
            "[snib] globals: shm={} copy_mgr={} out_mgr={} tl_mgr={}",
            state.shm.is_some(), state.copy_mgr.is_some(),
            state.output_src_mgr.is_some(), state.toplevel_src_mgr.is_some(),
        );
        let tl = state.toplevels.values().filter(|t| !t.closed && t.identifier.is_some()).count();
        let out = state.outputs.values().filter(|o| o.name.is_some()).count();
        eprintln!("[snib] enumerated {tl} toplevels, {out} outputs");
    }

    let (Some(copy_mgr), Some(shm)) = (state.copy_mgr.clone(), state.shm.clone()) else {
        return Vec::new();
    };

    let mut jobs: Vec<Job> = Vec::new();

    if let Some(mgr) = state.toplevel_src_mgr.clone() {
        for id in &state.tl_order {
            let info = &state.toplevels[id];
            if info.closed {
                continue;
            }
            let Some(identifier) = info.identifier.clone() else {
                continue;
            };
            let caption = info
                .title
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| info.app_id.clone())
                .unwrap_or_else(|| "(untitled)".to_string());
            let haystack = format!(
                "{} {}",
                info.title.as_deref().unwrap_or_default(),
                info.app_id.as_deref().unwrap_or_default()
            )
            .to_lowercase();
            let app_id = match &info.app_id {
                Some(m) => format!("{m}"),
                None => "".to_string(),
            };
            jobs.push(Job {
                kind: format!("Window"),
                identifier: identifier,
                app_id: app_id,
                caption,
                haystack,
                source: mgr.create_source(&info.handle, &qh, ()),
            });
        }
    }

    if let Some(mgr) = state.output_src_mgr.clone() {
        for id in &state.out_order {
            let info = &state.outputs[id];
            let Some(name) = info.name.clone() else {
                continue;
            };
            let caption = match &info.model {
                Some(m) => format!("{name} — {m}"),
                None => name.clone(),
            };
            jobs.push(Job {
                kind: format!("Monitor"),
                identifier: name,
                app_id: "".to_string(),
                haystack: caption.to_lowercase(),
                caption,
                source: mgr.create_source(&info.output, &qh, ()),
            });
        }
    }

    if debug_on() {
        eprintln!("[snib] built {} capture jobs", jobs.len());
    }

    let mut thumbs = Vec::new();
    for job in jobs {
        if let Some((w, h, rgba)) =
            capture_source(&mut queue, &mut state, &copy_mgr, &shm, &job.source, max, fit_height)
        {
            thumbs.push(CapturedThumb {
                kind: job.kind,
                identifier: job.identifier,
                app_id: job.app_id,
                caption: job.caption,
                haystack: job.haystack,
                width: w,
                height: h,
                rgba,
            });
        }
        job.source.destroy();
    }
    thumbs
}