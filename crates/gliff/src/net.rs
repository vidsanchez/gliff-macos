//! Client network worker: SSH/TCP, native H.264 decode and input/clipboard.
//! Only pooled BGRA pixels cross to the GTK thread.

use std::rc::Rc;
use std::sync::mpsc::{Sender as StdSender, SyncSender};

use crate::input_queue::Receiver;
use anyhow::Context;
use bytes::Bytes;
use gliff_proto::clipboard::CHUNK;
use gliff_proto::{
    features, is_incompatible, version_mismatch, ChromaMode, ClientCaps, ClientMsg, ClipboardFile,
    ClipboardMsg, Codec, ServerMsg, VideoPipeline, PROTOCOL_VERSION,
};
use gliff_transport::clipboard::progress::Progress;
use gliff_transport::clipboard::{outbound_channel, Side, Transfers};
use gliff_transport::{spawn_ssh, Framed, SshTarget};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, unbounded_channel};

use crate::clipboard::{Bridge, ToWorker};

const MAX_STREAM: (u32, u32) = (3840, 2160);
// Older protocol-v1 GPU servers ignore the advertised limits on their first
// stream. Accept their bounded encoder maximum so they can reach the resize.
const MAX_DECODE: (u32, u32) = (4096, 4096);

/// A decoded frame with the geometry it was configured under, so the UI
/// never paints a frame with another configuration's view or scale (frames
/// and statuses travel on separate channels).
pub struct Picture {
    pub frame: Frame,
    /// Stream size in physical pixels.
    pub stream: (u32, u32),
    /// The full-quality fit size the frame should be drawn into.
    pub view: (u32, u32),
    /// Effective stream scale x1000 for pointer mapping.
    pub scale_milli: u32,
}

/// Status/telemetry the worker reports to the UI. Pointer-mapping geometry
/// is deliberately absent: it travels with each `Picture`, so the UI never
/// maps clicks against a configuration whose frame is not on screen yet.
pub enum Status {
    Connected {
        /// "server tier→client decoder", e.g. "GPU→VT".
        video: String,
        view_width: u32,
        view_height: u32,
        fps_cap: u32,
        /// The server serves a dedicated headless output whose frames carry
        /// the compositor-drawn cursor.
        headless: bool,
    },
    Stats {
        fps: f32,
        mbit: f32,
        decode_ms: f32,
        /// "server tier→client decoder", e.g. "GPU→VT".
        video: String,
    },
    /// The remote cursor image, for the client to set as its widget cursor.
    Cursor {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb: Vec<u8>,
    },
    /// The remote selection changed. The UI prefetches bounded UTF-8 text with
    /// this serial before publishing a native in-memory clipboard provider.
    ClipboardOffer {
        serial: u32,
        mime_types: Vec<String>,
        files: Vec<ClipboardFile>,
    },
    /// The server wants `mime_type` from the local clipboard: read it into
    /// `reply` chunk by chunk and drop the sender at the end.
    ClipboardRead {
        mime_type: String,
        reply: mpsc::Sender<std::io::Result<Bytes>>,
    },
    /// A paste from the server's offer that is taking a while, or ended.
    ClipboardTransfer {
        id: u32,
        progress: Progress,
    },
    Error(String),
    /// The two ends speak different protocol versions, so retrying cannot
    /// help.
    Incompatible(String),
    Closed,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Incompatible(String);

/// How to reach the server.
#[derive(Clone)]
pub enum Endpoint {
    Tcp(String),
    Ssh(SshTarget),
}

/// A native-decoded BGRA frame for the GTK thread.
pub enum Frame {
    Bgra(gliff_sw::PooledFrame),
}

pub struct Worker {
    pub endpoint: Endpoint,
    /// Bounded so a stalled UI thread cannot make the decoder buffer frames
    /// without limit; when full, the newest frame is dropped (latest-wins).
    pub frames: SyncSender<Picture>,
    pub status: StdSender<Status>,
    pub input: Receiver,
    pub keymap: String,
}

impl Worker {
    /// Run to completion on the calling thread (spawn it yourself).
    pub fn run(self) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = self.status.send(Status::Error(e.to_string()));
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        let status = self.status.clone();
        let result: anyhow::Result<()> = local.block_on(&rt, async move {
            match self.endpoint {
                Endpoint::Tcp(ref addr) => {
                    let stream = tokio::net::TcpStream::connect(addr).await?;
                    stream.set_nodelay(true)?;
                    let (rd, wr) = tokio::io::split(stream);
                    session(rd, wr, self.frames, self.status, self.input, self.keymap).await
                }
                Endpoint::Ssh(ref target) => {
                    let ssh = spawn_ssh(target)?;
                    session(
                        ssh.stdout,
                        ssh.stdin,
                        self.frames,
                        self.status,
                        self.input,
                        self.keymap,
                    )
                    .await
                }
            }
        });
        if let Err(e) = result {
            let _ = status.send(match e.downcast::<Incompatible>() {
                Ok(Incompatible(message)) => Status::Incompatible(message),
                Err(e) => Status::Error(e.to_string()),
            });
        } else {
            let _ = status.send(Status::Closed);
        }
    }
}

fn new_decoder(chroma: ChromaMode, width: u32, height: u32) -> anyhow::Result<gliff_sw::Decoder> {
    validate_stream_size(width, height)?;
    Ok(gliff_sw::Decoder::new(
        chroma != ChromaMode::Single420,
        width,
        height,
    )?)
}

/// The tiers in use, server then client, as the stats line shows them:
/// "GPU→VT" or "CPU→VT".
fn tier_label(pipeline: Option<VideoPipeline>) -> String {
    let server = pipeline.map_or("?", |p| match p {
        VideoPipeline::Gpu => "GPU",
        VideoPipeline::Cpu => "CPU",
    });
    format!("{server}→VT")
}

async fn session<R, W>(
    rd: R,
    wr: W,
    frames: SyncSender<Picture>,
    status: StdSender<Status>,
    input: Receiver,
    keymap: String,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let failure = input.failure();
    // Observe overflow independently of handshake, writer backpressure and
    // the queued backlog. Worker reports Error and drops this connection.
    tokio::select! { biased;
        _ = failure.wait() => anyhow::bail!("Essential input/control queue exhausted; connection closed"),
        result = session_inner(rd, wr, frames, status, input, keymap) => result,
    }
}

async fn session_inner<R, W>(
    rd: R,
    wr: W,
    frames: SyncSender<Picture>,
    status: StdSender<Status>,
    mut input: Receiver,
    keymap: String,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);

    let caps = ClientCaps {
        codecs: vec![Codec::H264],
        max_width: MAX_STREAM.0,
        max_height: MAX_STREAM.1,
        // Native client accepts one 4:2:0 stream, not Dual420.
        chroma: vec![ChromaMode::Single420],
        features: features(),
    };
    tracing::debug!(bytes = keymap.len(), "sending native keymap");
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap,
            caps,
        })
        .await?;

    // Between HelloAck and StreamConfig the server may send a Ping; answer
    // it before anything else so the server's round-trip
    // estimate is seeded ahead of the first frame ack.
    let mut got_ack = false;
    let mut headless = false;
    let (mut width, mut height, mut chroma, mut scale_milli, pipeline, mut view, mut fps_cap) = loop {
        match reader.read_msg::<ServerMsg>().await? {
            ServerMsg::HelloAck {
                version,
                features,
                session,
                ..
            } => {
                if version != PROTOCOL_VERSION {
                    return Err(
                        Incompatible(version_mismatch(PROTOCOL_VERSION, version).into()).into(),
                    );
                }
                tracing::debug!(?features, "server features");
                headless = session.headless;
                got_ack = true;
            }
            ServerMsg::Ping { t } => writer.write_msg(&ClientMsg::Pong { t }).await?,
            ServerMsg::StreamConfig {
                width,
                height,
                chroma,
                scale_milli,
                pipeline,
                view_width,
                view_height,
                fps_cap,
                ..
            } if got_ack => {
                break (
                    width,
                    height,
                    chroma,
                    scale_milli,
                    pipeline,
                    (view_width, view_height),
                    fps_cap,
                )
            }
            ServerMsg::Error { code, message, .. } if is_incompatible(code) => {
                return Err(Incompatible(message).into())
            }
            ServerMsg::Error { code, message, .. } => {
                anyhow::bail!("server error {code}: {message}")
            }
            other => anyhow::bail!("unexpected message before StreamConfig: {other:?}"),
        }
    };
    let mut decoder = new_decoder(chroma, width, height)?;
    let mut video_label = tier_label(pipeline);
    let _ = status.send(Status::Connected {
        video: video_label.clone(),
        view_width: view.0,
        view_height: view.1,
        fps_cap,
        headless,
    });
    tracing::info!(width, height, scale_milli, decode_on = "videotoolbox", video = %video_label, "connected");
    let mut logged_first = false;

    // Writes run on their own task, fed by `out_tx`, so reads (draining video)
    // never block on a write and the two peers cannot deadlock. Both the reader
    // loop (acks, keyframe requests) and the UI thread (input) feed `out_tx`.
    let (out_tx, mut out_rx) = mpsc::channel::<(ClientMsg, Bytes)>(256);
    let mut writes = tokio::task::spawn_local(async move {
        while let Some((m, payload)) = out_rx.recv().await {
            let closing = matches!(m, ClientMsg::Bye);
            if payload.is_empty() {
                writer.write_msg(&m).await?;
            } else {
                writer.write_msg_with_payloads(&m, &[&payload]).await?;
            }
            if closing {
                return Ok(());
            }
        }
        Ok::<(), gliff_transport::Error>(())
    });
    // Clipboard transfers write through the same channel, in chunks.
    let (clip_out_tx, mut clip_out_rx) = outbound_channel();
    let clipboard = Rc::new(Bridge::new(
        Transfers::new(clip_out_tx, Side::Client),
        status.clone(),
    ));
    {
        let out_tx = out_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some((m, payload)) = clip_out_rx.recv().await {
                if out_tx.send((ClientMsg::from(m), payload)).await.is_err() {
                    break;
                }
            }
        });
    }
    // Forward UI input into the same write channel; clipboard commands from
    // the UI go to the bridge. Closing the UI sender enqueues Bye after
    // releases, so the writer drains those messages before ending the session.
    // Releases, layout changes and keys share this FIFO queue.
    // Display pixels per stream pixel the UI wants; the main loop applies it.
    let (zoom_tx, mut zoom_rx) = unbounded_channel::<u32>();
    {
        let out_tx = out_tx.clone();
        let clipboard = clipboard.clone();
        tokio::task::spawn_local(async move {
            loop {
                let msg = match input.recv().await {
                    Some(ToWorker::Send(m)) => m,
                    Some(ToWorker::Zoom(z)) => {
                        let _ = zoom_tx.send(z.max(1));
                        continue;
                    }
                    Some(other) => {
                        clipboard.on_ui(other);
                        continue;
                    }
                    None => break,
                };
                if out_tx.send((msg, Bytes::new())).await.is_err() {
                    return;
                }
            }
            // The writer must drain releases before ending this connection.
            // Returning ui_gone immediately could abort queued release writes.
            let _ = out_tx.send((ClientMsg::Bye, Bytes::new())).await;
        });
    }

    let mut frames_since = 0u32;
    let mut bytes_since = 0u64;
    let mut decode_ms_acc = 0f32;
    let mut last_report = std::time::Instant::now();

    // Retry the latest frame if the bounded UI channel was full. Native decode
    // returns exactly one picture per successful VCL access unit, with no drain.
    const IDLE_DRAIN: std::time::Duration = std::time::Duration::from_millis(150);
    let mut drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
    let mut undelivered: Option<Picture> = None;
    let mut zoom = 1u32;
    let pool = gliff_sw::PixelPool::default();
    loop {
        let read = tokio::select! {
            read = reader.read_msg::<ServerMsg>() => read,
            Some(z) = zoom_rx.recv() => {
                if z == zoom {
                    continue;
                }
                zoom = z;
                // Request fresh pixels to redraw at another integer zoom.
                let _ = out_tx.try_send((ClientMsg::RequestKeyframe, Bytes::new()));
                continue;
            }
            written = &mut writes => {
                written.context("writer task")?.context("write to server")?;
                return Ok(());
            }
            _ = tokio::time::sleep_until(drain_at),
                if undelivered.is_some() =>
            {
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                let picture = undelivered.take();
                if let Some(picture) = picture {
                    if let Err(std::sync::mpsc::TrySendError::Full(picture)) =
                        frames.try_send(picture)
                    {
                        undelivered = Some(picture);
                    }
                }
                continue;
            }
        };
        let msg = match read {
            Ok(m) => m,
            Err(gliff_transport::Error::Closed) => break,
            Err(e) => return Err(e.into()),
        };
        match msg {
            ServerMsg::VideoFrame {
                frame_id,
                keyframe: _,
                data_len,
                aux_len,
                ..
            } => {
                let main = reader.read_payload(data_len).await?;
                let aux = if aux_len > 0 {
                    reader.read_payload(aux_len).await?
                } else {
                    bytes::Bytes::new()
                };
                bytes_since += (data_len + aux_len) as u64;
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                let t0 = std::time::Instant::now();
                let decoded = decoder
                    .decode(&main, &aux)
                    .map(|f| f.map(|f| Frame::Bgra(f.zoomed(zoom, &pool))))
                    .map_err(anyhow::Error::from);
                let dec_ms = t0.elapsed().as_secs_f32() * 1000.0;
                // Ack immediately so the server keeps pacing.
                let _ = out_tx.try_send((
                    ClientMsg::FrameAck {
                        frame_id,
                        decoded_at_ms: now_ms(),
                    },
                    Bytes::new(),
                ));
                match decoded {
                    Ok(Some(frame)) => {
                        if !logged_first {
                            tracing::info!("first frame decoded");
                            logged_first = true;
                        }
                        // Geometry comes from this access unit's current config;
                        // synchronous native decoding cannot return an older AU.
                        let picture = Picture {
                            frame,
                            stream: (width, height),
                            view,
                            scale_milli,
                        };
                        match frames.try_send(picture) {
                            Ok(()) => undelivered = None,
                            Err(std::sync::mpsc::TrySendError::Full(picture)) => {
                                undelivered = Some(picture)
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
                        }
                        frames_since += 1;
                        decode_ms_acc += dec_ms;
                    }
                    // Kept for the decoder interface; native VCL decode returns a frame or error.
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "decode error; requesting keyframe");
                        let _ = out_tx.try_send((ClientMsg::RequestKeyframe, Bytes::new()));
                    }
                }
            }
            ServerMsg::StreamConfig {
                width: w,
                height: h,
                chroma: c,
                scale_milli: s,
                pipeline,
                view_width,
                view_height,
                fps_cap: f,
                ..
            } => {
                validate_stream_size(w, h)?;
                // A pace-only change (fps cap) must not reset the decoder;
                // a coded-stream change does, and drops the frame waiting
                // for the UI with it.
                if (w, h, c) != (width, height, chroma) {
                    decoder = new_decoder(c, w, h)?;
                    undelivered = None;
                }
                (width, height, chroma, scale_milli) = (w, h, c, s);
                (view, fps_cap) = ((view_width, view_height), f);
                video_label = tier_label(pipeline);
                let _ = status.send(Status::Connected {
                    video: video_label.clone(),
                    view_width,
                    view_height,
                    fps_cap,
                    headless,
                });
            }
            ServerMsg::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb_len,
                ..
            } => {
                let argb = reader.read_payload(argb_len).await?.to_vec();
                let _ = status.send(Status::Cursor {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb,
                });
            }
            ServerMsg::Ping { t } => {
                let _ = out_tx.try_send((ClientMsg::Pong { t }, Bytes::new()));
            }
            ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. } => {}
            ServerMsg::Error { code, message, .. } if is_incompatible(code) => {
                return Err(Incompatible(message).into())
            }
            ServerMsg::Error { code, message, .. } => {
                anyhow::bail!("server error {code}: {message}")
            }
            ServerMsg::HelloAck { .. } => {}
            other => {
                if let Ok(clip) = other.into_clipboard() {
                    let payload = match &clip {
                        ClipboardMsg::Data { data_len, .. } => {
                            if *data_len as usize > CHUNK {
                                anyhow::bail!(
                                    "clipboard chunk of {data_len} bytes exceeds the limit"
                                );
                            }
                            reader.read_payload(*data_len).await?
                        }
                        _ => Bytes::new(),
                    };
                    clipboard.on_peer_msg(clip, payload);
                }
            }
        }
        if last_report.elapsed().as_secs_f32() >= 1.0 {
            let secs = last_report.elapsed().as_secs_f32();
            let _ = status.send(Status::Stats {
                fps: frames_since as f32 / secs,
                mbit: bytes_since as f32 * 8.0 / 1_000_000.0 / secs,
                decode_ms: if frames_since > 0 {
                    decode_ms_acc / frames_since as f32
                } else {
                    0.0
                },
                video: video_label.clone(),
            });
            frames_since = 0;
            bytes_since = 0;
            decode_ms_acc = 0.0;
            last_report = std::time::Instant::now();
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn validate_stream_size(width: u32, height: u32) -> anyhow::Result<()> {
    anyhow::ensure!(width > 0 && height > 0 && width <= MAX_DECODE.0 && height <= MAX_DECODE.1
        && width.is_multiple_of(2) && height.is_multiple_of(2),
        "invalid stream dimensions {width}x{height}; expected even dimensions within 4096x4096 decoder limit");
    Ok(())
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    #[test]
    fn reject_invalid_stream_geometry_before_creating_a_decoder() {
        for size in [
            (0, 1080),
            (1920, 0),
            (4098, 2160),
            (3840, 4098),
            (u32::MAX, 2),
            (1919, 1080),
        ] {
            assert!(validate_stream_size(size.0, size.1).is_err());
        }
        assert!(validate_stream_size(3840, 2160).is_ok());
        assert!(validate_stream_size(2, 2).is_ok());
    }

    #[test]
    fn accepts_legacy_server_startup_before_resize() {
        assert!(validate_stream_size(4096, 1152).is_ok());
        assert!(validate_stream_size(4096, 2304).is_ok());
        assert!(validate_stream_size(4096, 4096).is_ok());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn native_frames_follow_config_and_errors_request_idr_without_losing_geometry() {
        fn config(w: u32, h: u32, view: (u32, u32), scale: u32, fps: u32) -> ServerMsg {
            ServerMsg::StreamConfig {
                codec: Codec::H264,
                chroma: ChromaMode::Single420,
                pipeline: Some(VideoPipeline::Cpu),
                width: w,
                height: h,
                scale_milli: scale,
                extradata: vec![],
                aux_extradata: None,
                view_width: view.0,
                view_height: view.1,
                fps_cap: fps,
            }
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let (client,server) = tokio::io::duplex(8192);
                let (rd,wr) = tokio::io::split(client);
                let (srd,swr) = tokio::io::split(server);
                let (mut reader, mut writer) = (Framed::new(srd),Framed::new(swr));
                let (input_tx,input_rx) = crate::input_queue::channel();
                let (frame_tx,frame_rx) = std::sync::mpsc::sync_channel(2);
                let (status_tx,_status_rx) = std::sync::mpsc::channel();
                let task = tokio::task::spawn_local(session(rd,wr,frame_tx,status_tx,input_rx,String::new()));
                assert!(matches!(reader.read_msg::<ClientMsg>().await.unwrap(),ClientMsg::Hello {caps,..} if caps.chroma == vec![ChromaMode::Single420]));
                writer.write_msg(&ServerMsg::HelloAck { version:PROTOCOL_VERSION,
                    session:gliff_proto::SessionInfo {headless:false,output:"synthetic".into()},
                    outputs:vec![],features:features() }).await.unwrap();
                writer.write_msg(&config(64,48,(64,48),1000,60)).await.unwrap();
                let idr = include_bytes!("../../gliff-sw/tests/fixtures/64x48-0.h264").as_slice();
                let p = include_bytes!("../../gliff-sw/tests/fixtures/64x48-1.h264").as_slice();
                let resized = include_bytes!("../../gliff-sw/tests/fixtures/80x64-0.h264").as_slice();
                for (id,bytes,geometry) in [(1,idr,((64,48),(64,48),1000)),
                    (2,p,((64,48),(128,96),500)), (3,&[0,0,1,0x65][..],((0,0),(0,0),0)),
                    (4,idr,((64,48),(128,96),500)), (5,resized,((80,64),(80,64),1000))] {
                    if id == 2 { writer.write_msg(&config(64,48,(128,96),500,15)).await.unwrap(); }
                    if id == 5 { writer.write_msg(&config(80,64,(80,64),1000,60)).await.unwrap(); }
                    writer.write_msg_with_payloads(&ServerMsg::VideoFrame { frame_id:id,pts_us:id*16000,
                        keyframe:id != 2,damage:vec![],data_len:bytes.len() as u32,aux_len:0 }, &[bytes]).await.unwrap();
                    assert!(matches!(reader.read_msg::<ClientMsg>().await.unwrap(),ClientMsg::FrameAck {frame_id,..} if frame_id == id));
                    if id == 3 {
                        assert_eq!(reader.read_msg::<ClientMsg>().await.unwrap(),ClientMsg::RequestKeyframe);
                        assert!(frame_rx.try_recv().is_err());
                    } else {
                        let picture = frame_rx.try_recv().unwrap();
                        assert_eq!((picture.stream,picture.view,picture.scale_milli),geometry);
                        let Frame::Bgra(frame) = picture.frame;
                        assert_eq!((frame.width,frame.height),geometry.0);
                    }
                }
                drop(input_tx);
                assert_eq!(reader.read_msg::<ClientMsg>().await.unwrap(),ClientMsg::Bye);
                task.await.unwrap().unwrap();
            }).await.expect("headless synthetic session completes");
        });
    }

    #[test]
    fn essential_overflow_aborts_stalled_handshake_without_draining() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let (client, _unresponsive_server) = tokio::io::duplex(16);
            let (rd, wr) = tokio::io::split(client);
            let (tx, input) = crate::input_queue::channel();
            let (frames, _frames) = std::sync::mpsc::sync_channel(2);
            let (status, _status) = std::sync::mpsc::channel();
            let task =
                tokio::task::spawn_local(session(rd, wr, frames, status, input, String::new()));
            tokio::task::yield_now().await; // Hello writer now waits on a full pipe.
            for _ in 0..256 {
                tx.send(ToWorker::CancelTransfer(1)).unwrap();
            }
            assert!(tx.send(ToWorker::CancelTransfer(2)).is_err());
            let result = tokio::time::timeout(std::time::Duration::from_millis(100), task)
                .await
                .unwrap()
                .unwrap();
            assert!(result.unwrap_err().to_string().contains("queue exhausted"));
        });
    }
    #[test]
    fn release_and_layout_messages_are_written_before_bye_on_shutdown() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let (client,server) = tokio::io::duplex(4096);
                let (rd,wr) = tokio::io::split(client);
                let (srd,swr) = tokio::io::split(server);
                let mut reader = Framed::new(srd);
                let mut writer = Framed::new(swr);
                let (input_tx,input_rx) = crate::input_queue::channel();
                let (frame_tx,_frame_rx) = std::sync::mpsc::sync_channel(2);
                let (status_tx,_status_rx) = std::sync::mpsc::channel();
                let task = tokio::task::spawn_local(session(rd,wr,frame_tx,status_tx,input_rx,"initial map".into()));
                assert!(matches!(reader.read_msg::<ClientMsg>().await.unwrap(),ClientMsg::Hello { keymap,.. } if keymap == "initial map"));
                writer.write_msg(&ServerMsg::HelloAck {
                    version: PROTOCOL_VERSION,
                    session: gliff_proto::SessionInfo { headless:false,output:"test".into() },
                    outputs: vec![],features:features(),
                }).await.unwrap();
                writer.write_msg(&ServerMsg::StreamConfig {
                    codec:Codec::H264,chroma:ChromaMode::Single420,pipeline:Some(VideoPipeline::Cpu),
                    width:2,height:2,scale_milli:1000,extradata:vec![],aux_extradata:None,
                    view_width:2,view_height:2,fps_cap:30,
                }).await.unwrap();
                let expected = vec![ClientMsg::Key { keycode:42,pressed:false },
                    ClientMsg::Keymap { keymap:"new layout".into() },
                    ClientMsg::Key { keycode:30,pressed:true },
                    ClientMsg::Key { keycode:30,pressed:false }];
                let mut expected = expected;
                // Fill normal quota, then exercise reserved releases. Closing
                // the producer must drain ALL these before the generated Bye.
                expected.extend((0..124).map(|_| ClientMsg::Key { keycode:30, pressed:true }));
                expected.extend((0..100).map(|_| ClientMsg::Key { keycode:30, pressed:false }));
                for m in &expected { input_tx.send(ToWorker::Send(m.clone())).unwrap(); }
                drop(input_tx);
                for m in expected { assert_eq!(reader.read_msg::<ClientMsg>().await.unwrap(),m); }
                assert_eq!(reader.read_msg::<ClientMsg>().await.unwrap(),ClientMsg::Bye);
                task.await.unwrap().unwrap();
            }).await.expect("worker must drain and close");
        });
    }
}
