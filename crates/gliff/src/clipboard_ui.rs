//! The GTK thread's half of the clipboard: watches the local `gdk::Clipboard`
//! for changes to offer to the server, reads a local item when the server
//! requests one. Remote UTF-8 text is fetched asynchronously BEFORE publishing
//! a native C provider: no network/future depends on NSPasteboard fulfillment.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use crate::input_queue::Sender as InputSender;
use bytes::Bytes;
use gtk::gdk;
use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use gliff_proto::clipboard::{
    forwardable_mimes, is_file_mime, offers_files, parse_uri_list, resolve_mime, CHUNK, TEXT_MIME,
};
use gliff_proto::ClipboardFile;
use gliff_transport::clipboard::files::{list_files, LocalFiles};
use gliff_transport::clipboard::progress::{describe, Progress, State};

use crate::clipboard::ToWorker;
use crate::clipboard_text::TEXT_DEADLINE;

/// Longest `text/uri-list` we read to build an offer.
const URI_LIST_MAX: usize = 1024 * 1024;

fn clipboard() -> Option<gdk::Clipboard> {
    gdk::Display::default().map(|d| d.clipboard())
}

fn sensitive_selection(mimes: &[String]) -> bool {
    mimes.iter().any(|m| m == "x-kde-passwordManagerHint")
}

// Provider identity, NOT bytes/hash, suppresses echoes. An epoch also gates
// results from replaced offers, machines, connections and local selections.
struct Publication<T> {
    generation: u64,
    provider: Option<T>,
}
impl<T> Default for Publication<T> {
    fn default() -> Self {
        Self {
            generation: 0,
            provider: None,
        }
    }
}
impl<T: PartialEq> Publication<T> {
    fn owns(&self, current: Option<&T>) -> bool {
        self.provider.as_ref().is_some_and(|p| Some(p) == current)
    }
    fn invalidate(&mut self, current: Option<&T>) -> bool {
        let clear = self.owns(current);
        self.generation = self.generation.wrapping_add(1);
        self.provider = None;
        clear
    }
    fn claim(&mut self, generation: u64, provider: T) -> bool {
        if generation != self.generation {
            return false;
        }
        self.provider = Some(provider);
        true
    }
}

#[derive(Default)]
pub struct RemoteClipboard {
    publication: RefCell<Publication<gdk::ContentProvider>>,
    pending: RefCell<Option<glib::JoinHandle<()>>>,
    writing: Cell<bool>,
}
impl RemoteClipboard {
    fn owns(&self, cb: &gdk::Clipboard) -> bool {
        self.writing.get() || self.publication.borrow().owns(cb.content().as_ref())
    }
    fn cancel_pending(&self) {
        if let Some(task) = self.pending.borrow_mut().take() {
            task.abort();
        }
    }
    fn local_changed(&self, cb: &gdk::Clipboard) {
        self.cancel_pending();
        self.publication
            .borrow_mut()
            .invalidate(cb.content().as_ref());
    }
    /// Only clear a selection still owned by us; never erase a newer local copy.
    pub fn clear(&self) {
        let cb = clipboard();
        let current = cb.as_ref().and_then(|c| c.content());
        self.clear_owned(current.as_ref(), |provider| {
            cb.as_ref().map_or(Ok(()), |cb| cb.set_content(provider))
        });
    }
    // Injectable publication boundary: tests use native providers and a memory
    // stream, never the real NSPasteboard. `current` is provider identity only.
    fn clear_owned(
        &self,
        current: Option<&gdk::ContentProvider>,
        publish: impl FnOnce(Option<&gdk::ContentProvider>) -> Result<(), glib::BoolError>,
    ) {
        self.cancel_pending();
        let clear = self.publication.borrow_mut().invalidate(current);
        if clear {
            // GTK macOS 4.24.1's send_to_pasteboard requires a NON-NULL
            // provider before reaching NSPasteboard.clearContents. None only
            // drops GDK ownership and can leave already-fulfilled native bytes.
            // Replace them with an empty built-in C provider instead. Keep its
            // identity so delayed changed signals/remote reads cannot echo it.
            let replacement = materialized_text_provider(String::new());
            let generation = self.publication.borrow().generation;
            self.publication
                .borrow_mut()
                .claim(generation, replacement.clone());
            self.writing.set(true);
            let result = publish(Some(&replacement));
            self.writing.set(false);
            if result.is_err() {
                // Preserve ownership for a later retry if the backend refused
                // the replacement. Never pretend that its old bytes vanished.
                if let Some(old) = current {
                    self.publication.borrow_mut().claim(generation, old.clone());
                }
                tracing::warn!("Could not replace stale remote clipboard selection");
            }
        }
    }
}
impl Drop for RemoteClipboard {
    fn drop(&mut self) {
        if let Some(task) = self.pending.get_mut().take() {
            task.abort();
        }
    }
}

fn materialized_text_provider(text: String) -> gdk::ContentProvider {
    // Built-in C implementation honors GTK's private thread-default context.
    // Do NOT replace this with a Rust subclass, even with a ready future.
    gdk::ContentProvider::for_bytes(TEXT_MIME, &glib::Bytes::from_owned(text.into_bytes()))
}

fn text_offer(mimes: &[String]) -> bool {
    !sensitive_selection(mimes)
        && mimes
            .iter()
            .any(|m| matches!(m.as_str(), TEXT_MIME | "text/plain" | "UTF8_STRING"))
}

async fn prepared_text(
    reply: oneshot::Receiver<crate::clipboard_text::TextResult>,
    timeout: Duration,
) -> crate::clipboard_text::TextResult {
    match glib::future_with_timeout(timeout, reply).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("Clipboard: connection closed before text arrived"),
        Err(_) => Err("Clipboard: text transfer timed out (5 s)"),
    }
}

/// Report every change of the local clipboard to the workers, except changes
/// made by our own proxy provider. Every running session gets the offer, so
/// any of them can paste it after a switch; nothing is sent until one does.
pub fn watch_local(remote: Rc<RemoteClipboard>, senders: impl Fn() -> Vec<InputSender> + 'static) {
    let Some(cb) = clipboard() else {
        return;
    };
    let senders = Rc::new(senders);
    // Bumped per change so a slow file listing for a stale one is dropped.
    let selection_gen = Rc::new(Cell::new(0u64));
    cb.connect_changed(move |cb| {
        // Only our own proxy for the server's offer must not echo back;
        // a copy from another gliff widget is a real local change.
        let gen = selection_gen.get() + 1;
        selection_gen.set(gen);
        if remote.owns(cb) {
            return;
        }
        remote.local_changed(cb);
        let txs = senders();
        if txs.is_empty() {
            return;
        }
        let selection_gen = selection_gen.clone();
        let mimes: Vec<String> = cb
            .formats()
            .mime_types()
            .iter()
            .map(|m| m.to_string())
            .collect();
        let cb = cb.clone();
        glib::MainContext::default().spawn_local(async move {
            let sensitive = sensitive_selection(&mimes);
            let files = if !sensitive && offers_files(&mimes) {
                local_files(&cb, &mimes).await
            } else {
                LocalFiles::default()
            };
            if selection_gen.get() != gen {
                return;
            }
            for tx in txs {
                let _ = tx.send(ToWorker::LocalOffer {
                    mime_types: if sensitive {
                        Vec::new()
                    } else {
                        forwardable_mimes(&mimes)
                    },
                    files: files.clone(),
                });
            }
        });
    });
}

/// List the files behind the local clipboard's file list, if readable. The
/// list is read as whichever file-list type the clipboard advertises; the
/// parser handles both bodies.
async fn local_files(cb: &gdk::Clipboard, mimes: &[String]) -> LocalFiles {
    let Some(list_mime) = mimes.iter().find(|m| is_file_mime(m)) else {
        return LocalFiles::default();
    };
    let Ok((stream, _)) = cb.read_future(&[list_mime], glib::Priority::DEFAULT).await else {
        return LocalFiles::default();
    };
    let mut body = Vec::new();
    while body.len() <= URI_LIST_MAX {
        match stream
            .read_bytes_future(64 * 1024, glib::Priority::DEFAULT)
            .await
        {
            Ok(b) if b.is_empty() => break,
            Ok(b) => body.extend_from_slice(&b),
            Err(_) => return LocalFiles::default(),
        }
    }
    if body.len() > URI_LIST_MAX {
        return LocalFiles::default();
    }
    let paths = parse_uri_list(&String::from_utf8_lossy(&body));
    if paths.is_empty() {
        return LocalFiles::default();
    }
    // The walk reads metadata for up to 10,000 entries, so it runs on its
    // own thread; blocking here would freeze rendering and input.
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(list_files(&paths));
    });
    match rx.await {
        Ok(Ok(files)) => files,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "clipboard files not offered");
            LocalFiles::default()
        }
        Err(_) => LocalFiles::default(),
    }
}

/// Stream the local clipboard's `mime_type` into `reply` for the server;
/// dropping `reply` marks the end. An error is sent as such. The server's
/// own offer is refused: reading it would fetch the item back from the
/// server, which would ask us for it again.
pub fn read_local(
    remote: Rc<RemoteClipboard>,
    mime_type: String,
    reply: Sender<std::io::Result<Bytes>>,
) {
    glib::MainContext::default().spawn_local(async move {
        let Some(cb) = clipboard() else {
            return;
        };
        if remote.owns(&cb) {
            let error = std::io::Error::other("clipboard holds the server's offer");
            let _ = reply.send(Err(error)).await;
            return;
        }
        // A text request may name a flavour the local source does not
        // advertise; any text flavour it does have will do.
        let available: Vec<String> = cb
            .formats()
            .mime_types()
            .iter()
            .map(|m| m.to_string())
            .collect();
        if sensitive_selection(&available) {
            let _ = reply
                .send(Err(std::io::Error::other(
                    "clipboard selection is marked sensitive",
                )))
                .await;
            return;
        }
        let Some(actual) = resolve_mime(&mime_type, &available) else {
            let error = std::io::Error::other(format!("clipboard has no {mime_type}"));
            let _ = reply.send(Err(error)).await;
            return;
        };
        let is_macos = gdk::Display::default().is_some_and(|d| d.backend().is_macos());
        let is_local = cb.is_local();
        let (stream, real_mime) = match cb.read_future(&[actual], glib::Priority::DEFAULT).await {
            Ok(value) => value,
            Err(e) => {
                let _ = reply.send(Err(std::io::Error::other(e.to_string()))).await;
                return;
            }
        };
        forward_local_stream(
            &stream,
            reply,
            native_utf8_terminated(is_macos, is_local, &real_mime),
        )
        .await;
    });
}

async fn forward_local_stream(
    stream: &gio::InputStream,
    reply: Sender<std::io::Result<Bytes>>,
    strip_final_c_nul: bool,
) {
    let mut pending: Option<Bytes> = None;
    loop {
        match stream
            .read_bytes_future(CHUNK, glib::Priority::DEFAULT)
            .await
        {
            Ok(b) if b.is_empty() => {
                if let Some(mut last) = pending.take() {
                    // Exactly one C terminator, not whitespace/interior NULs.
                    if last.last() == Some(&0) {
                        last.truncate(last.len() - 1);
                    }
                    if !last.is_empty() {
                        let _ = reply.send(Ok(last)).await;
                    }
                }
                break;
            }
            Ok(b) => {
                let bytes = Bytes::copy_from_slice(&b);
                if strip_final_c_nul {
                    // One bounded chunk of look-behind handles a terminator
                    // arriving alone in the next read without buffering text.
                    if let Some(previous) = pending.replace(bytes) {
                        if reply.send(Ok(previous)).await.is_err() {
                            break;
                        }
                    }
                } else if reply.send(Ok(bytes)).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = reply.send(Err(std::io::Error::other(e.to_string()))).await;
                break;
            }
        }
    }
    let _ = stream.close_future(glib::Priority::DEFAULT).await;
}

fn native_utf8_terminated(is_macos: bool, is_local: bool, real_mime: &str) -> bool {
    // GTK 4.24.1 gdkmacospasteboard.c:165-174 creates ONLY this external
    // NSString stream with strlen(str) + 1. All other streams are byte-exact.
    is_macos && !is_local && real_mime == TEXT_MIME
}

/// One asynchronous, bounded snapshot for the active connection. The global
/// context is safe here because this is NOT a pasteboard fulfillment callback.
/// Native pasteboard requests only see the final C in-memory provider.
pub fn set_remote_offer(
    remote: &Rc<RemoteClipboard>,
    tx: &InputSender,
    serial: u32,
    mime_types: Vec<String>,
    files: Vec<ClipboardFile>,
    report: impl Fn(&'static str) + 'static,
) {
    remote.clear();
    if !text_offer(&mime_types) {
        if sensitive_selection(&mime_types) {
            report("Clipboard: sensitive remote selection ignored");
        } else if !mime_types.is_empty() || !files.is_empty() {
            report("Clipboard: Linux→Mac currently supports UTF-8 text only");
        }
        return;
    }
    let generation = remote.publication.borrow().generation;
    let reply = match crate::clipboard_text::request_text(tx, serial) {
        Ok(reply) => reply,
        Err(error) => {
            report(error);
            return;
        }
    };
    if !files.is_empty() || mime_types.iter().any(|m| m.starts_with("image/")) {
        report("Clipboard: preparing text only; files/images are not imported");
    } else {
        report("Clipboard: preparing remote text…");
    }
    let weak = Rc::downgrade(remote);
    let sender = tx.downgrade();
    let task = glib::MainContext::default().spawn_local(async move {
        // Also bounds a fetch still sitting in a stalled/closed worker queue.
        // Abort of this task drops reply, cancelling the protocol fetch.
        let result = prepared_text(reply, TEXT_DEADLINE).await;
        let Some(remote) = weak.upgrade() else {
            return;
        };
        if remote.publication.borrow().generation != generation
            || sender.upgrade().is_none_or(|s| s.is_closed())
        {
            return;
        }
        let text = match result {
            Ok(text) => text,
            Err(error) => {
                report(error);
                return;
            }
        };
        let Some(cb) = clipboard() else {
            return;
        };
        let provider = materialized_text_provider(text);
        if !remote
            .publication
            .borrow_mut()
            .claim(generation, provider.clone())
        {
            return;
        }
        // Claim BEFORE set_content: changed can fire synchronously/reentrantly.
        remote.writing.set(true);
        let result = cb.set_content(Some(&provider));
        remote.writing.set(false);
        if result.is_err() {
            remote
                .publication
                .borrow_mut()
                .invalidate(cb.content().as_ref());
            report("Clipboard: could not publish remote text");
        } else {
            report("Clipboard: remote text ready");
        }
    });
    *remote.pending.borrow_mut() = Some(task);
}

#[cfg(test)]
mod private_context_tests {
    use super::*;

    fn stream_bytes(bytes: &[u8], strip_final_c_nul: bool) -> std::io::Result<Vec<u8>> {
        let context = glib::MainContext::new();
        let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(bytes));
        context.block_on(async {
            let (reply, mut receiver) = tokio::sync::mpsc::channel(2);
            let pump = context.spawn_local(async move {
                forward_local_stream(stream.upcast_ref(), reply, strip_final_c_nul).await;
            });
            let mut bytes = Vec::new();
            while let Some(chunk) = receiver.recv().await {
                bytes.extend_from_slice(&chunk?);
            }
            pump.await.unwrap();
            Ok(bytes)
        })
    }

    #[test]
    fn external_macos_utf8_drops_only_the_backend_final_c_terminator() {
        // GTK 4.24.1's external UTF-8 read uses strlen(str) + 1.
        assert_eq!(stream_bytes(b"propio\0", true).unwrap(), b"propio");
        assert_eq!(
            stream_bytes("ñ 🦀\0".as_bytes(), true).unwrap(),
            "ñ 🦀".as_bytes()
        );
        assert_eq!(stream_bytes(b"\0", true).unwrap(), b"");
        assert_eq!(stream_bytes(b"", true).unwrap(), b"");
        assert_eq!(stream_bytes(b"a\0b\0\0", true).unwrap(), b"a\0b\0");
        assert_eq!(
            stream_bytes(b"with space \n\0", true).unwrap(),
            b"with space \n"
        );
        assert_eq!(
            stream_bytes(b"no terminator", true).unwrap(),
            b"no terminator"
        );
    }

    #[test]
    fn terminator_fix_handles_read_boundaries_and_leaves_other_sources_byte_exact() {
        for length in [CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK] {
            let mut bytes = vec![b'x'; length];
            bytes.push(0);
            assert_eq!(stream_bytes(&bytes, true).unwrap(), &bytes[..length]);
            assert_eq!(stream_bytes(&bytes, false).unwrap(), bytes);
        }
        let bytes = b"synthetic\0binary\0 \n\0";
        for (macos, local, mime) in [
            (true, true, TEXT_MIME),
            (false, false, TEXT_MIME),
            (true, false, "application/octet-stream"),
            (true, false, "image/png"),
            (true, false, "text/plain"),
            (true, false, "UTF8_STRING"),
        ] {
            let strip = native_utf8_terminated(macos, local, mime);
            assert!(!strip);
            assert_eq!(stream_bytes(bytes, strip).unwrap(), bytes);
        }
        assert!(native_utf8_terminated(true, false, TEXT_MIME));
    }

    #[test]
    fn local_stream_errors_are_errors_and_cancelled_receivers_close_the_stream() {
        let context = glib::MainContext::new();
        let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(b"synthetic\0"));
        stream.close(gio::Cancellable::NONE).unwrap();
        context.block_on(async {
            let (reply, mut receiver) = tokio::sync::mpsc::channel(2);
            forward_local_stream(stream.upcast_ref(), reply, true).await;
            assert!(receiver.recv().await.unwrap().is_err());
            assert!(receiver.recv().await.is_none());
        });
        let stream =
            gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(vec![b'x'; 3 * CHUNK]));
        context.block_on(async {
            let (reply, receiver) = tokio::sync::mpsc::channel(2);
            drop(receiver);
            forward_local_stream(stream.upcast_ref(), reply, true).await;
        });
        assert!(stream.is_closed());
    }

    fn native_bytes(provider: &gdk::ContentProvider) -> Vec<u8> {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let stream = gio::MemoryOutputStream::new_resizable();
                let completed = Rc::new(Cell::new(false));
                let done = completed.clone();
                provider.write_mime_type_async(
                    TEXT_MIME,
                    &stream,
                    glib::Priority::DEFAULT,
                    gio::Cancellable::NONE,
                    move |result| {
                        result.unwrap();
                        done.set(true);
                    },
                );
                let deadline = std::time::Instant::now() + Duration::from_millis(200);
                while !completed.get() && std::time::Instant::now() < deadline {
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(
                    completed.get(),
                    "native provider must complete without the global context"
                );
                stream.close(gio::Cancellable::NONE).unwrap();
                stream.steal_as_bytes().as_ref().to_vec()
            })
            .unwrap()
    }

    #[test]
    fn retiring_owned_text_replaces_native_cached_bytes_but_preserves_new_local_copy() {
        let remote = RemoteClipboard::default();
        let old = materialized_text_provider("synthetic old remote text".into());
        assert!(remote.publication.borrow_mut().claim(0, old.clone()));
        // Model the already-fulfilled native cache. GTK 4.24.1 does not reach
        // clearContents for a NULL provider, so NULL intentionally does nothing.
        let mut cached = native_bytes(&old);
        let mut replacement = None;
        remote.clear_owned(Some(&old), |provider| {
            if let Some(provider) = provider {
                assert!(remote.writing.get(), "reentrant changed must not echo");
                cached = native_bytes(provider);
                replacement = Some(provider.clone());
            }
            Ok(())
        });
        assert!(
            cached.is_empty(),
            "retirement left fulfilled remote text in the native cache"
        );
        let replacement = replacement.expect("must publish a non-NULL native provider");
        assert!(
            remote.publication.borrow().owns(Some(&replacement)),
            "empty replacement must not echo later"
        );
        let local = materialized_text_provider("synthetic newer local copy".into());
        let local_bytes = native_bytes(&local);
        remote.clear_owned(Some(&local), |_| {
            panic!("must not replace someone else's selection")
        });
        assert_eq!(native_bytes(&local), local_bytes);
    }

    #[test]
    fn offers_local_changes_and_connection_changes_invalidate_publication() {
        let mut gate = Publication::default();
        let initial = gate.generation;
        assert!(gate.claim(initial, 10));
        assert!(gate.owns(Some(&10)), "our exact provider must not echo");
        assert!(
            !gate.owns(Some(&11)),
            "another provider with any bytes is local"
        );
        // Local copy: cancel the pending remote result, but don't clear local.
        assert!(!gate.invalidate(Some(&11)));
        assert!(!gate.claim(initial, 12));
        let next = gate.generation;
        assert!(gate.claim(next, 12));
        // New offer/machine/disconnect/shutdown all use this same invalidation.
        for _ in 0..4 {
            let old = gate.generation;
            assert!(
                gate.invalidate(Some(&12)),
                "retire the previous remote cache"
            );
            assert!(!gate.claim(old, 99), "stale completion must never publish");
            assert!(gate.claim(gate.generation, 12));
        }
        assert!(!gate.invalidate(None));
        assert!(!gate.owns(None));
    }

    #[test]
    fn prefetch_never_accepts_sensitive_files_images_or_ambiguous_legacy_encoding() {
        for mimes in [
            vec![],
            vec!["image/png"],
            vec!["text/uri-list"],
            vec!["STRING"],
            vec!["TEXT"],
            vec![TEXT_MIME, "x-kde-passwordManagerHint"],
        ] {
            assert!(!text_offer(
                &mimes.into_iter().map(String::from).collect::<Vec<_>>()
            ));
        }
        for mime in [TEXT_MIME, "text/plain", "UTF8_STRING"] {
            assert!(text_offer(&[mime.into()]));
        }
    }

    #[test]
    fn cancelling_ui_task_drops_reply_and_stalled_queue_has_deadline() {
        let context = glib::MainContext::new();
        let remote = RemoteClipboard::default();
        let (sender, receiver) = oneshot::channel::<crate::clipboard_text::TextResult>();
        *remote.pending.borrow_mut() = Some(context.spawn_local(async move {
            let _ = receiver.await;
        }));
        remote.cancel_pending();
        assert!(sender.is_closed());
        let (sender, receiver) = oneshot::channel();
        let result = context.block_on(prepared_text(receiver, Duration::from_millis(5)));
        assert_eq!(result, Err("Clipboard: text transfer timed out (5 s)"));
        assert!(
            sender.is_closed(),
            "UI timeout must also cancel queued worker work"
        );
        let (sender, receiver) = oneshot::channel();
        drop(sender);
        assert_eq!(
            context.block_on(prepared_text(receiver, TEXT_DEADLINE)),
            Err("Clipboard: connection closed before text arrived")
        );
    }

    // No display, window or real clipboard: mimic GTK macOS' private context.
    #[test]
    fn text_provider_completes_on_private_context_without_global_iteration() {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let provider = materialized_text_provider("synthetic ñ 🦀".into());
                let stream = gio::MemoryOutputStream::new_resizable();
                let completed = Rc::new(Cell::new(false));
                let done = completed.clone();
                provider.write_mime_type_async(
                    "text/plain;charset=utf-8",
                    &stream,
                    glib::Priority::DEFAULT,
                    gio::Cancellable::NONE,
                    move |result| {
                        result.unwrap();
                        done.set(true);
                    },
                );
                let deadline = std::time::Instant::now() + Duration::from_millis(200);
                while !completed.get() && std::time::Instant::now() < deadline {
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(
                    completed.get(),
                    "NSPasteboard waits here forever: provider was scheduled on the global context"
                );
                stream.close(gio::Cancellable::NONE).unwrap();
                assert_eq!(
                    stream.steal_as_bytes().as_ref(),
                    "synthetic ñ 🦀".as_bytes()
                );
            })
            .unwrap();
    }
}

/// The transfer bars shown over the video while a paste from the server
/// takes a while: one row per job, with a cancel button. Empty rows are
/// removed, so the box is only visible while something is in flight.
pub struct TransferBars {
    list: gtk::Box,
    rows: RefCell<HashMap<u32, Row>>,
}

struct Row {
    root: gtk::Box,
    text: gtk::Label,
    bar: gtk::ProgressBar,
}

/// How long a failure stays on screen.
const FAILURE_LINGER: Duration = Duration::from_secs(6);

impl TransferBars {
    pub fn new() -> Self {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::End)
            .visible(false)
            .css_classes(["transfers"])
            .build();
        Self {
            list,
            rows: RefCell::default(),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.list
    }

    /// Show the state of job `id`; `cancel` is called with the id when the
    /// user presses the row's button.
    pub fn update(self: &Rc<Self>, id: u32, p: &Progress, cancel: impl Fn(u32) + 'static) {
        match &p.state {
            State::Running => {
                let mut rows = self.rows.borrow_mut();
                let row = rows.entry(id).or_insert_with(|| self.add_row(id, cancel));
                row.text
                    .set_text(&format!("Pasting {}: {}", p.label, describe(p)));
                match p.percent() {
                    Some(pct) => row.bar.set_fraction(pct as f64 / 100.0),
                    None => row.bar.pulse(),
                }
            }
            State::Done | State::Cancelled => self.remove(id),
            State::Failed(e) => {
                let mut rows = self.rows.borrow_mut();
                let row = rows.entry(id).or_insert_with(|| self.add_row(id, cancel));
                row.text
                    .set_text(&format!("Paste of {} failed: {e}", p.label));
                row.bar.set_visible(false);
                drop(rows);
                let this = self.clone();
                glib::timeout_add_local_once(FAILURE_LINGER, move || this.remove(id));
            }
        }
    }

    fn add_row(&self, id: u32, cancel: impl Fn(u32) + 'static) -> Row {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["transfer"])
            .build();
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        let text = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let bar = gtk::ProgressBar::new();
        column.append(&text);
        column.append(&bar);
        let button = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Cancel")
            .valign(gtk::Align::Center)
            .build();
        button.connect_clicked(move |_| cancel(id));
        root.append(&column);
        root.append(&button);
        self.list.append(&root);
        self.list.set_visible(true);
        Row { root, text, bar }
    }

    fn remove(&self, id: u32) {
        let mut rows = self.rows.borrow_mut();
        if let Some(row) = rows.remove(&id) {
            self.list.remove(&row.root);
        }
        self.list.set_visible(!rows.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an isolated Wayland session; run by scripts/e2e.sh"]
    fn password_manager_selection_cannot_be_offered_or_read() {
        gtk::init().unwrap();
        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let cb = clipboard().unwrap();
        let (tx, mut rx) = crate::input_queue::channel();
        watch_local(Rc::default(), move || vec![tx.clone()]);
        let provider = gdk::ContentProvider::new_union(&[
            gdk::ContentProvider::for_bytes(
                "text/plain",
                &glib::Bytes::from_static(b"private-password"),
            ),
            gdk::ContentProvider::for_bytes(
                "text/uri-list",
                &glib::Bytes::from_static(b"file:///private-secret\r\n"),
            ),
            gdk::ContentProvider::for_bytes(
                "x-kde-passwordManagerHint",
                &glib::Bytes::from_static(b"secret"),
            ),
        ]);
        cb.set_content(Some(&provider)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let offer = loop {
            context.iteration(false);
            if let Ok(offer) = rx.try_recv() {
                break offer;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sensitive offer was not cleared"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        match offer {
            ToWorker::LocalOffer { mime_types, files } => {
                assert!(mime_types.is_empty());
                assert!(files.entries.is_empty());
            }
            _ => panic!("expected a cleared offer"),
        }
        // A request racing the cleared offer must not read the live secret.
        let (reply, mut read) = tokio::sync::mpsc::channel(2);
        read_local(Rc::default(), "text/plain".into(), reply);
        let result = loop {
            context.iteration(false);
            if let Ok(result) = read.try_recv() {
                break result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "clipboard read was not refused"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(result.is_err());
    }
}
