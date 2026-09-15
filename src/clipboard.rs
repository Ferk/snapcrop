use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

pub fn publish(bytes: &[u8]) -> Result<(), String> {
    let executable =
        std::env::current_exe().map_err(|e| format!("cannot locate snapcrop executable: {e}"))?;
    let mut child = Command::new(executable)
        .arg("--clipboard-helper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start clipboard helper: {e}"))?;

    child
        .stdin
        .take()
        .ok_or("clipboard helper has no stdin")?
        .write_all(bytes)
        .map_err(|e| format!("cannot send image to clipboard helper: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or("clipboard helper has no status channel")?;
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .map_err(|e| format!("cannot read clipboard helper status: {e}"))?;
    if line.trim() != "READY" {
        let status = child
            .wait()
            .map_err(|e| format!("clipboard helper failed: {e}"))?;
        return Err(format!(
            "clipboard helper exited before taking ownership ({status})"
        ));
    }
    Ok(())
}

pub fn serve_from_stdin() -> Result<(), String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read clipboard image: {e}"))?;
    if bytes.is_empty() {
        return Err("clipboard image is empty".into());
    }

    let connection = Connection::connect_to_env()
        .map_err(|e| format!("cannot connect clipboard to Wayland: {e}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut state = ClipboardState {
        manager: None,
        seat: None,
        bytes,
        running: true,
    };
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("clipboard registry dispatch failed: {e}"))?;

    let manager = state
        .manager
        .as_ref()
        .ok_or("compositor does not support zwlr_data_control_manager_v1")?;
    let seat = state
        .seat
        .as_ref()
        .ok_or("compositor does not advertise a wl_seat")?;
    let source = manager.create_data_source(&qh, ());
    source.offer("image/png".into());
    let device = manager.get_data_device(seat, &qh, ());
    device.set_selection(Some(&source));
    connection
        .flush()
        .map_err(|e| format!("cannot claim clipboard: {e}"))?;
    println!("READY");
    std::io::stdout()
        .flush()
        .map_err(|e| format!("cannot acknowledge clipboard ownership: {e}"))?;

    while state.running {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|e| format!("clipboard dispatch failed: {e}"))?;
    }
    Ok(())
}

struct ClipboardState {
    manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    bytes: Vec<u8>,
    running: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClipboardState {
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
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, version.min(9), qh, ()));
            }
            "zwlr_data_control_manager_v1" if state.manager.is_none() => {
                state.manager = Some(registry.bind(name, version.min(2), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        source: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                if mime_type == "image/png" {
                    let mut file = File::from(fd);
                    let _ = file.write_all(&state.bytes);
                    let _ = file.flush();
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                source.destroy();
                state.running = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        device: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_device_v1::Event::Finished = event {
            device.destroy();
            state.running = false;
        }
    }

    event_created_child!(
        ClipboardState,
        zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        [
            zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (
                zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
                ()
            ),
        ]
    );
}

delegate_noop!(ClipboardState: ignore wl_seat::WlSeat);
delegate_noop!(ClipboardState: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);
delegate_noop!(ClipboardState: ignore zwlr_data_control_offer_v1::ZwlrDataControlOfferV1);
