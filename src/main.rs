mod clipboard;
mod geometry;
mod output;
mod wayland;

use std::path::PathBuf;

use output::{Pixels, encode_crop, save_atomic};

const HELP: &str = "snapcrop - crop a frozen Wayland screenshot

Usage: snapcrop [FILE]

Drag with the left mouse button to select a region. Right click or press Escape
to cancel. The selected region is copied to the clipboard and, when FILE is
provided, also saved there as a PNG.
";

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(error) if error == "selection cancelled" => 2,
        Err(error) => {
            eprintln!("snapcrop: {error}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let argument = arguments.next();
    if argument.as_deref() == Some(std::ffi::OsStr::new("--clipboard-helper")) {
        if arguments.next().is_some() {
            return Err("clipboard helper received unexpected arguments".into());
        }
        return clipboard::serve_from_stdin();
    }
    if argument.as_deref() == Some(std::ffi::OsStr::new("--help"))
        || argument.as_deref() == Some(std::ffi::OsStr::new("-h"))
    {
        print!("{HELP}");
        return Ok(());
    }
    if argument.as_deref() == Some(std::ffi::OsStr::new("--version"))
        || argument.as_deref() == Some(std::ffi::OsStr::new("-V"))
    {
        println!("snapcrop {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if arguments.next().is_some() {
        return Err(format!("too many arguments\n\n{HELP}"));
    }
    let path = argument.map(PathBuf::from);
    let captured = wayland::select_region()?;
    let png = encode_crop(
        Pixels {
            bytes: &captured.bytes,
            width: captured.width,
            height: captured.height,
            stride: captured.stride,
            format: captured.format,
        },
        captured.crop,
    )?;
    if let Some(path) = &path {
        save_atomic(path, &png)?;
    }
    clipboard::publish(&png).map_err(|error| match &path {
        Some(path) => format!(
            "saved {} but could not update clipboard: {error}",
            path.display()
        ),
        None => format!("could not update clipboard: {error}"),
    })?;
    Ok(())
}
