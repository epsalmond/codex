#![allow(clippy::expect_used)]

// Single integration test binary that aggregates all test modules.
// The submodules live in `tests/suite/`.
mod suite;

// Exercise the private production observer/replay against actual host outputs.
#[allow(dead_code)]
#[path = "../../ext/goal/src/stall.rs"]
mod stall;
#[allow(dead_code)]
#[path = "../../ext/goal/src/stall_observation.rs"]
mod stall_observation;
#[allow(dead_code)]
#[path = "../../ext/goal/src/stall_replay.rs"]
mod stall_replay;
#[allow(dead_code)]
#[path = "../../ext/goal/src/stall_settings.rs"]
mod stall_settings;
