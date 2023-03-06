use crate::events::{Event, EventManager};
use crate::model::playable::Playable;
use crate::queue::QueueEvent;
use crate::spotify::PlayerEvent;
use futures::channel::oneshot;
use futures::{Future, FutureExt};
use librespot_core::token::Token;
use librespot_core::session::Session;
use librespot_core::spotify_id::{SpotifyItemType, SpotifyId};
use librespot_playback::mixer::Mixer;
use librespot_playback::player::{Player, PlayerEvent as LibrespotPlayerEvent};
use log::{debug, error, info, warn};
use std::time::Duration;
use std::{pin::Pin, time::SystemTime};
use tokio::sync::mpsc;
use tokio::time;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;

#[derive(Debug)]
pub(crate) enum WorkerCommand {
    Load(Playable, bool, u32),
    Play,
    Pause,
    Stop,
    Seek(u32),
    SetVolume(u16),
    RequestToken(oneshot::Sender<Option<Token>>),
    Preload(Playable),
    Shutdown,
}

pub struct Worker {
    events: EventManager,
    player_events: UnboundedReceiverStream<LibrespotPlayerEvent>,
    commands: UnboundedReceiverStream<WorkerCommand>,
    session: Session,
    player: Player,
    token_task: Pin<Box<dyn Future<Output = ()> + Send>>,
    active: bool,
    mixer: Box<dyn Mixer>,
}

impl Worker {
    pub(crate) fn new(
        events: EventManager,
        player_events: mpsc::UnboundedReceiver<LibrespotPlayerEvent>,
        commands: mpsc::UnboundedReceiver<WorkerCommand>,
        session: Session,
        player: Player,
        mixer: Box<dyn Mixer>,
    ) -> Worker {
        Worker {
            events,
            player_events: UnboundedReceiverStream::new(player_events),
            commands: UnboundedReceiverStream::new(commands),
            player,
            session,
            token_task: Box::pin(futures::future::pending()),
            active: false,
            mixer,
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        debug!("Worker thread is shutting down, stopping player");
        self.player.stop();
    }
}

impl Worker {
    fn get_token(
        &self,
        sender: oneshot::Sender<Option<Token>>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let scopes = "user-read-private,playlist-read-private,playlist-read-collaborative,playlist-modify-public,playlist-modify-private,user-follow-modify,user-follow-read,user-library-read,user-library-modify,user-top-read,user-read-recently-played";

        let fut_session = self.session.clone();

        return Box::pin(
            async move {
                let token_provider = fut_session.token_provider();
                let fut = token_provider.get_token(scopes);
                fut
                    .map(move |result| result.ok())
                    .map(move |result| sender.send(result).unwrap()).await
            }
        )
    }

    pub async fn run_loop(&mut self) {
        let mut ui_refresh = time::interval(Duration::from_millis(400));

        loop {
            if self.session.is_invalid() {
                info!("Librespot session invalidated, terminating worker");
                self.events.send(Event::Player(PlayerEvent::Stopped));
                break;
            }

            tokio::select! {
                cmd = self.commands.next() => match cmd {
                    Some(WorkerCommand::Load(playable, start_playing, position_ms)) => {
                        match SpotifyId::from_uri(&playable.uri()) {
                            Ok(id) => {
                                info!("player loading track: {:?}", id);
                                if id.item_type == SpotifyItemType::Unknown {
                                    warn!("track is not playable");
                                    self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                                } else {
                                    self.player.load(id, start_playing, position_ms);
                                }
                            }
                            Err(e) => {
                                error!("error parsing uri: {:?}", e);
                                self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                            }
                        }
                    }
                    Some(WorkerCommand::Play) => {
                        self.player.play();
                    }
                    Some(WorkerCommand::Pause) => {
                        self.player.pause();
                    }
                    Some(WorkerCommand::Stop) => {
                        self.player.stop();
                    }
                    Some(WorkerCommand::Seek(pos)) => {
                        self.player.seek(pos);
                    }
                    Some(WorkerCommand::SetVolume(volume)) => {
                        self.mixer.set_volume(volume);
                    }
                    Some(WorkerCommand::RequestToken(sender)) => {
                        self.token_task = self.get_token(sender);
                    }
                    Some(WorkerCommand::Preload(playable)) => {
                        if let Ok(id) = SpotifyId::from_uri(&playable.uri()) {
                            debug!("Preloading {:?}", id);
                            self.player.preload(id);
                        }
                    }
                    Some(WorkerCommand::Shutdown) => {
                        self.player.stop();
                        self.session.shutdown();
                    }
                    None => info!("empty stream")
                },
                event = self.player_events.next() => match event {
                    Some(LibrespotPlayerEvent::Playing {
                        play_request_id: _,
                        track_id: _,
                        position_ms,
                    }) => {
                        let position = Duration::from_millis(position_ms as u64);
                        let playback_start = SystemTime::now() - position;
                        self.events
                            .send(Event::Player(PlayerEvent::Playing(playback_start)));
                        self.active = true;
                    }
                    Some(LibrespotPlayerEvent::Seeked {
                        play_request_id: _,
                        track_id: _,
                        position_ms,
                    }) | Some(LibrespotPlayerEvent::PositionCorrection {
                        play_request_id: _,
                        track_id: _,
                        position_ms,
                    }) => {
                        let position = Duration::from_millis(position_ms as u64);
                        if self.active {
                            let playback_start = SystemTime::now() - position;
                            self.events
                                .send(Event::Player(PlayerEvent::Playing(playback_start)));
                        } else {
                            self.events
                                .send(Event::Player(PlayerEvent::Paused(position)));
                        }
                    }
                    Some(LibrespotPlayerEvent::Paused {
                        play_request_id: _,
                        track_id: _,
                        position_ms,
                    }) => {
                        let position = Duration::from_millis(position_ms as u64);
                        self.events
                            .send(Event::Player(PlayerEvent::Paused(position)));
                        self.active = false;
                    }
                    Some(LibrespotPlayerEvent::Stopped { .. }) => {
                        self.events.send(Event::Player(PlayerEvent::Stopped));
                        self.active = false;
                    }
                    Some(LibrespotPlayerEvent::EndOfTrack { .. }) => {
                        self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                    }
                    Some(LibrespotPlayerEvent::TimeToPreloadNextTrack { .. }) => {
                        self.events
                            .send(Event::Queue(QueueEvent::PreloadTrackRequest));
                    }
                    Some(LibrespotPlayerEvent::Loading { .. })
                        | Some(LibrespotPlayerEvent::Preloading { .. })
                        | Some(LibrespotPlayerEvent::Unavailable { .. })
                        | Some(LibrespotPlayerEvent::VolumeChanged { .. })
                        | Some(LibrespotPlayerEvent::TrackChanged { .. })
                        | Some(LibrespotPlayerEvent::SessionConnected { .. })
                        | Some(LibrespotPlayerEvent::SessionDisconnected { .. })
                        | Some(LibrespotPlayerEvent::SessionClientChanged { .. })
                        | Some(LibrespotPlayerEvent::ShuffleChanged { .. })
                        | Some(LibrespotPlayerEvent::RepeatChanged { .. })
                        | Some(LibrespotPlayerEvent::AutoPlayChanged { .. })
                        | Some(LibrespotPlayerEvent::FilterExplicitContentChanged { .. }) => {
                        },
                    None => {
                        warn!("Librespot player event channel died, terminating worker");
                        break
                    },
                },
                _ = ui_refresh.tick() => {
                    if self.active {
                        self.events.trigger();
                    }
                },
                _ = self.token_task.as_mut() => {
                    info!("token updated!");
                    self.token_task = Box::pin(futures::future::pending());
                }
            }
        }
    }
}
