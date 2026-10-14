//! Pause media around dictation over MPRIS.
//!
//! The app's "pause music while dictating" setting (`prefs.user.shouldPauseAudio`)
//! is carried out by the helper, not the app: on `DictationStart` the macOS
//! helper pauses whatever is playing and on `DictationStop` resumes only what it
//! paused. It also reports playback state with `IsMediaPlayingUpdate` (the app
//! holds back attention nudges while media plays). This is the Linux counterpart
//! over MPRIS, the D-Bus interface desktop media players and browsers implement.
//!
//! All D-Bus traffic stays off the dispatch thread: a worker applies
//! `DictationStart`/`DictationStop` in order, and a watcher re-reports playback
//! state whenever a player's properties change or a player comes or goes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type;
use zbus::zvariant::OwnedValue;
use zbus::MatchRule;

use crate::backend::EventSink;

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
/// playerctld re-exports whichever player is active; acting on it as well would
/// pause or resume that player twice.
const PLAYERCTLD: &str = "org.mpris.MediaPlayer2.playerctld";
/// Coalesce a burst of player signals (track change = several PropertiesChanged)
/// into one state check.
const SETTLE: Duration = Duration::from_millis(150);

enum Command {
    DictationStart,
    DictationStop,
}

/// Handle to the MPRIS worker; cheap to call from the dispatch thread.
pub struct MediaController {
    tx: mpsc::Sender<Command>,
}

impl MediaController {
    /// Connect to the session bus and spawn the dictation worker and the
    /// playback-state watcher. `Err` when the session bus is unreachable.
    pub fn start(events: EventSink) -> Result<MediaController, String> {
        let conn = Connection::session().map_err(|e| format!("session bus: {e}"))?;
        let (tx, rx) = mpsc::channel();

        let worker_conn = conn.clone();
        std::thread::Builder::new()
            .name("mpris-dictation".into())
            .spawn(move || dictation_worker(&worker_conn, rx))
            .map_err(|e| format!("spawn worker: {e}"))?;
        std::thread::Builder::new()
            .name("mpris-watch".into())
            .spawn(move || watch_playback(&conn, &events))
            .map_err(|e| format!("spawn watcher: {e}"))?;

        log::info!("media: MPRIS controller started");
        Ok(MediaController { tx })
    }

    pub fn on_dictation_start(&self) {
        let _ = self.tx.send(Command::DictationStart);
    }

    pub fn on_dictation_stop(&self) {
        let _ = self.tx.send(Command::DictationStop);
    }
}

/// Pause playing players on `DictationStart` (when the user enabled it) and
/// resume exactly those on `DictationStop`.
fn dictation_worker(conn: &Connection, rx: mpsc::Receiver<Command>) {
    let mut paused_by_us: Vec<String> = Vec::new();
    for cmd in rx {
        match cmd {
            Command::DictationStart => {
                if crate::prefs::user_bool("shouldPauseAudio") != Some(true) {
                    continue;
                }
                for name in player_names(conn) {
                    let Some(status) = player_status(conn, &name) else {
                        continue;
                    };
                    if !(status.playing && status.can_pause) || paused_by_us.contains(&name) {
                        continue;
                    }
                    match call_player(conn, &name, "Pause") {
                        Ok(()) => {
                            log::info!("media: paused {name} for dictation");
                            paused_by_us.push(name);
                        }
                        Err(e) => log::warn!("media: pausing {name} failed: {e}"),
                    }
                }
            }
            Command::DictationStop => {
                for name in paused_by_us.drain(..) {
                    // Leave alone a player the user resumed, stopped or closed
                    // while dictating; only undo our own pause.
                    let still_paused = player_status(conn, &name).is_some_and(|s| s.paused);
                    if !still_paused {
                        continue;
                    }
                    match call_player(conn, &name, "Play") {
                        Ok(()) => log::info!("media: resumed {name} after dictation"),
                        Err(e) => log::warn!("media: resuming {name} failed: {e}"),
                    }
                }
            }
        }
    }
}

/// Report the aggregate playback state now and again after every MPRIS change.
fn watch_playback(conn: &Connection, events: &EventSink) {
    let (dirty_tx, dirty_rx) = mpsc::channel::<()>();
    let rules = [
        // A player's PlaybackStatus/CanPause changed.
        MatchRule::builder()
            .msg_type(Type::Signal)
            .interface("org.freedesktop.DBus.Properties")
            .and_then(|b| b.member("PropertiesChanged"))
            .and_then(|b| b.path(MPRIS_PATH))
            .map(|b| b.build()),
        // A player appeared or went away.
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender("org.freedesktop.DBus")
            .and_then(|b| b.interface("org.freedesktop.DBus"))
            .and_then(|b| b.member("NameOwnerChanged"))
            .and_then(|b| b.arg0ns("org.mpris.MediaPlayer2"))
            .map(|b| b.build()),
    ];
    for rule in rules {
        let rule = match rule {
            Ok(r) => r,
            Err(e) => {
                log::warn!("media: bad match rule ({e}); playback updates disabled");
                return;
            }
        };
        let signals = match MessageIterator::for_match_rule(rule, conn, None) {
            Ok(it) => it,
            Err(e) => {
                log::warn!("media: subscribing to MPRIS signals failed: {e}");
                return;
            }
        };
        let dirty_tx = dirty_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("mpris-signals".into())
            .spawn(move || {
                for _ in signals {
                    if dirty_tx.send(()).is_err() {
                        break;
                    }
                }
            });
        if let Err(e) = spawned {
            log::warn!("media: spawn signal reader failed: {e}");
            return;
        }
    }
    drop(dirty_tx);

    let pid = std::process::id();
    let counter = AtomicU64::new(0);
    let mut last: Option<PlaybackState> = None;
    loop {
        let state = playback_state(conn);
        if last != Some(state) {
            let n = counter.fetch_add(1, Ordering::Relaxed);
            let uuid = format!("media-{pid}-{n}");
            let _ = events.send(crate::proto::request(
                "IsMediaPlayingUpdate",
                state.payload(),
                &uuid,
            ));
            last = Some(state);
        }
        if dirty_rx.recv().is_err() {
            break;
        }
        while dirty_rx.recv_timeout(SETTLE).is_ok() {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlaybackState {
    playing: bool,
    pausable: bool,
}

impl PlaybackState {
    fn from_players(players: &[PlayerStatus]) -> PlaybackState {
        PlaybackState {
            playing: players.iter().any(|p| p.playing),
            pausable: players.iter().any(|p| p.playing && p.can_pause),
        }
    }

    /// `IsMediaPlayingUpdate` body.
    fn payload(self) -> Value {
        json!({ "payload": {
            "isMediaPlaying": self.playing,
            "isMediaPausable": self.pausable,
        } })
    }
}

fn playback_state(conn: &Connection) -> PlaybackState {
    let players: Vec<PlayerStatus> = player_names(conn)
        .iter()
        .filter_map(|name| player_status(conn, name))
        .collect();
    PlaybackState::from_players(&players)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlayerStatus {
    playing: bool,
    paused: bool,
    can_pause: bool,
}

/// Bus names of the MPRIS players on the session bus.
fn player_names(conn: &Connection) -> Vec<String> {
    let names = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "ListNames",
            &(),
        )
        .and_then(|reply| reply.body().deserialize::<Vec<String>>());
    match names {
        Ok(names) => names.into_iter().filter(|n| is_player(n)).collect(),
        Err(e) => {
            log::debug!("media: ListNames failed: {e}");
            Vec::new()
        }
    }
}

fn is_player(name: &str) -> bool {
    name.starts_with(MPRIS_PREFIX) && name != PLAYERCTLD
}

/// `PlaybackStatus` and `CanPause` of one player, or `None` if it did not answer.
fn player_status(conn: &Connection, name: &str) -> Option<PlayerStatus> {
    let reply = conn
        .call_method(
            Some(name),
            MPRIS_PATH,
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &(PLAYER_IFACE,),
        )
        .ok()?;
    let props: HashMap<String, OwnedValue> = reply.body().deserialize().ok()?;
    let status = props.get("PlaybackStatus")?.downcast_ref::<&str>().ok()?;
    let can_pause = props
        .get("CanPause")
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false);
    Some(PlayerStatus {
        playing: status == "Playing",
        paused: status == "Paused",
        can_pause,
    })
}

fn call_player(conn: &Connection, name: &str, method: &str) -> Result<(), String> {
    conn.call_method(Some(name), MPRIS_PATH, Some(PLAYER_IFACE), method, &())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYING: PlayerStatus = PlayerStatus {
        playing: true,
        paused: false,
        can_pause: true,
    };
    const PAUSED: PlayerStatus = PlayerStatus {
        playing: false,
        paused: true,
        can_pause: true,
    };

    #[test]
    fn is_player_skips_non_mpris_names_and_playerctld() {
        assert!(is_player("org.mpris.MediaPlayer2.spotify"));
        assert!(is_player("org.mpris.MediaPlayer2.brave.instance1234"));
        assert!(!is_player("org.mpris.MediaPlayer2.playerctld"));
        assert!(!is_player("org.freedesktop.Notifications"));
    }

    #[test]
    fn playback_state_aggregates_players() {
        assert_eq!(
            PlaybackState::from_players(&[PAUSED, PLAYING]),
            PlaybackState {
                playing: true,
                pausable: true
            }
        );
        let unpausable = PlayerStatus {
            can_pause: false,
            ..PLAYING
        };
        assert_eq!(
            PlaybackState::from_players(&[unpausable]),
            PlaybackState {
                playing: true,
                pausable: false
            }
        );
        assert_eq!(
            PlaybackState::from_players(&[]),
            PlaybackState {
                playing: false,
                pausable: false
            }
        );
    }

    // `IsMediaPlayingUpdate` is decoded by the app's quicktype validator.
    #[test]
    fn payload_matches_is_media_playing_update() {
        let body = PlaybackState {
            playing: true,
            pausable: false,
        }
        .payload();
        assert_eq!(body["payload"]["isMediaPlaying"], true);
        assert_eq!(body["payload"]["isMediaPausable"], false);
    }
}
