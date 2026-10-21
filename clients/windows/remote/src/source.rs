//! `PaneSource` over one `forgetty_client::PaneStream`.
//!
//! Three tokio tasks per pane: the reader (stream to `PaneEvent`), the input
//! writer (ordered, never blocks the UI) and the size sender (latest size
//! wins). Dropping or shutting down the source aborts them, which drops the
//! `PaneStream` and detaches the pane.

use std::sync::Mutex;

use forgetty_client::{ClientHandle, PaneItem, PaneStream};
use forgetty_shell_core::{PaneEvent, PaneSource};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use uuid::Uuid;

/// Output events buffered between the reader and the view. A full channel
/// back-pressures the QUIC stream (the client buffers `pane_buffer` items).
const EVENT_BUFFER: usize = 256;
/// Input bytes joined into one `send_input` call.
const INPUT_COALESCE: usize = 16 * 1024;

pub struct RemotePaneSource {
    events: async_channel::Receiver<PaneEvent>,
    input: mpsc::UnboundedSender<Vec<u8>>,
    size: watch::Sender<(u64, u16, u16)>,
    last_size: Mutex<Option<(u16, u16)>>,
    tasks: Vec<AbortHandle>,
}

impl RemotePaneSource {
    pub(crate) fn spawn(rt: &Handle, client: ClientHandle, pane: Uuid, mut stream: PaneStream) -> Self {
        let (ev_tx, ev_rx) = async_channel::bounded::<PaneEvent>(EVENT_BUFFER);
        let reader = rt.spawn(async move {
            loop {
                let ev = match stream.next().await {
                    Some(PaneItem::Reset { rows, cols }) => PaneEvent::Reset { rows, cols },
                    Some(PaneItem::Data { bytes, .. }) => PaneEvent::Output(bytes),
                    Some(PaneItem::Ended) => {
                        let _ = ev_tx.send(PaneEvent::Closed("pane ended".into())).await;
                        break;
                    }
                    None => {
                        let _ = ev_tx.send(PaneEvent::Closed("connection closed".into())).await;
                        break;
                    }
                };
                if ev_tx.send(ev).await.is_err() {
                    break; // the view is gone
                }
            }
        });

        let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let c = client.clone();
        let writer = rt.spawn(async move {
            while let Some(mut buf) = in_rx.recv().await {
                while buf.len() < INPUT_COALESCE {
                    match in_rx.try_recv() {
                        Ok(more) => buf.extend_from_slice(&more),
                        Err(_) => break,
                    }
                }
                // While disconnected the client buffers up to 64 KiB and sends
                // the bytes in order after the connection resumes.
                if let Err(e) = c.send_input(pane, &buf).await {
                    eprintln!("forgetty-win: remote input dropped ({} bytes): {e}", buf.len());
                }
            }
        });

        let (size_tx, mut size_rx) = watch::channel((0u64, 0u16, 0u16));
        let sizer = rt.spawn(async move {
            while size_rx.changed().await.is_ok() {
                let (_, rows, cols) = *size_rx.borrow_and_update();
                if let Err(e) = client.resize(pane, rows, cols).await {
                    eprintln!("forgetty-win: remote resize failed: {e}");
                }
            }
        });

        RemotePaneSource {
            events: ev_rx,
            input: in_tx,
            size: size_tx,
            last_size: Mutex::new(None),
            tasks: vec![reader.abort_handle(), writer.abort_handle(), sizer.abort_handle()],
        }
    }

    fn send_size(&self, rows: u16, cols: u16) {
        self.size.send_modify(|s| *s = (s.0 + 1, rows, cols));
    }
}

impl PaneSource for RemotePaneSource {
    fn events(&self) -> async_channel::Receiver<PaneEvent> {
        self.events.clone()
    }

    fn write_input(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.input.send(bytes.to_vec());
        }
    }

    fn resize(&self, rows: u16, cols: u16) {
        let mut last = self.last_size.lock().unwrap_or_else(|e| e.into_inner());
        if *last == Some((rows, cols)) {
            return;
        }
        *last = Some((rows, cols));
        drop(last);
        self.send_size(rows, cols);
    }

    fn reassert_size(&self, rows: u16, cols: u16) {
        *self.last_size.lock().unwrap_or_else(|e| e.into_inner()) = Some((rows, cols));
        self.send_size(rows, cols);
    }

    fn shutdown(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Drop for RemotePaneSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}
