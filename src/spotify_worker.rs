use crate::events::{Event, EventManager};
use crate::model::playable::Playable;
use crate::queue::QueueEvent;
use crate::spotify::PlayerEvent;
use librespot_core::SpotifyUri;
use librespot_core::session::Session;
use librespot_playback::mixer::Mixer;
use librespot_playback::player::{Player, PlayerEvent as LibrespotPlayerEvent};
use log::{debug, error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tokio::sync::mpsc;
use tokio::time;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[derive(Debug)]
pub(crate) enum WorkerCommand {
    Load(Playable, bool, u32),
    Play,
    Pause,
    Stop,
    Seek(u32),
    SetVolume(u16),
    Preload(Playable),
    Shutdown,
}

enum PlayerStatus {
    Playing,
    Paused,
    Stopped,
}

pub struct Worker {
    events: EventManager,
    player_events: UnboundedReceiverStream<LibrespotPlayerEvent>,
    commands: mpsc::UnboundedReceiver<WorkerCommand>,
    session: Session,
    player: Arc<Player>,
    player_status: PlayerStatus,
    mixer: Arc<dyn Mixer>,
    /// The play_request_id librespot assigned to our most recent player.load() call.
    /// Used to drop stale EndOfTrack events that arrive after a manual skip.
    current_play_request_id: Option<u64>,
}

impl Worker {
    pub(crate) fn new(
        events: EventManager,
        player_events: mpsc::UnboundedReceiver<LibrespotPlayerEvent>,
        commands: mpsc::UnboundedReceiver<WorkerCommand>,
        session: Session,
        player: Arc<Player>,
        mixer: Arc<dyn Mixer>,
    ) -> Self {
        Self {
            events,
            player_events: UnboundedReceiverStream::new(player_events),
            commands,
            player,
            session,
            player_status: PlayerStatus::Stopped,
            mixer,
            current_play_request_id: None,
        }
    }

    fn handle_command(&mut self, cmd: WorkerCommand) {
        match cmd {
            WorkerCommand::Load(..) => unreachable!("Load handled by run_loop"),
            WorkerCommand::Play => self.player.play(),
            WorkerCommand::Pause => self.player.pause(),
            WorkerCommand::Stop => self.player.stop(),
            WorkerCommand::Seek(pos) => self.player.seek(pos),
            WorkerCommand::SetVolume(volume) => self.mixer.set_volume(volume),
            WorkerCommand::Preload(playable) => {
                if let Ok(uri) = SpotifyUri::from_uri(&playable.uri()) {
                    debug!("Preloading {uri:?}");
                    self.player.preload(uri);
                }
            }
            WorkerCommand::Shutdown => {
                self.player.stop();
                self.session.shutdown();
            }
        }
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
                cmd = self.commands.recv() => match cmd {
                    Some(WorkerCommand::Load(mut playable, mut start_playing, mut position_ms)) => {
                        // Coalesce consecutive Load commands queued before this one is processed.
                        // Rapid next/prev clicks can stack multiple Loads; calling player.load()
                        // twice before librespot acknowledges the first one crashes the player task.
                        loop {
                            match self.commands.try_recv() {
                                Ok(WorkerCommand::Load(p, s, pos)) => {
                                    playable = p;
                                    start_playing = s;
                                    position_ms = pos;
                                }
                                Ok(other) => {
                                    // Non-load command between rapid skips — execute it, then stop draining.
                                    self.handle_command(other);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        match SpotifyUri::from_uri(&playable.uri()) {
                            Ok(uri) => {
                                info!("player loading track: {uri:?}");
                                if !uri.is_playable() {
                                    warn!("track is not playable");
                                    self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                                } else {
                                    self.player.load(uri, start_playing, position_ms);
                                }
                            }
                            Err(e) => {
                                error!("error parsing uri: {e:?}");
                                self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                            }
                        }
                    }
                    Some(other) => self.handle_command(other),
                    None => info!("command channel closed"),
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
                        self.player_status = PlayerStatus::Playing;
                    }
                    Some(LibrespotPlayerEvent::Paused {
                        play_request_id: _,
                        track_id: _,
                        position_ms,
                    }) => {
                        let position = Duration::from_millis(position_ms as u64);
                        self.events
                            .send(Event::Player(PlayerEvent::Paused(position)));
                        self.player_status = PlayerStatus::Paused;
                    }
                    Some(LibrespotPlayerEvent::Stopped { .. }) => {
                        self.events.send(Event::Player(PlayerEvent::Stopped));
                        self.player_status = PlayerStatus::Stopped;
                    }
                    Some(LibrespotPlayerEvent::PlayRequestIdChanged { play_request_id }) => {
                        self.current_play_request_id = Some(play_request_id);
                    }
                    Some(LibrespotPlayerEvent::EndOfTrack { play_request_id, .. }) => {
                        // Ignore stale EndOfTrack events from tracks we manually skipped.
                        // After player.load() the play_request_id changes; an EndOfTrack
                        // with the old id means the previous track naturally ran out after
                        // we had already moved on, which would cause a double-load crash.
                        if self.current_play_request_id == Some(play_request_id) {
                            self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                        } else {
                            debug!("Ignoring stale EndOfTrack (id {play_request_id}, current {:?})", self.current_play_request_id);
                        }
                    }
                    Some(LibrespotPlayerEvent::TimeToPreloadNextTrack { play_request_id, .. }) => {
                        if self.current_play_request_id == Some(play_request_id) {
                            self.events
                                .send(Event::Queue(QueueEvent::PreloadTrackRequest));
                        }
                    }
                    Some(LibrespotPlayerEvent::Seeked { play_request_id: _, track_id: _, position_ms}) => {
                        let position = Duration::from_millis(position_ms as u64);
                        let event = match self.player_status {
                            PlayerStatus::Playing => {
                                let playback_start = SystemTime::now() - position;
                                PlayerEvent::Playing(playback_start)
                            },
                            PlayerStatus::Paused => PlayerEvent::Paused(position),
                            PlayerStatus::Stopped => PlayerEvent::Stopped,
                        };
                        self.events.send(Event::Player(event));
                    }
                    Some(event) => {
                        debug!("Unhandled player event: {event:?}");
                    }
                    None => {
                        warn!("Librespot player event channel died, terminating worker");
                        break
                    },
                },
                // Update animated parts of the UI (e.g. the progress bar). Only
                // while actually playing — when paused or stopped the progress
                // is static, so periodic redraws would just burn CPU.
                _ = ui_refresh.tick() => {
                    if matches!(self.player_status, PlayerStatus::Playing) {
                        self.events.trigger();
                    }
                },
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        debug!("Worker thread is shutting down, stopping player");
        self.player.stop();
    }
}
