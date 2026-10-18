//! Single instance: the first copy listens on a loopback port; later copies ask it to show its window.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

const ADDR: &str = "127.0.0.1:47613";

/// Returns the listener if this is the first instance. Otherwise asks the running
/// instance to show itself and returns `None`.
pub fn acquire() -> Option<Option<TcpListener>> {
    match TcpListener::bind(ADDR) {
        Ok(listener) => Some(Some(listener)),
        Err(_) => match TcpStream::connect(ADDR) {
            Ok(mut s) => {
                let _ = s.write_all(b"show");
                None
            }
            // The port belongs to something else; run without single-instance support.
            Err(_) => Some(None),
        },
    }
}

/// Calls `on_show` (from a background thread) whenever another instance starts.
pub fn serve(listener: TcpListener, on_show: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 4];
            if stream.read_exact(&mut buf).is_ok() && &buf == b"show" {
                on_show();
            }
        }
    });
}
