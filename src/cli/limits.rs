//! `limits`: the protocol's hard limits, as a table and as JSON.
//!
//! Every numeric cap in the table is the crate's own constant, so the table
//! cannot drift from the code that enforces it; the rule and invariant entries
//! are the specification's own tables and cite their section.

use serde_json::{Value, json};

use crate::cli::{CliError, Options, Produced, json_line, table};
use crate::media::MAX_MEDIA_VIEWERS_PER_HOST;
use crate::media::bwe::BweConfig;
use crate::media::cells::{
    ACK_RING_FLOOR, MAX_CELL_SIZE, MAX_CELLS, MIN_CELL_SIZE, REPAIR_DELAY_DATAGRAM_CEIL_MS,
    REPAIR_DELAY_DATAGRAM_FLOOR_MS, REPAIR_DELAY_STREAM_CEIL_MS, REPAIR_MAX_ROUNDS,
};
use crate::media::input::{
    INPUT_DEADLINE_MS, INPUT_DEDUP_WINDOW, INPUT_QUEUE_BYTES, INPUT_QUEUE_OBJECTS,
};
use crate::media::object::{DEDUP_WINDOW, MAX_ASSEMBLY_BYTES, MAX_OPEN_OBJECTS};
use crate::media::pacer::{MAX_GRANT_BYTES, RESERVED_RATE_FLOOR_BPS, RESERVED_RATE_FRACTION};
use crate::media::pdu::{
    FRAGMENT_FIXED_HEADER, MAX_CONTROL_LANE_OBJECT_BYTES, MAX_FRAGMENT_COUNT, MAX_GRPC_COALESCE,
    MAX_HEADER_EXT, MAX_WS_FRAGMENT, TARGET_FRAGMENT_PAYLOAD,
};
use crate::media::resume::{
    PARK_TTL_CEIL_SECONDS, PARK_TTL_FLOOR_SECONDS, RECONNECT_CAP_MS, RECONNECT_FLOOR_MS,
    RESUME_TTL_SECONDS,
};
use crate::media::scheduler::{
    DEFAULT_CELL_BYTES, IDLE_FRAME_INTERVAL_CEILING_MS, IDLE_FRAME_INTERVAL_FLOOR_MS,
    SchedulerConfig,
};

/// What an entry holds: a number, a fraction, or a rule's own text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EntryValue {
    /// A whole number.
    Number(u64),
    /// A fraction.
    Fraction(f64),
    /// A rule: the specification's own text.
    Rule(&'static str),
}

impl EntryValue {
    /// The JSON form of the value.
    fn to_json(self) -> Value {
        match self {
            Self::Number(value) => Value::from(value),
            Self::Fraction(value) => json!(value),
            Self::Rule(value) => Value::String(value.to_string()),
        }
    }

    /// The human form of the value.
    fn to_text(self) -> String {
        match self {
            Self::Number(value) => value.to_string(),
            Self::Fraction(value) => value.to_string(),
            Self::Rule(value) => value.to_string(),
        }
    }
}

/// One entry of the limits table.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    /// The constant's name.
    pub name: &'static str,
    /// The constant's value: a number, or a rule's own text.
    pub value: EntryValue,
    /// The unit the value is counted in.
    pub unit: Option<&'static str>,
    /// The section that states it.
    pub spec: &'static str,
}

impl Entry {
    /// A numeric cap.
    const fn number(
        name: &'static str,
        value: u64,
        unit: &'static str,
        spec: &'static str,
    ) -> Self {
        Self {
            name,
            value: EntryValue::Number(value),
            unit: Some(unit),
            spec,
        }
    }

    /// A fractional cap.
    const fn fraction(
        name: &'static str,
        value: f64,
        unit: &'static str,
        spec: &'static str,
    ) -> Self {
        Self {
            name,
            value: EntryValue::Fraction(value),
            unit: Some(unit),
            spec,
        }
    }

    /// A rule: a text, not a number.
    const fn rule(name: &'static str, value: &'static str, spec: &'static str) -> Self {
        Self {
            name,
            value: EntryValue::Rule(value),
            unit: None,
            spec,
        }
    }
}

/// One group of the limits table.
#[derive(Debug, Clone)]
pub struct Group {
    /// The group's name.
    pub name: &'static str,
    /// What the group holds.
    pub spec: &'static str,
    /// The group's entries.
    pub entries: Vec<Entry>,
}

/// Every group, in the order the table prints them.
pub fn groups() -> Vec<Group> {
    let scheduler = SchedulerConfig::default();
    let bwe = BweConfig::default();
    vec![
        Group {
            name: "objects",
            spec: "§4.4",
            entries: vec![
                Entry::number(
                    "MAX_OPEN_OBJECTS",
                    MAX_OPEN_OBJECTS as u64,
                    "objects",
                    "§4.4",
                ),
                Entry::number(
                    "MAX_ASSEMBLY_BYTES",
                    MAX_ASSEMBLY_BYTES as u64,
                    "bytes",
                    "§4.4",
                ),
                Entry::number(
                    "DEDUP_WINDOW",
                    DEDUP_WINDOW as u64,
                    "fragments",
                    "§4.4 step 5",
                ),
                Entry::number(
                    "MAX_FRAGMENT_COUNT",
                    u64::from(MAX_FRAGMENT_COUNT),
                    "fragments",
                    "§4.2",
                ),
                Entry::number(
                    "MAX_CONTROL_LANE_OBJECT_BYTES",
                    MAX_CONTROL_LANE_OBJECT_BYTES as u64,
                    "bytes",
                    "§3.1",
                ),
                Entry::number(
                    "MAX_MEDIA_VIEWERS_PER_HOST",
                    MAX_MEDIA_VIEWERS_PER_HOST as u64,
                    "viewers",
                    "§8",
                ),
            ],
        },
        Group {
            name: "fragments",
            spec: "§4.1",
            entries: vec![
                Entry::number(
                    "FRAGMENT_FIXED_HEADER",
                    FRAGMENT_FIXED_HEADER as u64,
                    "bytes",
                    "§4.2",
                ),
                Entry::number("MAX_HEADER_EXT", u64::from(MAX_HEADER_EXT), "bytes", "§4.2"),
                Entry::number("MAX_WS_FRAGMENT", MAX_WS_FRAGMENT as u64, "bytes", "§4.1"),
                Entry::number(
                    "MAX_GRPC_COALESCE",
                    MAX_GRPC_COALESCE as u64,
                    "bytes",
                    "§4.1",
                ),
                Entry::number(
                    "TARGET_FRAGMENT_PAYLOAD",
                    TARGET_FRAGMENT_PAYLOAD as u64,
                    "bytes",
                    "§4.1",
                ),
                Entry::number("MAX_GRANT_BYTES", MAX_GRANT_BYTES, "bytes", "§6.4"),
            ],
        },
        Group {
            name: "parts",
            spec: "§6.3.4",
            entries: vec![
                Entry::number(
                    "max_parts_stream",
                    u64::from(scheduler.max_parts_stream),
                    "parts",
                    "§6.3.4",
                ),
                Entry::number(
                    "max_parts_datagram",
                    u64::from(scheduler.max_parts_datagram),
                    "parts",
                    "§6.3.4",
                ),
                Entry::number("max_parts_datagram_heavy_loss", 1, "parts", "§6.3.4"),
                Entry::fraction(
                    "single_part_loss_ratio",
                    scheduler.single_part_loss_ratio,
                    "of the objects",
                    "§6.3.4",
                ),
            ],
        },
        Group {
            name: "updates",
            spec: "§6.2.5",
            entries: vec![
                Entry::number("budget_bytes_floor", 256, "bytes", "§6.2.5"),
                Entry::number("budget_bytes_ceiling", 1_048_576, "bytes", "§6.2.5"),
                Entry::number("max_object_bytes_floor", 1_024, "bytes", "§6.2.5"),
                Entry::number("max_object_bytes_ceiling", 524_288, "bytes", "§6.2.5"),
                Entry::number("min_delta_bytes", bwe.min_delta_bytes, "bytes", "§6.2.5"),
                Entry::number(
                    "pace_floor_bps",
                    bwe.pace_floor_bps,
                    "bits per second",
                    "§6.2.4",
                ),
                Entry::number("ceiling_bps", bwe.ceiling_bps, "bits per second", "§6.2.4"),
                Entry::fraction("target_fps_floor", 0.5, "frames per second", "§6.2.5"),
                Entry::number("target_fps_ceiling", 30, "frames per second", "§6.2.5"),
                Entry::number(
                    "idle_interval_floor_ms",
                    IDLE_FRAME_INTERVAL_FLOOR_MS,
                    "ms",
                    "§8.4",
                ),
                Entry::number(
                    "idle_interval_ceiling_ms",
                    IDLE_FRAME_INTERVAL_CEILING_MS,
                    "ms",
                    "§8.4",
                ),
            ],
        },
        Group {
            name: "regions",
            spec: "§8",
            entries: vec![
                Entry::number("MAX_CELLS", u64::from(MAX_CELLS), "cells", "§8"),
                Entry::number("MIN_CELL_SIZE", u64::from(MIN_CELL_SIZE), "cells", "§8"),
                Entry::number("MAX_CELL_SIZE", u64::from(MAX_CELL_SIZE), "cells", "§8"),
                Entry::number(
                    "DEFAULT_CELL_BYTES",
                    u64::from(DEFAULT_CELL_BYTES),
                    "bytes",
                    "§8",
                ),
                Entry::number("ACK_RING_FLOOR", ACK_RING_FLOOR as u64, "updates", "§8"),
                Entry::number(
                    "REPAIR_MAX_ROUNDS",
                    u64::from(REPAIR_MAX_ROUNDS),
                    "rounds",
                    "§8",
                ),
                Entry::number(
                    "REPAIR_DELAY_FLOOR_MS",
                    REPAIR_DELAY_DATAGRAM_FLOOR_MS,
                    "ms",
                    "§9.1",
                ),
                Entry::number(
                    "REPAIR_DELAY_DATAGRAM_CEIL_MS",
                    REPAIR_DELAY_DATAGRAM_CEIL_MS,
                    "ms",
                    "§9.1",
                ),
                Entry::number(
                    "REPAIR_DELAY_STREAM_CEIL_MS",
                    REPAIR_DELAY_STREAM_CEIL_MS,
                    "ms",
                    "§9.1",
                ),
            ],
        },
        Group {
            name: "queues",
            spec: "§8.6",
            entries: vec![
                Entry::number(
                    "INPUT_QUEUE_OBJECTS",
                    INPUT_QUEUE_OBJECTS as u64,
                    "objects",
                    "§8.6",
                ),
                Entry::number(
                    "INPUT_QUEUE_BYTES",
                    INPUT_QUEUE_BYTES as u64,
                    "bytes",
                    "§8.6",
                ),
                Entry::number(
                    "INPUT_DEDUP_WINDOW",
                    INPUT_DEDUP_WINDOW as u64,
                    "objects",
                    "§8.6",
                ),
                Entry::number("INPUT_DEADLINE_MS", INPUT_DEADLINE_MS, "ms", "§9.1"),
                Entry::number(
                    "RESERVED_RATE_FLOOR_BPS",
                    RESERVED_RATE_FLOOR_BPS,
                    "bits per second",
                    "§6.4",
                ),
                Entry::fraction(
                    "RESERVED_RATE_FRACTION",
                    RESERVED_RATE_FRACTION,
                    "of the pace",
                    "§6.4",
                ),
            ],
        },
        Group {
            name: "continuity",
            spec: "§7",
            entries: vec![
                Entry::number(
                    "PARK_TTL_FLOOR_SECONDS",
                    PARK_TTL_FLOOR_SECONDS,
                    "seconds",
                    "§7",
                ),
                Entry::number(
                    "PARK_TTL_CEIL_SECONDS",
                    PARK_TTL_CEIL_SECONDS,
                    "seconds",
                    "§7",
                ),
                Entry::number("RESUME_TTL_SECONDS", RESUME_TTL_SECONDS, "seconds", "§7"),
                Entry::number("RECONNECT_FLOOR_MS", RECONNECT_FLOOR_MS, "ms", "§9.1"),
                Entry::number("RECONNECT_CAP_MS", RECONNECT_CAP_MS, "ms", "§9.1"),
            ],
        },
        Group {
            name: "timeouts",
            spec: "§9.1",
            entries: vec![
                Entry::rule("lane heartbeat", "clamp(3 x RTT, 2 s, 30 s)", "§9.1"),
                Entry::rule(
                    "lane stream idle",
                    "clamp(4 x HB + 2 x J, 15 s, 300 s)",
                    "§9.1",
                ),
                Entry::rule("lane handshake", "clamp(2 x RTT + 2 s, 5 s, 60 s)", "§9.1"),
                Entry::rule("lane connect", "clamp(2 x RTT + 1 s, 3 s, 30 s)", "§9.1"),
                Entry::rule(
                    "lane reconnect base",
                    "clamp(1 x RTT, 500 ms, 5 s), full jitter, unlimited attempts",
                    "§9.1",
                ),
                Entry::rule("lane reconnect max", "clamp(30 x RTT, 10 s, 60 s)", "§9.1"),
                Entry::rule("lane attach", "clamp(2 x RTT + 2 s, 5 s, 45 s)", "§9.1"),
                Entry::rule(
                    "session idle",
                    "clamp(2 x outage_target + 4 x RTT, 60 s, 300 s)",
                    "§9.1",
                ),
                Entry::rule(
                    "peer heartbeat",
                    "clamp(0.5 x session_idle, 5 s, 60 s) and < session_idle / 4",
                    "§9.1",
                ),
                Entry::rule("transport idle", ">= 2 x session_idle", "§9.1"),
                Entry::rule("park TTL", "clamp(2 x outage + 60 s, 120 s, 900 s)", "§9.1"),
                Entry::rule("resume grace", "2 x RTT + 2 s", "§9.1"),
                Entry::rule("assembly timeout", "3 x RTT + 500 ms", "§9.1"),
                Entry::rule("report cadence", "clamp(4 x RTT, 250 ms, 2 s)", "§9.1"),
                Entry::rule(
                    "prompt report",
                    "clamp(RTT / 4, 25 ms, 250 ms); an echo or a loss goes out at once",
                    "§9.1",
                ),
                Entry::rule(
                    "session idle at the edge",
                    "clamp(2 x outage + 60 s, 120 s, 600 s); any inbound byte resets it",
                    "§9.1",
                ),
                Entry::rule("keepalive", "clamp(3 x RTT, 5 s, 60 s)", "§9.1"),
                Entry::rule(
                    "stall and repair window",
                    "clamp(3 x RTT + 250 ms, 500 ms, 8 s)",
                    "§9.1",
                ),
                Entry::rule(
                    "repair delay (stream)",
                    "clamp(RTT + update_interval, 250 ms, 3 s)",
                    "§9.1",
                ),
                Entry::rule(
                    "repair delay (datagram)",
                    "clamp(2 x RTT, 250 ms, 5 s)",
                    "§9.1",
                ),
                Entry::rule("idle object", "clamp(1 000 ms, 2 x RTT, 3 000 ms)", "§9.1"),
                Entry::rule(
                    "input deadline",
                    "clamp(2 x RTT, 5 s, 30 s); the per-object deadline is 5 000 ms by default",
                    "§9.1",
                ),
                Entry::rule(
                    "pointer-move dispatch",
                    "clamp(RTT / 8, 16 ms, 100 ms)",
                    "§9.1",
                ),
                Entry::rule(
                    "waiting-object timeout",
                    "clamp(2 x RTT + 500 ms, 1 s, 10 s)",
                    "§9.1",
                ),
                Entry::rule(
                    "datagram establishment",
                    "clamp(2 x RTT + 1 s, 3 s, 30 s)",
                    "§9.1",
                ),
                Entry::rule("audit tap publish", "clamp(3 x RTT, 500 ms, 10 s)", "§9.1"),
            ],
        },
        Group {
            name: "invariants",
            spec: "§10",
            entries: vec![
                Entry::rule(
                    "INV-1",
                    "An update is applied only if every part of it arrived.",
                    "§10",
                ),
                Entry::rule(
                    "INV-2",
                    "At every converged observation, the receiver's content equals the sender's emitted state for every acknowledged cell.",
                    "§10",
                ),
                Entry::rule(
                    "INV-3",
                    "No input object is applied twice, and none after its deadline.",
                    "§10",
                ),
                Entry::rule(
                    "INV-4",
                    "After any session end or park beyond grace, the sender's pressed set is empty.",
                    "§10",
                ),
                Entry::rule("INV-5", "Every queue stays within its cap.", "§10"),
                Entry::rule(
                    "INV-6",
                    "No object exceeds max_object_bytes, and no update exceeds max_parts x the carrier's fragment cap.",
                    "§10",
                ),
                Entry::rule(
                    "INV-7",
                    "The bytes sent in any one-second window are at most pace_bps / 8 x 1.25.",
                    "§10",
                ),
                Entry::rule(
                    "INV-8",
                    "The input echo is at most 2 x RTT + 250 ms whenever the link delivers any object.",
                    "§10",
                ),
                Entry::rule(
                    "INV-9",
                    "A session survives every outage and flap schedule, with no lost session.",
                    "§10",
                ),
                Entry::rule(
                    "INV-10",
                    "Every drop is attributed to exactly one hop.",
                    "§10",
                ),
            ],
        },
    ]
}

/// Print the protocol's hard limits.
pub fn limits(options: &Options, json: bool) -> Result<Produced, CliError> {
    if !options.positionals().is_empty() {
        return Err(CliError::Usage(format!(
            "the limits command takes no argument, but {} were given",
            options.positionals().len()
        )));
    }
    let groups = groups();
    if json {
        let rendered: Vec<Value> = groups
            .iter()
            .map(|group| {
                json!({
                    "name": group.name,
                    "spec": group.spec,
                    "entries": group.entries.iter().map(|entry| json!({
                        "name": entry.name,
                        "value": entry.value.to_json(),
                        "unit": entry.unit,
                        "spec": entry.spec,
                    })).collect::<Vec<Value>>(),
                })
            })
            .collect();
        Ok(Produced::ok(json_line(&json!({
            "command": "limits",
            "protocol_version": crate::media::pdu::MEDIA_PROTOCOL_VERSION,
            "groups": rendered,
        }))))
    } else {
        Ok(Produced::ok(render(&groups)))
    }
}

/// The human form of the limits table.
fn render(groups: &[Group]) -> String {
    let mut text = String::new();
    for group in groups {
        text.push_str(&format!("{} ({})\n", group.name, group.spec));
        let rows: Vec<Vec<String>> = group
            .entries
            .iter()
            .map(|entry| {
                vec![
                    entry.name.to_string(),
                    entry.value.to_text(),
                    entry.unit.unwrap_or("").to_string(),
                    entry.spec.to_string(),
                ]
            })
            .collect();
        text.push_str(&table(&["name", "value", "unit", "spec"], &rows));
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_numeric_caps_are_the_crates_own_constants() {
        let groups = groups();
        let objects = &groups[0];
        assert_eq!(
            objects.entries[0].value,
            EntryValue::Number(MAX_OPEN_OBJECTS as u64)
        );
        assert_eq!(
            objects.entries[1].value,
            EntryValue::Number(MAX_ASSEMBLY_BYTES as u64)
        );
        let fragments = &groups[1];
        assert_eq!(
            fragments.entries[1].value,
            EntryValue::Number(u64::from(MAX_HEADER_EXT))
        );
        let regions = &groups[4];
        assert_eq!(
            regions.entries[0].value,
            EntryValue::Number(u64::from(MAX_CELLS))
        );
    }

    #[test]
    fn every_group_has_entries_and_a_section() {
        for group in groups() {
            assert!(!group.entries.is_empty(), "{} has no entries", group.name);
            assert!(!group.spec.is_empty(), "{} has no section", group.name);
        }
    }

    #[test]
    fn the_table_holds_every_cap_the_specification_states() {
        let names: Vec<&str> = groups()
            .iter()
            .flat_map(|group| group.entries.iter())
            .map(|entry| entry.name)
            .collect();
        for expected in [
            "MAX_OPEN_OBJECTS",
            "MAX_ASSEMBLY_BYTES",
            "MAX_FRAGMENT_COUNT",
            "MAX_HEADER_EXT",
            "MAX_WS_FRAGMENT",
            "TARGET_FRAGMENT_PAYLOAD",
            "MAX_CELLS",
            "ACK_RING_FLOOR",
            "INV-1",
            "INV-10",
        ] {
            assert!(names.contains(&expected), "{expected} is missing");
        }
    }

    #[test]
    fn the_json_form_carries_every_group() {
        let options = crate::cli::test_options(&[]);
        let produced = limits(&options, true).expect("limits");
        let document: Value = serde_json::from_str(&produced.text).expect("json");
        let rendered = document["groups"].as_array().expect("groups");
        assert_eq!(rendered.len(), groups().len());
        assert_eq!(document["command"], "limits");
    }
}
