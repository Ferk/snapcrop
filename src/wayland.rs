use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsFd;

use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::geometry::{Point, Rect};
use crate::output::PixelFormat;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const KEY_ESC: u32 = 1;

pub struct CaptureResult {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub crop: Rect,
}

struct OutputInfo {
    proxy: wl_output::WlOutput,
    scale: i32,
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

#[derive(Clone, Copy)]
struct BufferId {
    overlay: usize,
    slot: usize,
}

#[derive(Clone, Copy)]
struct Drag {
    output: usize,
    start: Point,
    current: Point,
}

enum Outcome {
    Selecting,
    Accepted { output: usize, rect: Rect },
    Cancelled,
}

struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    outputs: Vec<OutputInfo>,
    captures: Vec<CaptureJob>,
    overlays: Vec<Overlay>,
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
            outputs: Vec::new(),
            captures: Vec::new(),
            overlays: Vec::new(),
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

    fn captures_complete(&self) -> bool {
        self.captures.len() == self.outputs.len()
            && self
                .captures
                .iter()
                .all(|capture| capture.bytes.is_some() || capture.failed.is_some())
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
        Ok(())
    }

    fn current_rect(&self, output: usize) -> Option<Rect> {
        let drag = self.drag.filter(|drag| drag.output == output)?;
        let overlay = self.overlays.get(output)?;
        Rect::from_points(
            drag.start,
            drag.current,
            overlay.logical_width,
            overlay.logical_height,
        )
    }

    fn render_overlay(&mut self, index: usize, qh: &QueueHandle<Self>) -> Result<(), String> {
        let (logical_width, logical_height) = {
            let overlay = self.overlays.get(index).ok_or("invalid overlay index")?;
            (overlay.logical_width, overlay.logical_height)
        };
        if logical_width == 0 || logical_height == 0 {
            return Ok(());
        }
        let crop = self.current_rect(index).map(|rect| {
            rect.to_buffer(
                logical_width,
                logical_height,
                self.captures[index].width,
                self.captures[index].height,
            )
        });
        let rendered = draw_dimmed(&self.captures[index], crop)?;

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
        Outcome::Accepted { output, rect } => {
            let capture = &mut state.captures[output];
            let crop = rect.to_buffer(
                state.overlays[output].logical_width,
                state.overlays[output].logical_height,
                capture.width,
                capture.height,
            );
            Ok(CaptureResult {
                bytes: capture.bytes.take().expect("completed capture has bytes"),
                width: capture.width,
                height: capture.height,
                stride: capture.stride,
                format: capture.format.expect("completed capture has format"),
                crop,
            })
        }
        Outcome::Cancelled | Outcome::Selecting => Err("selection cancelled".into()),
    }
}

fn make_file(size: u64) -> Result<File, String> {
    let fd = memfd_create("snapcrop", MemfdFlags::CLOEXEC)
        .map_err(|e| format!("cannot create shared-memory file: {e}"))?;
    let file = File::from(fd);
    file.set_len(size)
        .map_err(|e| format!("cannot size shared-memory file: {e}"))?;
    Ok(file)
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

fn draw_dimmed(capture: &CaptureJob, crop: Option<Rect>) -> Result<Vec<u8>, String> {
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
    Ok(result)
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
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
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
                state.outputs.push(OutputInfo { proxy, scale: 1 });
            }
            "zwlr_screencopy_manager_v1" if state.screencopy.is_none() => {
                state.screencopy = Some(registry.bind(name, version.min(3), qh, ()));
            }
            "zwlr_layer_shell_v1" if state.layer_shell.is_none() => {
                state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
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
        if let wl_output::Event::Scale { factor } = event
            && let Some(output) = state.outputs.get_mut(*index)
        {
            output.scale = factor.max(1);
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
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_focus = state
                    .overlays
                    .iter()
                    .position(|overlay| overlay.surface.id() == surface.id());
                state.pointer_position = Point {
                    x: surface_x.floor() as i32,
                    y: surface_y.floor() as i32,
                };
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_position = Point {
                    x: surface_x.floor() as i32,
                    y: surface_y.floor() as i32,
                };
                let Some(mut drag) = state.drag else { return };
                if state.pointer_focus == Some(drag.output) {
                    drag.current = state.pointer_position;
                    state.drag = Some(drag);
                    if let Err(error) = state.render_overlay(drag.output, qh) {
                        state.fail(error);
                    }
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
                    if let Some(output) = state.pointer_focus {
                        state.drag = Some(Drag {
                            output,
                            start: state.pointer_position,
                            current: state.pointer_position,
                        });
                    }
                } else if button == BTN_LEFT
                    && button_state == wl_pointer::ButtonState::Released
                    && let Some(drag) = state.drag
                    && let Some(rect) = state.current_rect(drag.output)
                {
                    state.outcome = Outcome::Accepted {
                        output: drag.output,
                        rect,
                    };
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
