//! wang-voice: the parts of talking to wang that don't need a model.
//!
//! - [`vad`]: decides when you've stopped talking (energy-based, calibrated to the room).
//! - [`wake`]: spots the assistant's name in a transcript and returns the command after it.
//! - [`text`]: cuts a streamed reply into sentences, so speech starts before the reply ends.
//! - [`assets`]: pinned, checksum-verified downloads for speech-to-text (whisper.cpp) and
//!   text-to-speech (Piper). Both run inside wang's jail; audio never leaves the laptop.

pub mod assets;
pub mod text;
pub mod vad;
pub mod wake;
