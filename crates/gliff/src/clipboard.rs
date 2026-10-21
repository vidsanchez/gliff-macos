//! The network worker's half of the clipboard: joins the transfer engine to
//! the GTK thread, which owns `gdk::Clipboard`. Bytes cross between the two
//! threads through bounded channels, so the ack window applies end to end.
//!
//! Server to local: the UI requests bounded text prefetch before publishing a
//! native, in-memory GDK provider. No network work runs during a local paste.
//!
//! Local to server: the UI reports a new local selection as
//! [`ToWorker::LocalOffer`]. When the server requests a mime type the UI is
//! asked to read it ([`Status::ClipboardRead`]) into a channel that is served
//! to the engine; a file is served from disk here.

use std::cell::{Cell, RefCell};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::Sender as StdSender;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use gliff_proto::clipboard::resolve_mime;
use gliff_proto::{ClientMsg, ClipboardItem, ClipboardMsg};
use gliff_transport::clipboard::files::{open_source, LocalFiles};
use gliff_transport::clipboard::progress::Jobs;
use gliff_transport::clipboard::{Event, Transfers};

use crate::net::Status;

/// What the UI thread sends the worker.
pub enum ToWorker {
    Send(ClientMsg),
    /// Display pixels per stream pixel the decoder should write, so the
    /// picture is drawn 1:1 at an integer scale (see `paintable::layout`).
    Zoom(u32),
    /// The local clipboard changed to these forwardable mime types and files.
    LocalOffer {
        mime_types: Vec<String>,
        files: LocalFiles,
    },
    /// Prefetch one bounded UTF-8 snapshot; dropping result cancels it.
    FetchText {
        serial: u32,
        deadline: std::time::Instant,
        result: oneshot::Sender<crate::clipboard_text::TextResult>,
    },
    /// The user pressed cancel on a transfer reported as `id`.
    CancelTransfer(u32),
}

pub struct Bridge {
    transfers: Transfers,
    jobs: Jobs,
    status: StdSender<Status>,
    /// None until a local selection has been offered. Serial zero is a
    /// valid wire value, so it must not stand in for an absent offer.
    local_serial: Cell<Option<u32>>,
    local_mimes: RefCell<Vec<String>>,
    local_files: RefCell<LocalFiles>,
}

impl Bridge {
    pub fn new(transfers: Transfers, status: StdSender<Status>) -> Self {
        let report_to = status.clone();
        Self {
            transfers,
            jobs: Jobs::new(move |id, progress| {
                let _ = report_to.send(Status::ClipboardTransfer { id, progress });
            }),
            status,
            local_serial: Cell::new(None),
            local_mimes: RefCell::default(),
            local_files: RefCell::default(),
        }
    }

    pub fn on_peer_msg(&self, msg: ClipboardMsg, payload: Bytes) {
        match self.transfers.on_msg(msg, payload) {
            Some(Event::Offer {
                serial,
                mime_types,
                files,
            }) => {
                let _ = self.status.send(Status::ClipboardOffer {
                    serial,
                    mime_types,
                    files,
                });
            }
            Some(Event::Request { id, serial, item }) => self.on_request(id, serial, item),
            None => {}
        }
    }

    fn on_request(&self, id: u32, serial: u32, item: ClipboardItem) {
        if self.local_serial.get() != Some(serial) {
            tracing::debug!(id, serial, "clipboard request against a replaced offer");
            self.transfers.refuse(id);
            return;
        }
        match item {
            ClipboardItem::Mime(mime_type) => {
                // The UI reads the live clipboard, which may expose types
                // deliberately omitted from the offer (such as URI lists).
                if resolve_mime(&mime_type, &self.local_mimes.borrow()).is_none() {
                    self.transfers.refuse(id);
                    return;
                }
                let (reply, rx) = mpsc::channel::<io::Result<Bytes>>(2);
                if self
                    .status
                    .send(Status::ClipboardRead { mime_type, reply })
                    .is_err()
                {
                    self.transfers.refuse(id);
                    return;
                }
                self.transfers.serve(id, rx);
            }
            ClipboardItem::File(_) => {
                let Some(path) = self.local_files.borrow().path_for(&item).map(PathBuf::from)
                else {
                    self.transfers.refuse(id);
                    return;
                };
                let transfers = self.transfers.clone();
                tokio::task::spawn_local(async move {
                    match open_source(&path).await {
                        Ok(src) => transfers.serve(id, src),
                        Err(e) => {
                            tracing::warn!(id, path = %path.display(), error = %e, "clipboard file open failed");
                            transfers.refuse(id);
                        }
                    }
                });
            }
        }
    }

    /// A clipboard command from the UI; `Send` is handled by the caller.
    pub fn on_ui(&self, cmd: ToWorker) {
        match cmd {
            ToWorker::Send(_) | ToWorker::Zoom(_) => {}
            ToWorker::LocalOffer { mime_types, files } => {
                let serial = self.local_serial.get().unwrap_or(0).wrapping_add(1);
                self.local_serial.set(Some(serial));
                *self.local_mimes.borrow_mut() = mime_types.clone();
                let entries = files.entries.clone();
                *self.local_files.borrow_mut() = files;
                let transfers = self.transfers.clone();
                tokio::task::spawn_local(async move {
                    let _ = transfers
                        .send(ClipboardMsg::Offer {
                            serial,
                            mime_types,
                            files: entries,
                        })
                        .await;
                });
            }
            ToWorker::FetchText {
                serial,
                deadline,
                result,
            } => {
                tokio::task::spawn_local(crate::clipboard_text::fetch_text(
                    self.transfers.clone(),
                    serial,
                    deadline,
                    result,
                ));
            }
            ToWorker::CancelTransfer(id) => self.jobs.cancel(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_transport::clipboard::{outbound_channel, Side};

    #[test]
    fn local_read_error_aborts_without_marking_partial_bytes_successful() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut wire) = outbound_channel();
            let (status, statuses) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status);
            bridge.on_ui(ToWorker::LocalOffer {
                mime_types: vec!["text/plain;charset=utf-8".into()],
                files: LocalFiles::default(),
            });
            assert!(matches!(
                wire.recv().await.unwrap().0,
                ClipboardMsg::Offer { serial: 1, .. }
            ));
            bridge.on_peer_msg(
                ClipboardMsg::Request {
                    id: 2,
                    serial: 1,
                    item: ClipboardItem::Mime("text/plain;charset=utf-8".into()),
                },
                Bytes::new(),
            );
            let Status::ClipboardRead { reply, .. } = statuses.try_recv().unwrap() else {
                panic!("no local read")
            };
            reply
                .send(Ok(Bytes::from_static(b"synthetic partial")))
                .await
                .unwrap();
            reply
                .send(Err(io::Error::other("synthetic stream read failed")))
                .await
                .unwrap();
            drop(reply);
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    match wire.recv().await.unwrap().0 {
                        ClipboardMsg::Data { id: 2, done, .. } => assert!(
                            !done,
                            "must not publish partial bytes as a successful final chunk"
                        ),
                        ClipboardMsg::Abort { id: 2 } => break,
                        _ => panic!("unexpected transfer response"),
                    }
                }
            })
            .await
            .expect("read failure must abort promptly");
        });
    }

    #[test]
    fn a_request_before_any_local_offer_is_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            for (id, item) in [
                (2, ClipboardItem::Mime("text/plain".into())),
                (4, ClipboardItem::File(0)),
            ] {
                bridge.on_peer_msg(
                    ClipboardMsg::Request {
                        id,
                        serial: 0,
                        item,
                    },
                    Bytes::new(),
                );
                let refused = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                    .await
                    .expect("request without an offer was not refused")
                    .unwrap();
                assert!(matches!(refused.0, ClipboardMsg::Abort { id: got } if got == id));
                assert!(status_rx.try_recv().is_err());
            }
        });
    }

    #[test]
    fn requests_are_limited_to_the_current_offers_mime_types() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            for (serial, offered, requested, allowed) in [
                (1, "text/plain", "text/uri-list", false),
                (2, "image/png", "text/plain", false),
                (3, "text/plain", "UTF8_STRING", true),
                (4, "image/png", "image/png", true),
            ] {
                bridge.on_ui(ToWorker::LocalOffer {
                    mime_types: vec![offered.into()],
                    files: LocalFiles::default(),
                });
                let offer = rx.recv().await.unwrap();
                assert!(
                    matches!(offer.0, ClipboardMsg::Offer { serial: got, .. } if got == serial)
                );
                let id = serial * 2;
                bridge.on_peer_msg(
                    ClipboardMsg::Request {
                        id,
                        serial,
                        item: ClipboardItem::Mime(requested.into()),
                    },
                    Bytes::new(),
                );
                if allowed {
                    assert!(matches!(
                        status_rx.try_recv().unwrap(),
                        Status::ClipboardRead { mime_type, .. } if mime_type == requested
                    ));
                    let sent = rx.recv().await.unwrap();
                    assert!(matches!(
                        sent.0,
                        ClipboardMsg::Data { id: got, done: true, .. } if got == id
                    ));
                } else {
                    let refused =
                        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                            .await
                            .expect("unoffered mime type was not refused")
                            .unwrap();
                    assert!(matches!(refused.0, ClipboardMsg::Abort { id: got } if got == id));
                    assert!(status_rx.try_recv().is_err());
                }
            }
        });
    }

    #[test]
    fn pressure_does_not_drop_cancellation_or_sensitive_offer_withdrawal() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut wire) = outbound_channel();
            let (status, statuses) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status);
            bridge.on_ui(ToWorker::LocalOffer { mime_types:vec!["text/plain".into()], files:LocalFiles::default() });
            assert!(matches!(wire.recv().await.unwrap().0, ClipboardMsg::Offer { serial:1,.. }));
            let id = bridge.jobs.run("pending paste".into(), None, |_| std::future::pending());
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if matches!(statuses.try_recv(), Ok(Status::ClipboardTransfer { progress,.. }) if progress.state==gliff_transport::clipboard::progress::State::Running) { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            }).await.unwrap();
            let (tx, mut input) = crate::input_queue::channel();
            for _ in 0..crate::input_queue::NORMAL_LIMIT { tx.send(ToWorker::Zoom(1)).unwrap(); }
            tx.send(ToWorker::CancelTransfer(id)).unwrap();
            tx.send(ToWorker::LocalOffer { mime_types:vec![], files:LocalFiles::default() }).unwrap();
            drop(tx);
            while let Some(message) = input.recv().await { bridge.on_ui(message); }
            assert!(matches!(statuses.try_recv().unwrap(), Status::ClipboardTransfer { id:got, progress } if got==id && progress.state==gliff_transport::clipboard::progress::State::Cancelled));
            assert!(matches!(wire.recv().await.unwrap().0, ClipboardMsg::Offer { serial:2,mime_types,.. } if mime_types.is_empty()));
        });
    }
    #[test]
    fn a_request_against_a_replaced_offer_is_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (out, mut rx) = outbound_channel();
            let (status_tx, status_rx) = std::sync::mpsc::channel();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), status_tx);
            bridge.on_ui(ToWorker::LocalOffer {
                mime_types: vec!["text/plain".into()],
                files: LocalFiles::default(),
            });
            let offered = rx.recv().await.unwrap();
            assert!(matches!(offered.0, ClipboardMsg::Offer { serial: 1, .. }));
            bridge.on_peer_msg(
                ClipboardMsg::Request {
                    id: 2,
                    serial: 0,
                    item: ClipboardItem::Mime("text/plain".into()),
                },
                Bytes::new(),
            );
            let refused = rx.recv().await.unwrap();
            assert!(matches!(refused.0, ClipboardMsg::Abort { id: 2 }));
            assert!(status_rx.try_recv().is_err());
            bridge.on_peer_msg(
                ClipboardMsg::Request {
                    id: 4,
                    serial: 1,
                    item: ClipboardItem::Mime("text/plain".into()),
                },
                Bytes::new(),
            );
            assert!(matches!(
                status_rx.try_recv().unwrap(),
                Status::ClipboardRead { .. }
            ));
        });
    }
}
