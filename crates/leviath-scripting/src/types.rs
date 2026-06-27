//! Leviath types registered in Rhai.

use rhai::Engine;

/// Register Leviath types in the Rhai engine.
pub fn register_types(_engine: &mut Engine) {
    // For v0, we'll use basic Rhai types (String, Array, Map)
    // and add custom type registrations as needed.
    
    // TODO: Register Region, RegionEntry, ContentFormat when needed
}
