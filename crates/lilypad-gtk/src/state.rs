//! The engine's state is the GTK app's state: detection, sessions and queues all live in
//! `lilypad_core::engine`, shared with the headless Gaming Mode engine.

pub use lilypad_core::engine::{EngineState as AppState, DEFAULT_API_URL};
