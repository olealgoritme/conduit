//! The GameStream protocol, host side (what Moonlight clients talk to).
//! Layout and flow: docs/STREAMING.md.

pub mod audio;
pub mod control;
pub mod http;
pub mod input;
pub mod nvhttp;
pub mod pairing;
pub mod rtsp;
pub mod udp;
pub mod video;
