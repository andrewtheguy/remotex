//! The AirPlay speaker, in a build made without the `airplay` feature: there is
//! none. The config refuses `[airplay]` and a Mac's `audio` in such a build, so
//! nothing reaches [`AirPlay::start`], and the type has no values for a session
//! to be handed.

use std::sync::Arc;

use crate::audio::AudioBridge;
use crate::config::AirPlayConfig;

/// No speaker: uninhabited, so every `Option<Arc<AirPlay>>` is `None`.
pub enum AirPlay {}

impl AirPlay {
    pub fn start(_config: &AirPlayConfig) -> anyhow::Result<Arc<Self>> {
        anyhow::bail!("this remotex was built without the airplay feature")
    }

    pub fn attach(&self, _bridge: &Arc<AudioBridge>) {
        match *self {}
    }
}
