//! Shared media-player UI, built once and reused by the inspector preview, the
//! global bottom playback bar, and the dockable media panel. Every control acts
//! on the single global [`crate::services::audio_player::AudioPlayer`], so the
//! three surfaces stay in sync automatically.

pub mod bar;
pub mod media_panel;
pub mod transport;

pub use bar::MediaBar;
pub use media_panel::MediaPanel;
