//! oam_core: event loop, op system, Promise<->Future bridge.
//!
//! M1 workstream. At M0 this crate only anchors the dependency graph
//! (everything above the engine speaks oam types, starting with ODIF).

pub use oam_diagnostics as diagnostics;
