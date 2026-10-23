//! Board variant, chosen at build time (`cargo xtask build --board ...`).
//!
//! `single`: one heating zone, relay on GPIO16. `dual` (feature `board-dual`):
//! two zones of one heater, relays on GPIO16 and GPIO17. Both have the status
//! LED on GPIO23. Images carry the board in their version string
//! (`...@dual`), and an update built for another board is refused.

#[cfg(not(feature = "board-dual"))]
pub const NAME: &str = "single";
#[cfg(feature = "board-dual")]
pub const NAME: &str = "dual";

/// Heating zones, one relay each.
#[cfg(not(feature = "board-dual"))]
pub const ZONES: usize = 1;
#[cfg(feature = "board-dual")]
pub const ZONES: usize = 2;

/// Make stored settings fit this board: sensors in zones it does not have go to
/// zone 1, and boards with several zones run their own heater (no heater group).
pub fn fit(mut s: thermostat_core::settings::Settings) -> thermostat_core::settings::Settings {
    for t in s.temp_sensors.iter_mut() {
        if t.zone as usize >= ZONES {
            t.zone = 0;
        }
    }
    if ZONES > 1 {
        s.heater_id.clear();
    }
    s
}
