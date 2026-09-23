//! The AirPlay speaker, in a build made without the `airplay` feature: there is
//! none. The config refuses `[airplay]` in such a build — and a Mac's `audio` key
//! in every build — so nothing reaches [`AirPlay::start`], and the type has no
//! values for a session to be handed.

use std::path::Path;
use std::sync::Arc;

use crate::audio::AudioBridge;
use crate::config::AirPlayConfig;

/// No speaker: uninhabited, so every `Option<Arc<AirPlay>>` is `None`.
pub enum AirPlay {}

impl AirPlay {
    pub fn start(_config: &AirPlayConfig, _config_path: &Path) -> anyhow::Result<Arc<Self>> {
        anyhow::bail!("this remotex was built without the airplay feature")
    }

    pub fn attach(&self, _bridge: &Arc<AudioBridge>) -> Attached {
        match *self {}
    }
}

/// Never made, since there is no speaker to attach to: a struct, not an empty
/// enum, so the session's code that stores one is not unreachable.
pub struct Attached(());
