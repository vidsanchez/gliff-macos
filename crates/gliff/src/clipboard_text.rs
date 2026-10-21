//! macOS publishes only fully materialized UTF-8 text. Never fetch from an
//! NSPasteboard fulfillment callback: GTK iterates a private GLib context there.
use std::{
    cell::RefCell,
    io,
    rc::Rc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use gliff_proto::{clipboard::TEXT_MIME, ClipboardItem};
use gliff_transport::clipboard::{ChunkSink, Transfers};
use tokio::sync::oneshot;

pub const TEXT_LIMIT: usize = 1024 * 1024;
pub const TEXT_DEADLINE: Duration = Duration::from_secs(5);
pub type TextResult = Result<String, &'static str>;

pub fn request_text(
    tx: &crate::input_queue::Sender,
    serial: u32,
) -> Result<oneshot::Receiver<TextResult>, &'static str> {
    let (result, reply) = oneshot::channel();
    tx.send(crate::clipboard::ToWorker::FetchText {
        serial,
        deadline: Instant::now() + TEXT_DEADLINE,
        result,
    })
    .map_err(|_| "Clipboard: connection queue unavailable")?;
    Ok(reply)
}

/// A second bound at the sink, in addition to the protocol assembler's cap.
#[derive(Clone, Default)]
struct TextBuffer(Rc<RefCell<Vec<u8>>>);
impl ChunkSink for TextBuffer {
    async fn write(&mut self, chunk: Bytes) -> io::Result<()> {
        let mut bytes = self.0.borrow_mut();
        if chunk.len() > TEXT_LIMIT.saturating_sub(bytes.len()) {
            return Err(io::Error::other("text exceeds 1 MiB"));
        }
        bytes.extend_from_slice(&chunk);
        Ok(())
    }
    async fn finish(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Dropping the UI receiver cancels the fetch, including while the reply is
/// still queued. Dropping Transfers::fetch uses its existing FetchGuard to
/// unregister the transfer and send protocol Abort. No new wire messages.
pub async fn fetch_text(
    transfers: Transfers,
    serial: u32,
    deadline: Instant,
    mut reply: oneshot::Sender<TextResult>,
) {
    let buffer = TextBuffer::default();
    let result = tokio::select! { biased;
        _ = reply.closed() => return,
        _ = tokio::time::sleep_until(deadline.into()) => Err("Clipboard: text transfer timed out (5 s)"),
        result = transfers.fetch(ClipboardItem::Mime(TEXT_MIME.into()), serial, buffer.clone(), Some(TEXT_LIMIT as u64)) => {
            match result {
                Ok(_) => String::from_utf8(std::mem::take(&mut *buffer.0.borrow_mut()))
                    .map_err(|_| "Clipboard: remote text is not valid UTF-8"),
                Err(_) => Err("Clipboard: text transfer failed or exceeds 1 MiB"),
            }
        }
    };
    let _ = reply.send(result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_proto::{clipboard::CHUNK, ClipboardMsg};
    use gliff_transport::clipboard::{outbound_channel, Side};

    fn run(future: impl std::future::Future<Output = ()>) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            tokio::time::timeout(Duration::from_secs(2), future)
                .await
                .expect("test hung");
        });
    }
    fn start(transfers: &Transfers, timeout: Duration) -> oneshot::Receiver<TextResult> {
        let (tx, rx) = oneshot::channel();
        tokio::task::spawn_local(fetch_text(
            transfers.clone(),
            17,
            Instant::now() + timeout,
            tx,
        ));
        rx
    }
    async fn request(wire: &mut tokio::sync::mpsc::Receiver<(ClipboardMsg, Bytes)>) -> u32 {
        let (message, _) = wire.recv().await.unwrap();
        let ClipboardMsg::Request { id, serial, item } = message else {
            panic!("not a request")
        };
        assert_eq!(serial, 17);
        assert_eq!(item, ClipboardItem::Mime(TEXT_MIME.into()));
        id
    }
    async fn body(
        transfers: &Transfers,
        wire: &mut tokio::sync::mpsc::Receiver<(ClipboardMsg, Bytes)>,
        id: u32,
        bytes: &[u8],
    ) {
        if bytes.is_empty() {
            transfers.on_msg(
                ClipboardMsg::Data {
                    id,
                    offset: 0,
                    data_len: 0,
                    done: true,
                },
                Bytes::new(),
            );
            return;
        }
        let chunks: Vec<_> = bytes.chunks(CHUNK).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let done = i + 1 == chunks.len();
            transfers.on_msg(
                ClipboardMsg::Data {
                    id,
                    offset: (i * CHUNK) as u64,
                    data_len: chunk.len() as u32,
                    done,
                },
                Bytes::copy_from_slice(chunk),
            );
            if !done {
                assert!(
                    matches!(wire.recv().await.unwrap().0, ClipboardMsg::Ack { id: got, .. } if got == id)
                );
            }
        }
    }
    #[test]
    fn prefetch_accepts_utf8_only_after_complete_transfer() {
        run(async {
            // The final case splits a multibyte character across wire chunks.
            for text in [
                String::new(),
                "synthetic ñ 🦀".into(),
                "x".repeat(CHUNK - 1) + "ñ 🦀",
            ] {
                let (out, mut wire) = outbound_channel();
                let transfers = Transfers::new(out, Side::Client);
                let result = start(&transfers, TEXT_DEADLINE);
                let id = request(&mut wire).await;
                body(&transfers, &mut wire, id, text.as_bytes()).await;
                assert_eq!(result.await.unwrap(), Ok(text));
            }
        });
    }
    #[test]
    fn prefetch_rejects_invalid_utf8_without_replacement_characters() {
        run(async {
            let (out, mut wire) = outbound_channel();
            let transfers = Transfers::new(out, Side::Client);
            let result = start(&transfers, TEXT_DEADLINE);
            let id = request(&mut wire).await;
            body(&transfers, &mut wire, id, &[0xff]).await;
            assert_eq!(
                result.await.unwrap(),
                Err("Clipboard: remote text is not valid UTF-8")
            );
        });
    }
    #[test]
    fn one_mib_is_accepted_but_one_extra_byte_is_rejected() {
        run(async {
            for length in [TEXT_LIMIT, TEXT_LIMIT + 1] {
                let (out, mut wire) = outbound_channel();
                let transfers = Transfers::new(out, Side::Client);
                let result = start(&transfers, TEXT_DEADLINE);
                let id = request(&mut wire).await;
                body(&transfers, &mut wire, id, &vec![b'x'; length]).await;
                let result = result.await.unwrap();
                if length == TEXT_LIMIT {
                    assert_eq!(result.unwrap().len(), TEXT_LIMIT);
                } else {
                    assert!(result.is_err());
                    assert!(
                        matches!(wire.recv().await.unwrap().0, ClipboardMsg::Abort { id: got } if got == id)
                    );
                }
            }
        });
    }
    #[test]
    fn silent_and_incomplete_peers_deadline_and_abort() {
        run(async {
            for incomplete in [false, true] {
                let (out, mut wire) = outbound_channel();
                let transfers = Transfers::new(out, Side::Client);
                let result = start(&transfers, Duration::from_millis(20));
                let id = request(&mut wire).await;
                if incomplete {
                    transfers.on_msg(
                        ClipboardMsg::Data {
                            id,
                            offset: 0,
                            data_len: 1,
                            done: false,
                        },
                        Bytes::from_static(b"x"),
                    );
                    assert!(matches!(
                        wire.recv().await.unwrap().0,
                        ClipboardMsg::Ack { .. }
                    ));
                }
                assert_eq!(
                    result.await.unwrap(),
                    Err("Clipboard: text transfer timed out (5 s)")
                );
                assert!(
                    matches!(wire.recv().await.unwrap().0, ClipboardMsg::Abort { id: got } if got == id)
                );
            }
        });
    }
    #[test]
    fn obsolete_receiver_cancels_in_flight_and_queued_requests() {
        run(async {
            let (out, mut wire) = outbound_channel();
            let transfers = Transfers::new(out, Side::Client);
            let result = start(&transfers, TEXT_DEADLINE);
            let id = request(&mut wire).await;
            drop(result);
            assert!(
                matches!(wire.recv().await.unwrap().0, ClipboardMsg::Abort { id: got } if got == id)
            );
            let (reply, result) = oneshot::channel();
            drop(result);
            fetch_text(transfers.clone(), 18, Instant::now() + TEXT_DEADLINE, reply).await;
            assert!(
                wire.try_recv().is_err(),
                "cancelled queued work must not send a request"
            );
            let (reply, result) = oneshot::channel();
            fetch_text(
                transfers,
                19,
                Instant::now() - Duration::from_millis(1),
                reply,
            )
            .await;
            assert_eq!(
                result.await.unwrap(),
                Err("Clipboard: text transfer timed out (5 s)")
            );
            assert!(
                wire.try_recv().is_err(),
                "queue time counts towards the deadline"
            );
        });
    }
    #[test]
    fn closed_and_exhausted_input_queues_fail_without_waiting() {
        let (tx, rx) = crate::input_queue::channel();
        drop(rx);
        assert!(request_text(&tx, 1).is_err());
        let (tx, _rx) = crate::input_queue::channel();
        for _ in 0..256 {
            tx.send(crate::clipboard::ToWorker::LocalOffer {
                mime_types: vec![],
                files: Default::default(),
            })
            .unwrap();
        }
        assert!(request_text(&tx, 2).is_err());
    }
}
