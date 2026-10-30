//! The real backend over the `serialport` crate (docs/design.md, "Stack": blocking
//! reader on its own thread, forwarding over a channel).
//!
//! Threading: `open` starts two OS threads.
//!
//! - **Reader**: owns the port, blocks in `read` with `config.read_timeout`, checks
//!   the cancel flag between reads and forwards chunks as [`ReadEvent::Data`]. A
//!   device error ends the stream with [`ReadEvent::Error`]. This is the "close
//!   never hangs" guarantee: after a close request the thread is gone within one
//!   read timeout, and nothing ever joins it from the UI path.
//! - **Writer**: owns a `try_clone` of the port, drains its queue with
//!   `blocking_recv`, writing each chunk fully before the next.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::{OpenPort, PortConfig, PortInfo, PortWriter, ReadEvent, SerialBackend};
use crate::core::error::{CoreError, Result};

pub struct SerialPortBackend;

impl SerialPortBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SerialPortBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// How many queued write chunks the writer accepts before backpressure (the UI send
/// path uses `try_write`, so a wedged wire drops sends instead of freezing the UI).
const WRITE_QUEUE: usize = 64;

impl SerialBackend for SerialPortBackend {
    fn enumerate(&self) -> Result<Vec<PortInfo>> {
        let mut ports = Vec::new();
        for info in serialport::available_ports()? {
            let (vid, pid, description) = match &info.port_type {
                serialport::SerialPortType::UsbPort(usb) => {
                    (Some(usb.vid), Some(usb.pid), usb.product.clone())
                }
                _ => (None, None, None),
            };
            ports.push(PortInfo {
                name: info.port_name,
                vid,
                pid,
                description,
                is_mock: false,
            });
        }
        Ok(ports)
    }

    fn open(&self, name: &str, config: &PortConfig) -> Result<OpenPort> {
        tracing::info!(port = name, baud = config.baud_rate, "opening serial port");

        let port = serialport::new(name, config.baud_rate)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .timeout(config.read_timeout)
            .open()?;

        // Full duplex without lock contention: the writer gets its own handle to the
        // same port (serialport-rs `try_clone`).
        let writer_port = port
            .try_clone()
            .map_err(|error| CoreError::Io(std::io::Error::other(error.to_string())))?;

        let cancel = Arc::new(AtomicBool::new(false));

        let (event_tx, event_rx) = mpsc::channel::<ReadEvent>(256);
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);

        spawn_reader(port, event_tx.clone(), cancel.clone());
        spawn_writer(writer_port, write_rx, event_tx);

        Ok(OpenPort {
            events: event_rx,
            writer: PortWriter { tx: write_tx },
            cancel,
        })
    }
}

/// The read timeout granularity for the writer loopback check: a `try_clone`'d port
/// inherits the read timeout, but the writer never reads; this only bounds its shutdown.
const WRITER_TIMEOUT: Duration = Duration::from_millis(100);

fn spawn_reader(
    mut port: Box<dyn serialport::SerialPort>,
    events: mpsc::Sender<ReadEvent>,
    cancel: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("rcom-serial-reader".into())
        .spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                if cancel.load(Ordering::Relaxed) {
                    let _ = events.blocking_send(ReadEvent::Closed);
                    break;
                }
                match port.read(&mut buffer) {
                    Ok(0) => {} // spurious wake with nothing to read
                    Ok(n) => {
                        if events
                            .blocking_send(ReadEvent::Data(buffer[..n].to_vec()))
                            .is_err()
                        {
                            break; // session gone
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) => {}
                    Err(error) => {
                        let _ = events.blocking_send(ReadEvent::Error(error.to_string()));
                        break;
                    }
                }
            }
            tracing::debug!("serial reader stopped (within one read timeout of the close request)");
        })
        .expect("spawn rcom-serial-reader");
}

fn spawn_writer(
    mut port: Box<dyn serialport::SerialPort>,
    mut queue: mpsc::Receiver<Vec<u8>>,
    events: mpsc::Sender<ReadEvent>,
) {
    std::thread::Builder::new()
        .name("rcom-serial-writer".into())
        .spawn(move || {
            port.set_timeout(WRITER_TIMEOUT)
                .map_err(|error| tracing::warn!("writer could not set timeout: {error}"))
                .ok();
            while let Some(bytes) = queue.blocking_recv() {
                if let Err(error) = write_all(&mut port, &bytes) {
                    // A failed write means the port is gone for practical purposes
                    // (unplug, driver error): end the writer and kill the session
                    // through the shared event stream.
                    tracing::warn!("serial write failed: {error}");
                    let _ = events.blocking_send(ReadEvent::Error(format!("write failed: {error}")));
                    break;
                }
            }
            tracing::debug!("serial writer stopped");
        })
        .expect("spawn rcom-serial-writer");
}

fn write_all(port: &mut Box<dyn serialport::SerialPort>, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let n = port.write(bytes)?; // serialport returns io::Result
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "serial port accepted no bytes",
            ));
        }
        bytes = &bytes[n..];
    }
    Ok(())
}
