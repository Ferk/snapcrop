use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsFd;

use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::geometry::{Point, Rect};
use crate::output::PixelFormat;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const KEY_ESC: u32 = 1;
const CURSOR_SIZE: u32 = 25;
const CURSOR_HOTSPOT: i32 = 12;
const BANNER_TEXT: &str = "DRAG TO SELECT - ESC OR RIGHT CLICK TO CANCEL";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum OutputTransform {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl OutputTransform {
    fn from_raw(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Normal),
            1 => Some(Self::Rotate90),
            2 => Some(Self::Rotate180),
            3 => Some(Self::Rotate270),
            4 => Some(Self::Flipped),
            5 => Some(Self::Flipped90),
            6 => Some(Self::Flipped180),
            7 => Some(Self::Flipped270),
            _ => None,
        }
    }

    fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::Flipped90 | Self::Flipped270
        )
    }

    fn map_pixel(self, x: u32, y: u32, width: u32, height: u32) -> (u32, u32) {
        match self {
            Self::Normal => (x, y),
            Self::Rotate90 => (y, width - 1 - x),
            Self::Rotate180 => (width - 1 - x, height - 1 - y),
            Self::Rotate270 => (height - 1 - y, x),
            Self::Flipped => (width - 1 - x, y),
            Self::Flipped90 => (y, x),
            Self::Flipped180 => (x, height - 1 - y),
            Self::Flipped270 => (height - 1 - y, width - 1 - x),
        }
    }
}

pub struct CaptureResult {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub crop: Rect,
}

struct OutputInfo {
    global_name: u32,
    proxy: wl_output::WlOutput,
    scale: i32,
    x: i32,
    y: i32,
    logical_width: u32,
    logical_height: u32,
    transform: OutputTransform,
    position_known: bool,
    removed: bool,
    _xdg_output: Option<zxdg_output_v1::ZxdgOutputV1>,
}

struct CaptureJob {
    _frame: zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    format: Option<PixelFormat>,
    shm_format: Option<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
    file: Option<File>,
    buffer: Option<wl_buffer::WlBuffer>,
    bytes: Option<Vec<u8>>,
    failed: Option<String>,
    y_invert: bool,
}

struct OverlayBuffer {
    file: File,
    buffer: wl_buffer::WlBuffer,
    busy: bool,
}

struct Overlay {
    surface: wl_surface::WlSurface,
    _layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    logical_width: u32,
    logical_height: u32,
    buffers: Vec<OverlayBuffer>,
    redraw_pending: bool,
}

struct Cursor {
    surface: wl_surface::WlSurface,
    _buffer: OverlayBuffer,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BannerPosition {
    Top,
    Bottom,
}

#[derive(Clone, Copy)]
struct BufferId {
    overlay: usize,
    slot: usize,
}

#[derive(Clone, Copy)]
struct Drag {
    start_global: Point,
    current_global: Point,
}

#[derive(Clone, Copy, Debug)]
struct GlobalRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

enum Outcome {
    Selecting,
    Accepted { rect: GlobalRect },
    Cancelled,
}

struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    xdg_output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: Vec<OutputInfo>,
    captures: Vec<CaptureJob>,
    overlays: Vec<Overlay>,
    cursor: Option<Cursor>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer_focus: Option<usize>,
    pointer_position: Point,
    drag: Option<Drag>,
    outcome: Outcome,
    setup_error: Option<String>,
}

impl State {
    fn new() -> Self {
        Self {
            compositor: None,
            shm: None,
            seat: None,
            screencopy: None,
            layer_shell: None,
            xdg_output_manager: None,
            outputs: Vec::new(),
            captures: Vec::new(),
            overlays: Vec::new(),
            cursor: None,
            pointer: None,
            keyboard: None,
            pointer_focus: None,
            pointer_position: Point::default(),
            drag: None,
            outcome: Outcome::Selecting,
            setup_error: None,
        }
    }

    fn begin_capture(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let manager = self
            .screencopy
            .as_ref()
            .ok_or("compositor does not support zwlr_screencopy_manager_v1")?;
        if self.outputs.is_empty() {
            return Err("compositor reported no outputs".into());
        }
        for (index, output) in self.outputs.iter().enumerate() {
            let frame = manager.capture_output(0, &output.proxy, qh, index);
            self.captures.push(CaptureJob {
                _frame: frame,
                format: None,
                shm_format: None,
                width: 0,
                height: 0,
                stride: 0,
                file: None,
                buffer: None,
                bytes: None,
                failed: None,
                y_invert: false,
            });
        }
        Ok(())
    }

    fn begin_output_info(&mut self, qh: &QueueHandle<Self>) {
        let Some(manager) = self.xdg_output_manager.clone() else {
            return;
        };
        for (index, output) in self.outputs.iter_mut().enumerate() {
            output._xdg_output = Some(manager.get_xdg_output(&output.proxy, qh, index));
        }
    }

    fn banner_position(&self, output: usize) -> Option<BannerPosition> {
        if self.drag.is_some() || self.pointer_focus != Some(output) {
            return None;
        }
        let height = self.overlays.get(output)?.logical_height;
        Some(if self.pointer_position.y >= height as i32 / 2 {
            BannerPosition::Top
        } else {
            BannerPosition::Bottom
        })
    }

    fn captures_complete(&self) -> bool {
        self.setup_error.is_some()
            || (self.captures.len() == self.outputs.len()
                && self
                    .captures
                    .iter()
                    .all(|capture| capture.bytes.is_some() || capture.failed.is_some()))
    }

    fn begin_overlays(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let compositor = self
            .compositor
            .as_ref()
            .ok_or("compositor does not advertise wl_compositor")?;
        let layer_shell = self
            .layer_shell
            .as_ref()
            .ok_or("compositor does not support zwlr_layer_shell_v1")?;
        for (index, output) in self.outputs.iter().enumerate() {
            let surface = compositor.create_surface(qh, index);
            if output.scale > 0 {
                surface.set_buffer_scale(output.scale);
            }
            let layer = layer_shell.get_layer_surface(
                &surface,
                Some(&output.proxy),
                zwlr_layer_shell_v1::Layer::Overlay,
                "snapcrop".into(),
                qh,
                index,
            );
            layer.set_anchor(
                zwlr_layer_surface_v1::Anchor::Top
                    | zwlr_layer_surface_v1::Anchor::Bottom
                    | zwlr_layer_surface_v1::Anchor::Left
                    | zwlr_layer_surface_v1::Anchor::Right,
            );
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(
                zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
            );
            surface.commit();
            self.overlays.push(Overlay {
                surface,
                _layer: layer,
                logical_width: 0,
                logical_height: 0,
                buffers: Vec::new(),
                redraw_pending: false,
            });
        }
        let cursor_surface = compositor.create_surface(qh, usize::MAX);
        let mut cursor_buffer = make_buffer(
            self.shm
                .as_ref()
                .ok_or("compositor does not advertise wl_shm")?,
            CURSOR_SIZE,
            CURSOR_SIZE,
            CURSOR_SIZE * 4,
            wl_shm::Format::Argb8888,
            qh,
            (),
        )?;
        cursor_buffer
            .file
            .write_all(&draw_crosshair_cursor())
            .map_err(|e| format!("cannot draw cursor: {e}"))?;
        cursor_surface.attach(Some(&cursor_buffer.buffer), 0, 0);
        cursor_surface.damage_buffer(0, 0, CURSOR_SIZE as i32, CURSOR_SIZE as i32);
        cursor_surface.commit();
        self.cursor = Some(Cursor {
            surface: cursor_surface,
            _buffer: cursor_buffer,
        });
        Ok(())
    }

    fn pointer_global(&self) -> Option<Point> {
        let output = self.outputs.get(self.pointer_focus?)?;
        Some(Point {
            x: output.x + self.pointer_position.x,
            y: output.y + self.pointer_position.y,
        })
    }

    fn current_global_rect(&self) -> Option<GlobalRect> {
        let drag = self.drag?;
        let x = drag.start_global.x.min(drag.current_global.x);
        let y = drag.start_global.y.min(drag.current_global.y);
        let width = drag.start_global.x.abs_diff(drag.current_global.x);
        let height = drag.start_global.y.abs_diff(drag.current_global.y);
        (width > 0 && height > 0).then_some(GlobalRect {
            x,
            y,
            width,
            height,
        })
    }

    fn selection_for_output(&self, index: usize) -> Option<Rect> {
        let selection = self.current_global_rect()?;
        let output = self.outputs.get(index)?;
        let intersection = intersect_global(
            selection,
            GlobalRect {
                x: output.x,
                y: output.y,
                width: output.logical_width,
                height: output.logical_height,
            },
        )?;
        Some(Rect {
            x: (intersection.x - output.x) as u32,
            y: (intersection.y - output.y) as u32,
            width: intersection.width,
            height: intersection.height,
        })
    }

    fn render_all_overlays(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        for index in 0..self.overlays.len() {
            self.render_overlay(index, qh)?;
        }
        Ok(())
    }

    fn render_overlay(&mut self, index: usize, qh: &QueueHandle<Self>) -> Result<(), String> {
        let (logical_width, logical_height) = {
            let overlay = self.overlays.get(index).ok_or("invalid overlay index")?;
            (overlay.logical_width, overlay.logical_height)
        };
        if logical_width == 0 || logical_height == 0 {
            return Ok(());
        }
        let crop = self.selection_for_output(index).map(|rect| {
            rect.to_buffer(
                logical_width,
                logical_height,
                self.captures[index].width,
                self.captures[index].height,
            )
        });
        let banner = self.banner_position(index);
        let display_scale = (self.captures[index].width / logical_width).max(1);
        let rendered = draw_dimmed(&self.captures[index], crop, banner, display_scale)?;

        let free_slot = self.overlays[index]
            .buffers
            .iter()
            .position(|buffer| !buffer.busy);
        let slot = if let Some(slot) = free_slot {
            slot
        } else if self.overlays[index].buffers.len() < 2 {
            let buffer = make_buffer(
                self.shm.as_ref().ok_or("wl_shm disappeared")?,
                self.captures[index].width,
                self.captures[index].height,
                self.captures[index].stride,
                self.captures[index]
                    .shm_format
                    .ok_or("capture has no shm format")?,
                qh,
                BufferId {
                    overlay: index,
                    slot: self.overlays[index].buffers.len(),
                },
            )?;
            self.overlays[index].buffers.push(buffer);
            self.overlays[index].buffers.len() - 1
        } else {
            self.overlays[index].redraw_pending = true;
            return Ok(());
        };

        let overlay = &mut self.overlays[index];
        let target = &mut overlay.buffers[slot];
        target
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("cannot seek overlay buffer: {e}"))?;
        target
            .file
            .write_all(&rendered)
            .map_err(|e| format!("cannot draw overlay: {e}"))?;
        overlay.surface.attach(Some(&target.buffer), 0, 0);
        overlay.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        overlay.surface.commit();
        target.busy = true;
        overlay.redraw_pending = false;
        Ok(())
    }

    fn fail(&mut self, message: impl Into<String>) {
        if self.setup_error.is_none() {
            self.setup_error = Some(message.into());
        }
        self.outcome = Outcome::Cancelled;
    }
}

pub fn select_region() -> Result<CaptureResult, String> {
    let connection =
        Connection::connect_to_env().map_err(|e| format!("cannot connect to Wayland: {e}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut state = State::new();

    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("Wayland registry dispatch failed: {e}"))?;
    state.begin_output_info(&qh);
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("Wayland output dispatch failed: {e}"))?;
    state.begin_capture(&qh)?;
    while !state.captures_complete() {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|e| format!("Wayland capture failed: {e}"))?;
    }
    if let Some(error) = state
        .captures
        .iter()
        .find_map(|capture| capture.failed.clone())
    {
        return Err(error);
    }
    if let Some(error) = state.setup_error.take() {
        return Err(error);
    }
    for (index, output) in state.outputs.iter_mut().enumerate() {
        let scale = output.scale.max(1) as u32;
        if output.logical_width == 0 {
            output.logical_width = state.captures[index].width / scale;
        }
        if output.logical_height == 0 {
            output.logical_height = state.captures[index].height / scale;
        }
    }
    if state.outputs.len() > 1 && state.outputs.iter().any(|output| !output.position_known) {
        return Err("compositor did not report logical positions for every output".into());
    }
    state.begin_overlays(&qh)?;
    while matches!(state.outcome, Outcome::Selecting) {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|e| format!("Wayland selection failed: {e}"))?;
        if let Some(error) = state.setup_error.take() {
            return Err(error);
        }
    }

    match state.outcome {
        Outcome::Accepted { rect } => compose_outputs(&state, rect),
        Outcome::Cancelled | Outcome::Selecting => Err("selection cancelled".into()),
    }
}

fn intersect_global(a: GlobalRect, b: GlobalRect) -> Option<GlobalRect> {
    let left = i64::from(a.x).max(i64::from(b.x));
    let top = i64::from(a.y).max(i64::from(b.y));
    let right = (i64::from(a.x) + i64::from(a.width)).min(i64::from(b.x) + i64::from(b.width));
    let bottom = (i64::from(a.y) + i64::from(a.height)).min(i64::from(b.y) + i64::from(b.height));
    (right > left && bottom > top).then_some(GlobalRect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

fn compose_outputs(state: &State, selection: GlobalRect) -> Result<CaptureResult, String> {
    let mut intersections = Vec::new();
    let mut target_scale = 1_u32;
    for (index, output) in state.outputs.iter().enumerate() {
        let bounds = GlobalRect {
            x: output.x,
            y: output.y,
            width: output.logical_width,
            height: output.logical_height,
        };
        if let Some(intersection) = intersect_global(selection, bounds) {
            let scale_x = state.captures[index]
                .width
                .div_ceil(output.logical_width.max(1));
            let scale_y = state.captures[index]
                .height
                .div_ceil(output.logical_height.max(1));
            target_scale = target_scale.max(scale_x).max(scale_y);
            intersections.push((index, intersection));
        }
    }
    if intersections.is_empty() {
        return Err("selection does not intersect an output".into());
    }

    let width = selection
        .width
        .checked_mul(target_scale)
        .ok_or("selected image is too wide")?;
    let height = selection
        .height
        .checked_mul(target_scale)
        .ok_or("selected image is too tall")?;
    let stride = width
        .checked_mul(4)
        .ok_or("selected image row is too wide")?;
    let length = usize::try_from(stride)
        .ok()
        .and_then(|stride| stride.checked_mul(height as usize))
        .ok_or("selected image is too large")?;
    let mut bytes = vec![0_u8; length];

    for (index, intersection) in intersections {
        let output = &state.outputs[index];
        let capture = &state.captures[index];
        let source = capture
            .bytes
            .as_ref()
            .ok_or("captured output pixels are unavailable")?;
        let local_x = (intersection.x - output.x) as u32;
        let local_y = (intersection.y - output.y) as u32;
        let destination_x = (intersection.x - selection.x) as u32 * target_scale;
        let destination_y = (intersection.y - selection.y) as u32 * target_scale;
        let destination_width = intersection.width * target_scale;
        let destination_height = intersection.height * target_scale;

        for dy in 0..destination_height {
            let source_y_numerator = (u64::from(local_y) * u64::from(target_scale)) + u64::from(dy);
            let source_y = (source_y_numerator * u64::from(capture.height)
                / (u64::from(output.logical_height) * u64::from(target_scale)))
                as u32;
            for dx in 0..destination_width {
                let source_x_numerator =
                    (u64::from(local_x) * u64::from(target_scale)) + u64::from(dx);
                let source_x = (source_x_numerator * u64::from(capture.width)
                    / (u64::from(output.logical_width) * u64::from(target_scale)))
                    as u32;
                let source_offset =
                    source_y as usize * capture.stride as usize + source_x as usize * 4;
                let target_offset = (destination_y + dy) as usize * stride as usize
                    + (destination_x + dx) as usize * 4;
                bytes[target_offset..target_offset + 3]
                    .copy_from_slice(&source[source_offset..source_offset + 3]);
                bytes[target_offset + 3] = 255;
            }
        }
    }

    Ok(CaptureResult {
        bytes,
        width,
        height,
        stride,
        format: PixelFormat::Argb8888,
        crop: Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
    })
}

fn make_file(size: u64) -> Result<File, String> {
    let fd = memfd_create("snapcrop", MemfdFlags::CLOEXEC)
        .map_err(|e| format!("cannot create shared-memory file: {e}"))?;
    let file = File::from(fd);
    file.set_len(size)
        .map_err(|e| format!("cannot size shared-memory file: {e}"))?;
    Ok(file)
}

fn transform_pixels(
    source: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    transform: OutputTransform,
) -> Result<(Vec<u8>, u32, u32, u32), String> {
    let source_len = usize::try_from(stride)
        .ok()
        .and_then(|stride| stride.checked_mul(height as usize))
        .ok_or("captured image is too large")?;
    if source.len() < source_len || stride < width.saturating_mul(4) {
        return Err("captured image has an invalid stride".into());
    }
    if transform == OutputTransform::Normal {
        return Ok((source.to_vec(), width, height, stride));
    }

    let (target_width, target_height) = if transform.swaps_axes() {
        (height, width)
    } else {
        (width, height)
    };
    let target_stride = target_width
        .checked_mul(4)
        .ok_or("transformed image is too wide")?;
    let target_len = usize::try_from(target_stride)
        .ok()
        .and_then(|stride| stride.checked_mul(target_height as usize))
        .ok_or("transformed image is too large")?;
    let mut target = vec![0_u8; target_len];
    for y in 0..height {
        for x in 0..width {
            let (target_x, target_y) = transform.map_pixel(x, y, width, height);
            let source_offset = y as usize * stride as usize + x as usize * 4;
            let target_offset = target_y as usize * target_stride as usize + target_x as usize * 4;
            target[target_offset..target_offset + 4]
                .copy_from_slice(&source[source_offset..source_offset + 4]);
        }
    }
    Ok((target, target_width, target_height, target_stride))
}

fn prepare_capture_buffer(
    state: &mut State,
    index: usize,
    frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    qh: &QueueHandle<State>,
) {
    let Some(job) = state.captures.get_mut(index) else {
        return;
    };
    if job.failed.is_some() || job.buffer.is_some() {
        return;
    }
    let Some(shm) = state.shm.as_ref() else {
        job.failed = Some("compositor does not advertise wl_shm".into());
        return;
    };
    let Some(size) = u64::from(job.stride).checked_mul(u64::from(job.height)) else {
        job.failed = Some("screenshot buffer is too large".into());
        return;
    };
    let Ok(pool_size) = i32::try_from(size) else {
        job.failed = Some("screenshot buffer exceeds Wayland limits".into());
        return;
    };
    let file = match make_file(size) {
        Ok(file) => file,
        Err(error) => {
            job.failed = Some(error);
            return;
        }
    };
    let pool = shm.create_pool(file.as_fd(), pool_size, qh, ());
    let buffer = pool.create_buffer(
        0,
        job.width as i32,
        job.height as i32,
        job.stride as i32,
        job.shm_format.expect("validated screenshot format"),
        qh,
        (),
    );
    pool.destroy();
    frame.copy(&buffer);
    job.file = Some(file);
    job.buffer = Some(buffer);
}

fn make_buffer<U: Send + Sync + 'static>(
    shm: &wl_shm::WlShm,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    qh: &QueueHandle<State>,
    data: U,
) -> Result<OverlayBuffer, String>
where
    State: Dispatch<wl_buffer::WlBuffer, U>,
{
    let size = u64::from(stride)
        .checked_mul(u64::from(height))
        .ok_or("shared-memory buffer is too large")?;
    let file = make_file(size)?;
    let pool = shm.create_pool(
        file.as_fd(),
        i32::try_from(size).map_err(|_| "shared-memory buffer exceeds Wayland limits")?,
        qh,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        i32::try_from(width).map_err(|_| "output width exceeds Wayland limits")?,
        i32::try_from(height).map_err(|_| "output height exceeds Wayland limits")?,
        i32::try_from(stride).map_err(|_| "output stride exceeds Wayland limits")?,
        format,
        qh,
        data,
    );
    pool.destroy();
    Ok(OverlayBuffer {
        file,
        buffer,
        busy: false,
    })
}

fn draw_dimmed(
    capture: &CaptureJob,
    crop: Option<Rect>,
    banner: Option<BannerPosition>,
    display_scale: u32,
) -> Result<Vec<u8>, String> {
    let source = capture
        .bytes
        .as_ref()
        .ok_or("capture pixels are unavailable")?;
    let mut result = source.clone();
    for row in result
        .chunks_exact_mut(capture.stride as usize)
        .take(capture.height as usize)
    {
        for pixel in row[..capture.width as usize * 4].as_chunks_mut::<4>().0 {
            pixel[0] /= 2;
            pixel[1] /= 2;
            pixel[2] /= 2;
            pixel[3] = 255;
        }
    }
    if let Some(rect) = crop {
        for y in rect.y..rect.y + rect.height {
            let start = y as usize * capture.stride as usize + rect.x as usize * 4;
            let end = start + rect.width as usize * 4;
            result[start..end].copy_from_slice(&source[start..end]);
        }
        draw_border(&mut result, capture.stride, rect);
    }
    if let Some(position) = banner {
        draw_banner(
            &mut result,
            capture.width,
            capture.height,
            capture.stride,
            position,
            display_scale,
        );
    }
    Ok(result)
}

fn draw_crosshair_cursor() -> Vec<u8> {
    let mut bytes = vec![0; (CURSOR_SIZE * CURSOR_SIZE * 4) as usize];
    let center = CURSOR_SIZE / 2;
    for y in 0..CURSOR_SIZE {
        for x in 0..CURSOR_SIZE {
            let distance = x.abs_diff(center).min(y.abs_diff(center));
            let on_axis = x == center || y == center;
            let outline = distance == 1 && (x.abs_diff(center) >= 4 || y.abs_diff(center) >= 4);
            let visible =
                (on_axis && (x.abs_diff(center) >= 3 || y.abs_diff(center) >= 3)) || outline;
            if visible {
                let offset = ((y * CURSOR_SIZE + x) * 4) as usize;
                let color = if on_axis { 255 } else { 24 };
                bytes[offset..offset + 4].copy_from_slice(&[color, color, color, 255]);
            }
        }
    }
    bytes
}

fn draw_banner(
    bytes: &mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    position: BannerPosition,
    display_scale: u32,
) {
    let margin = 12 * display_scale;
    let padding_x = 10 * display_scale;
    let padding_y = 7 * display_scale;
    let wide_scale = 2 * display_scale;
    let wide_text_width = BANNER_TEXT.len() as u32 * 6 * wide_scale - wide_scale;
    let font_scale = if wide_text_width + 2 * (margin + padding_x) <= width {
        wide_scale
    } else {
        display_scale
    };
    let text_width = BANNER_TEXT.len() as u32 * 6 * font_scale - font_scale;
    let banner_width = (text_width + 2 * padding_x).min(width.saturating_sub(2 * margin));
    let banner_height = 7 * font_scale + 2 * padding_y;
    if banner_width == 0 || banner_height + 2 * margin > height {
        return;
    }
    let left = (width - banner_width) / 2;
    let top = match position {
        BannerPosition::Top => margin,
        BannerPosition::Bottom => height - margin - banner_height,
    };

    for y in top..top + banner_height {
        for x in left..left + banner_width {
            let offset = y as usize * stride as usize + x as usize * 4;
            bytes[offset] = bytes[offset] * 2 / 5;
            bytes[offset + 1] = bytes[offset + 1] * 2 / 5;
            bytes[offset + 2] = bytes[offset + 2] * 2 / 5;
            bytes[offset + 3] = 255;
        }
    }

    let text_left = left + (banner_width.saturating_sub(text_width)) / 2;
    let text_top = top + padding_y;
    for (character_index, character) in BANNER_TEXT.chars().enumerate() {
        let glyph_left = text_left + character_index as u32 * 6 * font_scale;
        for (row, bits) in glyph(character).into_iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) == 0 {
                    continue;
                }
                for dy in 0..font_scale {
                    for dx in 0..font_scale {
                        let x = glyph_left + column * font_scale + dx;
                        let y = text_top + row as u32 * font_scale + dy;
                        if x < left + banner_width && y < height {
                            let offset = y as usize * stride as usize + x as usize * 4;
                            bytes[offset..offset + 4].copy_from_slice(&[238, 238, 238, 255]);
                        }
                    }
                }
            }
        }
    }
}

fn glyph(character: char) -> [u8; 7] {
    match character {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        _ => [0; 7],
    }
}

fn draw_border(bytes: &mut [u8], stride: u32, rect: Rect) {
    let thickness = 2_u32.min(rect.width).min(rect.height);
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            let edge = x < rect.x + thickness
                || x >= rect.x + rect.width - thickness
                || y < rect.y + thickness
                || y >= rect.y + rect.height - thickness;
            if edge {
                let offset = y as usize * stride as usize + x as usize * 4;
                bytes[offset..offset + 4].copy_from_slice(&[0xff, 0xb4, 0x00, 0xff]);
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let (name, interface, version) = match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => (name, interface, version),
            wl_registry::Event::GlobalRemove { name } => {
                let removed = if let Some(output) = state
                    .outputs
                    .iter_mut()
                    .find(|output| output.global_name == name)
                {
                    output.removed = true;
                    true
                } else {
                    false
                };
                if removed {
                    state.fail("an output was removed while snapcrop was active");
                }
                return;
            }
            _ => return,
        };
        match interface.as_str() {
            "wl_compositor" if state.compositor.is_none() => {
                state.compositor = Some(registry.bind(name, version.min(6), qh, ()));
            }
            "wl_shm" if state.shm.is_none() => {
                state.shm = Some(registry.bind(name, 1, qh, ()));
            }
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, version.min(9), qh, ()));
            }
            "wl_output" => {
                let index = state.outputs.len();
                let proxy = registry.bind(name, version.min(4), qh, index);
                state.outputs.push(OutputInfo {
                    global_name: name,
                    proxy,
                    scale: 1,
                    x: 0,
                    y: 0,
                    logical_width: 0,
                    logical_height: 0,
                    transform: OutputTransform::Normal,
                    position_known: false,
                    removed: false,
                    _xdg_output: None,
                });
            }
            "zwlr_screencopy_manager_v1" if state.screencopy.is_none() => {
                state.screencopy = Some(registry.bind(name, version.min(3), qh, ()));
            }
            "zwlr_layer_shell_v1" if state.layer_shell.is_none() => {
                state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "zxdg_output_manager_v1" if state.xdg_output_manager.is_none() => {
                state.xdg_output_manager = Some(registry.bind(name, version.min(3), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(*index) else {
            return;
        };
        let mut changed = false;
        match event {
            wl_output::Event::Scale { factor } => {
                let factor = factor.max(1);
                changed = output.scale != factor;
                output.scale = factor;
            }
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                let transform = match transform {
                    WEnum::Value(transform) => OutputTransform::from_raw(transform as u32),
                    WEnum::Unknown(_) => None,
                };
                let Some(transform) = transform else {
                    state.fail("compositor reported an unsupported output transform");
                    return;
                };
                changed = output.position_known
                    && (output.x != x || output.y != y || output.transform != transform);
                output.x = x;
                output.y = y;
                output.transform = transform;
                output.position_known = true;
            }
            _ => {}
        }
        if changed && !state.overlays.is_empty() {
            state.fail("output geometry changed while snapcrop was active");
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(*index) else {
            return;
        };
        let mut changed = false;
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                changed = output.position_known && (output.x != x || output.y != y);
                output.x = x;
                output.y = y;
                output.position_known = true;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                let width = width.max(0) as u32;
                let height = height.max(0) as u32;
                changed = output.logical_width != 0
                    && (output.logical_width != width || output.logical_height != height);
                output.logical_width = width;
                output.logical_height = height;
            }
            _ => {}
        }
        if changed && !state.overlays.is_empty() {
            state.fail("output geometry changed while snapcrop was active");
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let WEnum::Value(capabilities) = capabilities else {
                return;
            };
            if capabilities.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let transform = state
            .outputs
            .get(*index)
            .map(|output| output.transform)
            .unwrap_or_default();
        let Some(job) = state.captures.get_mut(*index) else {
            return;
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let WEnum::Value(format) = format else {
                    job.failed =
                        Some("compositor returned an unknown screenshot pixel format".into());
                    return;
                };
                job.format = match format {
                    wl_shm::Format::Xrgb8888 => Some(PixelFormat::Xrgb8888),
                    wl_shm::Format::Argb8888 => Some(PixelFormat::Argb8888),
                    _ => None,
                };
                if job.format.is_none() {
                    job.failed = Some(format!("unsupported screenshot pixel format: {format:?}"));
                }
                job.shm_format = Some(format);
                job.width = width;
                job.height = height;
                job.stride = stride;
                if frame.version() < 3 {
                    prepare_capture_buffer(state, *index, frame, qh);
                }
            }
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: WEnum::Value(flags),
            } => {
                job.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                prepare_capture_buffer(state, *index, frame, qh);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                let size = job.stride as usize * job.height as usize;
                let mut bytes = vec![0; size];
                let result = job
                    .file
                    .as_mut()
                    .ok_or("screenshot became ready without a buffer")
                    .and_then(|file| {
                        file.seek(SeekFrom::Start(0))
                            .map_err(|_| "cannot seek screenshot buffer")?;
                        file.read_exact(&mut bytes)
                            .map_err(|_| "cannot read screenshot buffer")
                    });
                if let Err(error) = result {
                    job.failed = Some(error.into());
                    return;
                }
                let transformed =
                    transform_pixels(&bytes, job.width, job.height, job.stride, transform);
                let (transformed_bytes, width, height, stride) = match transformed {
                    Ok(image) => image,
                    Err(error) => {
                        job.failed = Some(error);
                        return;
                    }
                };
                bytes = transformed_bytes;
                job.width = width;
                job.height = height;
                job.stride = stride;
                if job.y_invert {
                    let stride = job.stride as usize;
                    let mut flipped = vec![0; bytes.len()];
                    for y in 0..job.height as usize {
                        let source = y * stride;
                        let target = (job.height as usize - y - 1) * stride;
                        flipped[target..target + stride]
                            .copy_from_slice(&bytes[source..source + stride]);
                    }
                    bytes = flipped;
                }
                job.bytes = Some(bytes);
                frame.destroy();
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                job.failed = Some("the compositor refused the screenshot request".into());
                frame.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if let Some(overlay) = state.overlays.get_mut(*index) {
                    overlay.logical_width = width;
                    overlay.logical_height = height;
                }
                if let Some(output) = state.outputs.get_mut(*index) {
                    output.logical_width = width;
                    output.logical_height = height;
                }
                if let Err(error) = state.render_overlay(*index, qh) {
                    state.fail(error);
                }
            }
            zwlr_layer_surface_v1::Event::Closed => state.outcome = Outcome::Cancelled,
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                let previous_focus = state.pointer_focus;
                state.pointer_focus = state
                    .overlays
                    .iter()
                    .position(|overlay| overlay.surface.id() == surface.id());
                state.pointer_position = Point {
                    x: surface_x.floor() as i32,
                    y: surface_y.floor() as i32,
                };
                if let Some(cursor) = &state.cursor {
                    pointer.set_cursor(
                        serial,
                        Some(&cursor.surface),
                        CURSOR_HOTSPOT,
                        CURSOR_HOTSPOT,
                    );
                }
                if let Some(mut drag) = state.drag {
                    if let Some(global) = state.pointer_global() {
                        drag.current_global = global;
                        state.drag = Some(drag);
                    }
                    if let Err(error) = state.render_all_overlays(qh) {
                        state.fail(error);
                    }
                } else {
                    if let Some(previous) = previous_focus
                        && previous != state.pointer_focus.unwrap_or(usize::MAX)
                        && let Err(error) = state.render_overlay(previous, qh)
                    {
                        state.fail(error);
                    }
                    if let Some(output) = state.pointer_focus
                        && let Err(error) = state.render_overlay(output, qh)
                    {
                        state.fail(error);
                    }
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                let previous = state
                    .overlays
                    .iter()
                    .position(|overlay| overlay.surface.id() == surface.id());
                state.pointer_focus = None;
                if let Some(output) = previous
                    && let Err(error) = state.render_overlay(output, qh)
                {
                    state.fail(error);
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                let focused = state.pointer_focus;
                let old_banner = focused.and_then(|output| state.banner_position(output));
                state.pointer_position = Point {
                    x: surface_x.floor() as i32,
                    y: surface_y.floor() as i32,
                };
                if let Some(mut drag) = state.drag {
                    if let Some(global) = state.pointer_global() {
                        drag.current_global = global;
                        state.drag = Some(drag);
                    }
                    if let Err(error) = state.render_all_overlays(qh) {
                        state.fail(error);
                    }
                } else if let Some(output) = focused
                    && old_banner != state.banner_position(output)
                    && let Err(error) = state.render_overlay(output, qh)
                {
                    state.fail(error);
                }
            }
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => {
                let WEnum::Value(button_state) = button_state else {
                    return;
                };
                if button == BTN_RIGHT && button_state == wl_pointer::ButtonState::Pressed {
                    state.outcome = Outcome::Cancelled;
                } else if button == BTN_LEFT && button_state == wl_pointer::ButtonState::Pressed {
                    if let Some(global) = state.pointer_global() {
                        state.drag = Some(Drag {
                            start_global: global,
                            current_global: global,
                        });
                        if let Err(error) = state.render_all_overlays(qh) {
                            state.fail(error);
                        }
                    }
                } else if button == BTN_LEFT
                    && button_state == wl_pointer::ButtonState::Released
                    && state.drag.is_some()
                {
                    if let Some(rect) = state.current_global_rect() {
                        state.outcome = Outcome::Accepted { rect };
                    } else {
                        state.drag = None;
                        if let Err(error) = state.render_all_overlays(qh) {
                            state.fail(error);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key,
            state: key_state,
            ..
        } = event
            && key == KEY_ESC
            && key_state == WEnum::Value(wl_keyboard::KeyState::Pressed)
        {
            state.outcome = Outcome::Cancelled;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferId> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        id: &BufferId,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            let redraw = if let Some(overlay) = state.overlays.get_mut(id.overlay) {
                if let Some(buffer) = overlay.buffers.get_mut(id.slot) {
                    buffer.busy = false;
                }
                overlay.redraw_pending
            } else {
                false
            };
            if redraw && let Err(error) = state.render_overlay(id.overlay, qh) {
                state.fail(error);
            }
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);

impl Dispatch<wl_surface::WlSurface, usize> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersects_outputs_in_global_space() {
        let selection = GlobalRect {
            x: -50,
            y: 20,
            width: 200,
            height: 80,
        };
        let output = GlobalRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let intersection = intersect_global(selection, output).unwrap();
        assert_eq!(intersection.x, 0);
        assert_eq!(intersection.y, 20);
        assert_eq!(intersection.width, 100);
        assert_eq!(intersection.height, 80);
    }

    #[test]
    fn rejects_monitor_gap_as_an_intersection() {
        let selection = GlobalRect {
            x: 110,
            y: 10,
            width: 20,
            height: 20,
        };
        let output = GlobalRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(intersect_global(selection, output).is_none());
    }

    #[test]
    fn normalizes_every_output_transform() {
        let source: Vec<u8> = (*b"abcdef")
            .into_iter()
            .flat_map(|pixel| [pixel, 0, 0, 255])
            .collect();
        let cases = [
            (OutputTransform::Normal, b"abcdef".as_slice(), 2, 3),
            (OutputTransform::Rotate90, b"bdface".as_slice(), 3, 2),
            (OutputTransform::Rotate180, b"fedcba".as_slice(), 2, 3),
            (OutputTransform::Rotate270, b"ecafdb".as_slice(), 3, 2),
            (OutputTransform::Flipped, b"badcfe".as_slice(), 2, 3),
            (OutputTransform::Flipped90, b"acebdf".as_slice(), 3, 2),
            (OutputTransform::Flipped180, b"efcdab".as_slice(), 2, 3),
            (OutputTransform::Flipped270, b"fdbeca".as_slice(), 3, 2),
        ];

        for (transform, expected, width, height) in cases {
            let (pixels, transformed_width, transformed_height, stride) =
                transform_pixels(&source, 2, 3, 8, transform).unwrap();
            assert_eq!(
                (transformed_width, transformed_height, stride),
                (width, height, width * 4)
            );
            let actual: Vec<u8> = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .map(|pixel| pixel[0])
                .collect();
            assert_eq!(actual, expected, "{transform:?}");
        }
    }
}
