//! Prioritised, single-writer stdout channel.
//!
//! All `HostMsg` frames funnel through one writer thread so frames are never
//! interleaved. Two queues are used: a *control* queue (handshake, pongs,
//! terminal diffs, telemetry) that is always drained first, and a bounded
//! *bulk* queue for file chunks. This keeps RTT probes and typing latency
//! flat even while a large download saturates the link, and the bounded queue
//! applies natural back-pressure to file readers.

use std::io::{BufWriter, Write};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, select, unbounded};
use pane_proto::{HostMsg, write_frame};

/// Number of 64 KiB chunks that may be queued before readers block (~2 MiB).
const BULK_QUEUE_DEPTH: usize = 32;

#[derive(Clone)]
pub struct Outbox {
    control: Sender<HostMsg>,
    bulk: Sender<HostMsg>,
    handle: std::sync::Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
}

impl Outbox {
    pub fn spawn<W: Write + Send + 'static>(sink: W) -> Self {
        let (control, control_rx) = unbounded();
        let (bulk, bulk_rx) = bounded(BULK_QUEUE_DEPTH);
        let handle = std::thread::Builder::new()
            .name("stdout-writer".into())
            .spawn(move || writer_loop(sink, control_rx, bulk_rx))
            .expect("spawn writer thread");
        Self { control, bulk, handle: std::sync::Arc::new(std::sync::Mutex::new(Some(handle))) }
    }

    /// Queue a latency-sensitive message.
    pub fn send(&self, msg: HostMsg) {
        let _ = self.control.send(msg);
    }

    /// Queue a bulk message, blocking while the bulk queue is full.
    /// Returns `false` if the writer has gone away (client disconnected).
    pub fn send_bulk(&self, msg: HostMsg) -> bool {
        self.bulk.send(msg).is_ok()
    }

    /// Flush everything queued so far and stop the writer thread.
    pub fn shutdown(self) {
        let handle = self.handle.lock().ok().and_then(|mut h| h.take());
        drop(self);
        if let Some(h) = handle {
            // Other clones (e.g. telemetry) may still hold senders; give the
            // writer a moment to drain, but never hang on exit.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while !h.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

fn writer_loop<W: Write>(sink: W, control: Receiver<HostMsg>, bulk: Receiver<HostMsg>) {
    let mut out = BufWriter::with_capacity(256 * 1024, sink);
    loop {
        let msg = match control.try_recv() {
            Ok(m) => m,
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {
                // Nothing urgent pending: flush, then wait on either queue.
                if out.flush().is_err() {
                    break;
                }
                select! {
                    recv(control) -> m => match m { Ok(m) => m, Err(_) => break },
                    recv(bulk) -> m => match m { Ok(m) => m, Err(_) => continue },
                }
            }
        };
        if let Err(e) = write_frame(&mut out, &msg) {
            log!("stdout write failed ({e}); client is gone, exiting");
            std::process::exit(0);
        }
    }
    let _ = out.flush();
}
