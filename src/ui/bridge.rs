use crate::ui::video_view::VideoSink;
use reoling::{ChannelInfo, DeviceIdentity, DeviceUpdate, ReolinkClient, StreamProfile, VideoFrame, VideoType};
use std::net::IpAddr;
use tokio_stream::StreamExt;

/// What a device's connection tells the UI.
pub enum DeviceEvent {
    /// Logged in; carries the device's own name and model.
    Connected(DeviceIdentity),
    /// The device (NVR / Home Hub) described its channels.
    Channels(Vec<ChannelInfo>),
    /// The device's answer to a siren or spotlight command (`code` 200 is
    /// success).
    ControlReply { msg_id: u32, code: u16 },
    /// A control command could not even be sent.
    ControlFailed(String),
    /// A channel's name, asked for because the channel list had none.
    ChannelName { channel_id: u8, name: String },
    /// The requested stream's first frame reached GStreamer.
    Playing,
    /// The requested stream could not be started or broke off. The device
    /// itself is still connected.
    PlayFailed(String),
    /// The device refused the login.
    LoginRejected,
    /// The device could not be reached, or the connection
    /// died. Nothing more will come.
    Lost(String),
}

enum Command {
    Play { channel: u8, profile: StreamProfile },
    Siren { channel: u8 },
    Spotlight { channel: u8, on: bool },
    Stop,
    Shutdown,
}

/// The UI's handle on one device's connection (which lives on its own
/// thread). Dropping it disconnects.
pub struct DeviceLink {
    pub events: async_channel::Receiver<DeviceEvent>,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
}

impl DeviceLink {
    /// Streams the channel (replacing whatever this device was streaming).
    pub fn play(&self, channel: u8, profile: StreamProfile) {
        let _ = self.commands.send(Command::Play { channel, profile });
    }

    /// Plays the camera's siren once.
    pub fn siren(&self, channel: u8) {
        let _ = self.commands.send(Command::Siren { channel });
    }

    /// Spotlight on or off.
    pub fn spotlight(&self, channel: u8, on: bool) {
        let _ = self.commands.send(Command::Spotlight { channel, on });
    }

    /// Stops the stream; the connection stays.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    /// Tells the device we are leaving.
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

impl Drop for DeviceLink {
    fn drop(&mut self) {
        self.shutdown();
    }
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

/// Connects and logs in to a device on a dedicated thread with its own
/// tokio runtime, and keeps the connection up: it listens for pushes (the
/// channel list) and streams a channel on request. GTK is never touched from
/// there — everything reaches the UI through `DeviceLink::events`.
///
/// **Video frames bypass the GTK main loop entirely** — see
/// `.plans/reoling-baichuan-p2p-audit-2026-09-16.md` section 34. Routing
/// every frame through a `glib` task coupled live video to whatever else the
/// GTK thread was doing. `gstreamer::Pipeline`/`AppSrc` are thread-safe, so
/// this thread asks `main_window`'s responder (via `sink_request_tx`) for a
/// `VideoSink` once per stream, when the first frame shows its codec, then
/// pushes every frame itself. The UI only hears `Playing`.
pub fn spawn_device(
    target: ConnectTarget,
    username: String,
    password: String,
    uid_transport: UidTransport,
    sink_request_tx: tokio::sync::mpsc::Sender<SinkRequest>,
    audio_out: std::sync::Arc<crate::ui::audio::AudioOutput>,
) -> DeviceLink {
    let (tx, rx) = async_channel::unbounded();
    let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        runtime.block_on(async move {
            let connect_result = match target {
                // `Udp` is the plain UDP/P2P session, the only path the real
                // app targets. `PreferTcp` is diagnostic-only
                // (`--prefer-tcp`, see `main.rs`); the official Reolink app
                // never uses TCP for the BC protocol.
                ConnectTarget::Uid(uid) => match uid_transport {
                    UidTransport::PreferTcp => ReolinkClient::connect_by_uid_prefer_tcp(&uid).await,
                    UidTransport::Udp => ReolinkClient::connect_by_uid(&uid).await,
                },
                ConnectTarget::Ip { addr, port } => ReolinkClient::connect_by_ip(addr, port).await,
            };
            let mut client = match connect_result {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(DeviceEvent::Lost(e.to_string())).await;
                    return;
                }
            };
            match client.login(&username, &password).await {
                Ok(_) => {}
                Err(reoling::Error::LoginFailed { .. }) => {
                    let _ = tx.send(DeviceEvent::LoginRejected).await;
                    return;
                }
                Err(e) => {
                    let _ = tx.send(DeviceEvent::Lost(e.to_string())).await;
                    return;
                }
            }
            let mut channel_updates =
                client.take_channel_updates().expect("taken once per client");
            let mut audio = client.take_audio().expect("taken once per client");
            let identity = client.identity().await;
            if std::env::var("REOLING_PROBE_PUSH").is_ok() {
                client.probe_pushes(&username).await;
            }
            let _ = tx.send(DeviceEvent::Connected(identity)).await;

            let mut frames: Option<tokio_stream::wrappers::ReceiverStream<reoling::Result<VideoFrame>>> =
                None;
            let mut sink: Option<VideoSink> = None;
            let mut announced = false;
            let mut channel = 0u8;
            let mut asked_names: std::collections::HashSet<u8> = std::collections::HashSet::new();

            loop {
                tokio::select! {
                    command = command_rx.recv() => match command {
                        Some(Command::Play { channel: wanted, profile }) => {
                            if frames.take().is_some() {
                                let _ = client.stop_video(channel).await;
                            }
                            channel = wanted;
                            sink = None;
                            announced = false;
                            match client.start_video(channel, profile).await {
                                Ok(f) => frames = Some(f),
                                Err(e) => {
                                    let _ = tx.send(DeviceEvent::PlayFailed(e.to_string())).await;
                                }
                            }
                        }
                        Some(Command::Siren { channel }) => {
                            if let Err(e) = client.play_siren(channel).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::Spotlight { channel, on }) => {
                            if let Err(e) = client.set_spotlight(channel, on).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::Stop) => {
                            if frames.take().is_some() {
                                let _ = client.stop_video(channel).await;
                            }
                        }
                        Some(Command::Shutdown) | None => break,
                    },
                    Some(frame) = audio.recv() => audio_out.push(&frame),
                    Some(update) = channel_updates.recv() => {
                        let event = match update {
                            DeviceUpdate::Channels(channels) => {
                                // An NVR's list carries no names; ask for the
                                // ones we do not have (once per channel).
                                let unnamed: Vec<u8> = channels
                                    .iter()
                                    .filter(|c| c.online && c.name.is_empty())
                                    .map(|c| c.channel_id)
                                    .filter(|id| asked_names.insert(*id))
                                    .collect();
                                client.request_channel_names(&unnamed).await;
                                DeviceEvent::Channels(channels)
                            }
                            DeviceUpdate::ChannelName { channel_id, name } => {
                                DeviceEvent::ChannelName { channel_id, name }
                            }
                            DeviceUpdate::ControlReply { msg_id, code } => {
                                DeviceEvent::ControlReply { msg_id, code }
                            }
                        };
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    frame = async {
                        match frames.as_mut() {
                            Some(f) => f.next().await,
                            None => std::future::pending().await,
                        }
                    } => match frame {
                        Some(Ok(frame)) => {
                            if sink.is_none() {
                                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                                if sink_request_tx.send((frame.video_type, reply_tx)).await.is_err() {
                                    break; // GTK side gone
                                }
                                let Ok(built) = reply_rx.await else { break };
                                sink = Some(built);
                            }
                            sink.as_ref().expect("just built").push_frame(&frame);
                            if !announced {
                                announced = true;
                                if tx.send(DeviceEvent::Playing).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            frames = None;
                            let _ = tx.send(DeviceEvent::PlayFailed(e.to_string())).await;
                        }
                        None => frames = None,
                    },
                    error = client.wait_for_pushes(), if frames.is_none() => {
                        let _ = tx.send(DeviceEvent::Lost(error.to_string())).await;
                        break;
                    }
                }
            }
            // Every exit path ends the session — tell the device before
            // this task (and the client with it) goes away.
            client.disconnect().await;
        });
    });

    DeviceLink { events: rx, commands }
}
