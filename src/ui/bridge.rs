use crate::ui::video_view::VideoSink;
use reoling::{DeviceIdentity, ReolinkClient, StreamQuality, VideoType};
use std::net::IpAddr;
use tokio_stream::StreamExt;

pub enum AppEvent {
    LoggedIn(DeviceIdentity),
    /// A frame was pushed straight into GStreamer already. See
    /// `spawn_connection`'s doc comment for why frames no longer travel
    /// through this channel at all.
    FrameDelivered,
    Failed(String),
}

/// One connection's request for a `VideoSink`, sent to `main.rs`'s
/// GTK-main-thread responder task the first time a connection knows its
/// codec (from the first video frame) — see `spawn_connection`'s doc
/// comment.
pub type SinkRequest = (VideoType, tokio::sync::oneshot::Sender<VideoSink>);

/// How the user chose to reach the device — set by the explicit UID/IP
/// toggle in the connect dialog, never inferred or auto-detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectTarget {
    Uid(String),
    Ip { addr: IpAddr, port: u16 },
}

/// Which transport a UID connection should try. `Udp` (plain UDP/P2P, no
/// TCP fallback or preference) is the default and the only path the real
/// app targets — the official Reolink client never uses TCP for the BC
/// protocol. `PreferTcp` is diagnostic-only, opt-in via `main.rs`'s
/// `--prefer-tcp` flag; not exposed in the connect dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UidTransport {
    PreferTcp,
    Udp,
}

/// Spawns a dedicated tokio runtime on a background OS thread and drives the
/// whole connect→login→start_video flow there, forwarding progress to the
/// GTK main loop over an `async-channel` (GTK4/GLib are not thread-safe, so
/// no widget is ever touched off the main thread).
///
/// **Video frames bypass the GTK main loop entirely** — see
/// `.plans/reoling-baichuan-p2p-audit-2026-09-16.md` section 34. The
/// previous design routed every frame through `AppEvent::Frame` to a
/// `glib::spawn_future_local` task that called `VideoView::push_frame`
/// directly on the GTK main thread, coupling live-video delivery to
/// whatever else that thread was doing (layout, redraws, other widget
/// updates) — exactly the dependency a live source shouldn't have.
/// `gstreamer::Pipeline`/`AppSrc` are internally thread-safe, so this
/// function instead asks `main.rs`'s GTK-side responder (via
/// `sink_request_tx`) for a `VideoSink` handle once, the first time a
/// frame's codec is known, then pushes every frame — including that
/// first one — directly from this background thread. The GTK main thread
/// only ever sees `AppEvent::FrameDelivered { bytes }` afterward, for the
/// status label, not the frame data itself.
///
/// The returned `Notify` lets the caller (`main.rs`'s window-close handler)
/// ask this background task to disconnect promptly instead of just being
/// dropped — see `ReolinkClient::disconnect`'s doc comment for why that
/// matters: without it, the device kept streaming to us until its own idle
/// timeout, because nothing ever told it we were leaving.
pub fn spawn_connection(
    target: ConnectTarget,
    username: String,
    password: String,
    channel_id: u8,
    quality: StreamQuality,
    uid_transport: UidTransport,
    sink_request_tx: tokio::sync::mpsc::Sender<SinkRequest>,
) -> (async_channel::Receiver<AppEvent>, std::sync::Arc<tokio::sync::Notify>) {
    let (tx, rx) = async_channel::unbounded();
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let shutdown_bg = shutdown.clone();

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        runtime.block_on(async move {
            let connect_result = match target {
                // `Udp` (the default) is the plain UDP/P2P session — the
                // only path the real app targets. `PreferTcp` exists only
                // for diagnostic A/B testing against the UDP path on real
                // hardware (`--prefer-tcp`, see `main.rs`); it is never
                // the default and never should be — the official Reolink
                // app never uses TCP for the BC protocol.
                ConnectTarget::Uid(uid) => match uid_transport {
                    UidTransport::PreferTcp => ReolinkClient::connect_by_uid_prefer_tcp(&uid).await,
                    UidTransport::Udp => ReolinkClient::connect_by_uid(&uid).await,
                },
                ConnectTarget::Ip { addr, port } => ReolinkClient::connect_by_ip(addr, port).await,
            };
            let mut client = match connect_result {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(AppEvent::Failed(e.to_string())).await;
                    return;
                }
            };
            if let Err(e) = client.login(&username, &password).await {
                let _ = tx.send(AppEvent::Failed(e.to_string())).await;
                return;
            }
            let identity = client.identity().await;
            let _ = tx.send(AppEvent::LoggedIn(identity)).await;

            let mut frames = match client.start_video(channel_id, quality).await {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(AppEvent::Failed(e.to_string())).await;
                    return;
                }
            };

            // Requested lazily from the first frame (its codec is what
            // ensure_sink needs) and reused for every frame after —
            // exactly the same lazy-build-once semantics `VideoView` used
            // to implement internally, just with the request now crossing
            // a thread boundary once instead of every call happening on
            // the GTK thread.
            let mut sink: Option<VideoSink> = None;

            loop {
                tokio::select! {
                    frame = frames.next() => {
                        match frame {
                            Some(Ok(frame)) => {
                                if sink.is_none() {
                                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                                    if sink_request_tx.send((frame.video_type, reply_tx)).await.is_err() {
                                        break; // GTK side gone
                                    }
                                    let Ok(built) = reply_rx.await else {
                                        break; // GTK side gone before replying
                                    };
                                    sink = Some(built);
                                }
                                sink.as_ref().unwrap().push_frame(&frame);
                                if tx.send(AppEvent::FrameDelivered).await.is_err() {
                                    break; // UI side dropped the receiver (window closed)
                                }
                            }
                            Some(Err(e)) => {
                                let _ = tx.send(AppEvent::Failed(e.to_string())).await;
                                break;
                            }
                            None => break, // stream ended
                        }
                    }
                    _ = shutdown_bg.notified() => break,
                }
            }
            // Every exit path above ends the session — tell the device
            // before this task (and the client with it) goes away.
            client.disconnect().await;
        });
    });

    (rx, shutdown)
}
