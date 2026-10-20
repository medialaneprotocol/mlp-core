//! `conformance`: run the published conformance corpus against this build.
//!
//! The corpus is a sibling repository — it is read, never copied. The tool
//! reads the corpus's committed scenarios and link profiles, replays each one
//! through this build's simulator, and compares the decision trace it
//! produces with the trace the corpus committed. A scenario whose digest
//! differs is a failure: the corpus is the golden record.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::cli::{CliError, Options, Produced, json_line, table};
use crate::media::MediaPath;
use crate::media::cells::CellRect;
use crate::media::input::InputKind;
use crate::testing::lossy_link::{JitterDist, LinkProfile, Outage, Tier};
use crate::testing::scenario::{Invariant, Scenario, Step};

/// The environment variable that names the corpus directory.
pub const CORPUS_DIR_ENV: &str = "MLP_CONFORMANCE_DIR";

/// The verdict of one scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The scenario's id.
    pub id: String,
    /// What the run found: `match`, `mismatch`, `missing` or `error`.
    pub outcome: &'static str,
    /// The digest the corpus committed.
    pub expected: Option<String>,
    /// The digest this build produced.
    pub actual: Option<String>,
    /// The invariants the run violated.
    pub violations: usize,
    /// The scenario's simulated duration.
    pub duration_ms: u64,
    /// What went wrong, when something did.
    pub detail: Option<String>,
}

/// Run the published conformance corpus against this build.
pub fn conformance(options: &Options, json: bool) -> Result<Produced, CliError> {
    let corpus = locate_corpus(options)?;
    let profiles = read_profiles(&corpus)?;
    let wanted: Vec<String> = options
        .values("scenario")
        .into_iter()
        .map(str::to_string)
        .collect();

    let scenario_dir = corpus.join("scenarios");
    let decision_dir = corpus.join("decisions");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&scenario_dir)
        .map_err(|error| {
            CliError::Input(format!(
                "the corpus's scenarios could not be read from {}: {error}",
                scenario_dir.display()
            ))
        })?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    entries.sort();

    let mut verdicts = Vec::new();
    for path in &entries {
        let id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        if !wanted.is_empty() && !wanted.iter().any(|wanted| wanted == &id) {
            continue;
        }
        verdicts.push(run_one(&id, path, &decision_dir, &profiles)?);
    }

    if verdicts.is_empty() {
        return Err(CliError::Input(
            "the corpus holds no scenario to run".to_string(),
        ));
    }

    let matched = verdicts
        .iter()
        .filter(|verdict| verdict.outcome == "match")
        .count();
    let failed = verdicts.len() - matched;

    if json {
        let scenarios: Vec<Value> = verdicts
            .iter()
            .map(|verdict| {
                json!({
                    "id": verdict.id,
                    "verdict": verdict.outcome,
                    "expected_digest": verdict.expected,
                    "actual_digest": verdict.actual,
                    "violations": verdict.violations,
                    "duration_ms": verdict.duration_ms,
                    "detail": verdict.detail,
                })
            })
            .collect();
        let text = json_line(&json!({
            "command": "conformance",
            "corpus": corpus.to_string_lossy(),
            "scenarios": scenarios,
            "summary": {
                "total": verdicts.len(),
                "matched": matched,
                "failed": failed,
            },
        }));
        Ok(if failed == 0 {
            Produced::ok(text)
        } else {
            Produced::failed(text)
        })
    } else {
        let text = render(&corpus, &verdicts, matched);
        Ok(if failed == 0 {
            Produced::ok(text)
        } else {
            Produced::failed(text)
        })
    }
}

/// Find the corpus directory.
fn locate_corpus(options: &Options) -> Result<PathBuf, CliError> {
    if let Some(path) = options.value("corpus") {
        let path = PathBuf::from(path);
        if !path.is_dir() {
            return Err(CliError::Input(format!(
                "the corpus directory {} does not exist",
                path.display()
            )));
        }
        return Ok(path);
    }
    if let Ok(path) = std::env::var(CORPUS_DIR_ENV)
        && !path.is_empty()
    {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return Ok(path);
        }
        return Err(CliError::Input(format!(
            "the corpus directory {} named by {CORPUS_DIR_ENV} does not exist",
            path.display()
        )));
    }
    let sibling = PathBuf::from("../mlp-conformance");
    if sibling.is_dir() {
        return Ok(sibling);
    }
    Err(CliError::Input(format!(
        "the corpus repository could not be found; pass `--corpus <dir>` or set {CORPUS_DIR_ENV}"
    )))
}

/// Read every link profile the corpus commits.
fn read_profiles(corpus: &Path) -> Result<BTreeMap<String, LinkProfile>, CliError> {
    let dir = corpus.join("profiles");
    let mut profiles = BTreeMap::new();
    let entries = std::fs::read_dir(&dir).map_err(|error| {
        CliError::Input(format!(
            "the corpus's profiles could not be read from {}: {error}",
            dir.display()
        ))
    })?;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|error| {
            CliError::Input(format!(
                "the profile {} could not be read: {error}",
                path.display()
            ))
        })?;
        let document: Value = serde_json::from_str(&text).map_err(|error| {
            CliError::Input(format!(
                "the profile {} is not valid JSON: {error}",
                path.display()
            ))
        })?;
        let id = document["id"].as_str().unwrap_or_default().to_string();
        profiles.insert(id, profile_from(&document, &path)?);
    }
    Ok(profiles)
}

/// Read one link profile.
fn profile_from(document: &Value, path: &Path) -> Result<LinkProfile, CliError> {
    let bad = |field: &str| {
        CliError::Input(format!(
            "the profile {} has no usable {field}",
            path.display()
        ))
    };
    let jitter_dist = match document["jitter_dist"].as_str() {
        Some("Normal") | None => JitterDist::Normal,
        Some("Pareto") => JitterDist::Pareto,
        Some(other) => {
            return Err(CliError::Input(format!(
                "the profile {} has an unknown jitter distribution {other:?}",
                path.display()
            )));
        }
    };
    let outage_schedule = document["outage_schedule"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| Outage {
                    start_ms: entry["start_ms"].as_u64().unwrap_or(0),
                    dur_ms: entry["dur_ms"].as_u64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(LinkProfile {
        rtt_ms: document["rtt_ms"].as_f64().ok_or_else(|| bad("rtt_ms"))?,
        jitter_ms: document["jitter_ms"]
            .as_f64()
            .ok_or_else(|| bad("jitter_ms"))?,
        jitter_dist,
        loss_permille: document["loss_permille"].as_u64().unwrap_or(0) as u32,
        burst_loss: document["burst_loss"].as_u64().map(|value| value as u32),
        reorder_permille: document["reorder_permille"].as_u64().unwrap_or(0) as u32,
        duplicate_permille: document["duplicate_permille"].as_u64().unwrap_or(0) as u32,
        rate_bps_down: document["rate_bps_down"]
            .as_u64()
            .ok_or_else(|| bad("rate_bps_down"))?,
        rate_bps_up: document["rate_bps_up"]
            .as_u64()
            .ok_or_else(|| bad("rate_bps_up"))?,
        mtu_bytes: document["mtu_bytes"].as_u64().unwrap_or(65_536) as u32,
        outage_schedule,
        ack_delay_ms: document["ack_delay_ms"].as_u64().unwrap_or(0),
        seed: document["seed"].as_u64().unwrap_or(0),
    })
}

/// Run one scenario and compare its digest with the corpus's.
fn run_one(
    id: &str,
    path: &Path,
    decision_dir: &Path,
    profiles: &BTreeMap<String, LinkProfile>,
) -> Result<Verdict, CliError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        CliError::Input(format!(
            "the scenario {} could not be read: {error}",
            path.display()
        ))
    })?;
    let document: Value = serde_json::from_str(&text).map_err(|error| {
        CliError::Input(format!(
            "the scenario {} is not valid JSON: {error}",
            path.display()
        ))
    })?;
    let scenario = match scenario_from(&document, path, profiles) {
        Ok(scenario) => scenario,
        Err(error) => {
            return Ok(Verdict {
                id: id.to_string(),
                outcome: "error",
                expected: None,
                actual: None,
                violations: 0,
                duration_ms: 0,
                detail: Some(error.message().to_string()),
            });
        }
    };
    let trace = crate::testing::scenario::run(&scenario);
    let actual = trace.digest();
    let expected_path = decision_dir.join(format!("{id}.json"));
    let expected = std::fs::read_to_string(&expected_path)
        .ok()
        .and_then(|text| {
            serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|document| document["digest"].as_str().map(str::to_string))
        });
    let outcome = match &expected {
        None => "missing",
        Some(expected) if *expected == actual => "match",
        Some(_) => "mismatch",
    };
    Ok(Verdict {
        id: id.to_string(),
        outcome,
        expected,
        actual: Some(actual),
        violations: trace.violations.len(),
        duration_ms: trace.duration_ms,
        detail: None,
    })
}

/// Read one scenario.
fn scenario_from(
    document: &Value,
    path: &Path,
    profiles: &BTreeMap<String, LinkProfile>,
) -> Result<Scenario, CliError> {
    let id = document["id"].as_str().unwrap_or_default().to_string();
    let tier = match document["tier"].as_str() {
        Some("T0") => Tier::T0,
        Some("T1") => Tier::T1,
        Some("T2") => Tier::T2,
        Some("T3") => Tier::T3,
        Some("T4") => Tier::T4,
        other => {
            return Err(CliError::Input(format!(
                "the scenario {} names the unknown tier {other:?}",
                path.display()
            )));
        }
    };
    let receivers = document["receivers"].as_u64().unwrap_or(1) as u8;
    let mut steps = Vec::new();
    let entries = document["steps"].as_array().ok_or_else(|| {
        CliError::Input(format!(
            "the scenario {} holds no step list",
            path.display()
        ))
    })?;
    for (number, step) in entries.iter().enumerate() {
        steps.push(step_from(step, number, path, profiles)?);
    }
    Ok(Scenario {
        name: id,
        tier,
        steps,
        receivers,
    })
}

/// Read one step.
fn step_from(
    step: &Value,
    number: usize,
    path: &Path,
    profiles: &BTreeMap<String, LinkProfile>,
) -> Result<Step, CliError> {
    let bad = |what: &str| {
        CliError::Input(format!(
            "the scenario {} holds an unusable step {number}: {what}",
            path.display()
        ))
    };
    if let Some(text) = step.as_str() {
        return match text {
            "ViewerDisconnect" => Ok(Step::ViewerDisconnect),
            "ViewerReconnect" => Ok(Step::ViewerReconnect),
            "HostReconnect" => Ok(Step::HostReconnect),
            other => Err(bad(&format!("{other:?} is not a step"))),
        };
    }
    let object = step.as_object().ok_or_else(|| bad("it is not an object"))?;
    let (form, value) = object.iter().next().ok_or_else(|| bad("it is empty"))?;
    match form.as_str() {
        "Advance" => Ok(Step::Advance(
            value
                .as_u64()
                .ok_or_else(|| bad("Advance is not a number"))?,
        )),
        "Link" => {
            let profile = value["profile"].as_str().unwrap_or_default();
            let profile = profiles
                .get(profile)
                .ok_or_else(|| bad(&format!("the link profile {profile:?} is unknown")))?;
            Ok(Step::Link(Box::new(profile.clone())))
        }
        "CaptureChange" => Ok(Step::CaptureChange(CellRect::new(
            value["x"].as_u64().ok_or_else(|| bad("CaptureChange x"))? as u32,
            value["y"].as_u64().ok_or_else(|| bad("CaptureChange y"))? as u32,
            value["width"]
                .as_u64()
                .ok_or_else(|| bad("CaptureChange width"))? as u32,
            value["height"]
                .as_u64()
                .ok_or_else(|| bad("CaptureChange height"))? as u32,
        ))),
        "ViewerInput" => {
            let kind = if value.is_string() {
                InputKind::ReleaseAll
            } else {
                let name = value
                    .as_object()
                    .and_then(|object| object.keys().next().cloned())
                    .unwrap_or_default();
                match name.as_str() {
                    "PointerMove" => InputKind::PointerMove,
                    "PointerButton" => InputKind::PointerButton,
                    "Key" => InputKind::Key,
                    "Wheel" => InputKind::Wheel,
                    "PressedSet" => InputKind::PressedSet,
                    "Clipboard" => InputKind::Clipboard,
                    other => return Err(bad(&format!("the input {other:?} is unknown"))),
                }
            };
            Ok(Step::ViewerInput(kind))
        }
        "PathSwitch" => match value.as_str() {
            Some("ws") => Ok(Step::PathSwitch(MediaPath::Ws)),
            Some("dc") => Ok(Step::PathSwitch(MediaPath::DcDirect)),
            other => Err(bad(&format!("the path {other:?} is unknown"))),
        },
        "Assert" => {
            let text = value.as_str().unwrap_or_default();
            let number: usize = text
                .strip_prefix("INV-")
                .and_then(|number| number.parse().ok())
                .ok_or_else(|| bad(&format!("the invariant {text:?} is unknown")))?;
            if !(1..=10).contains(&number) {
                return Err(bad(&format!("the invariant {text:?} is out of range")));
            }
            Ok(Step::Assert(Invariant::all()[number - 1]))
        }
        other => Err(bad(&format!("{other:?} is not a step"))),
    }
}

/// The human form of the verdicts.
fn render(corpus: &Path, verdicts: &[Verdict], matched: usize) -> String {
    let mut text = String::new();
    text.push_str(&format!(
        "conformance corpus {}\n\n",
        corpus.to_string_lossy()
    ));
    let rows: Vec<Vec<String>> = verdicts
        .iter()
        .map(|verdict| {
            vec![
                verdict.id.clone(),
                verdict.outcome.to_string(),
                verdict.expected.clone().unwrap_or_else(|| "-".to_string()),
                verdict.actual.clone().unwrap_or_else(|| "-".to_string()),
                verdict.violations.to_string(),
            ]
        })
        .collect();
    text.push_str(&table(
        &["scenario", "verdict", "committed", "produced", "violations"],
        &rows,
    ));
    text.push_str(&format!(
        "\n{} of {} scenario(s) match the corpus\n",
        matched,
        verdicts.len()
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_is_read_from_the_corpuss_own_json() {
        let document = json!({
            "id": "t0",
            "rtt_ms": 10.0,
            "jitter_ms": 2.0,
            "jitter_dist": "Normal",
            "loss_permille": 0,
            "reorder_permille": 0,
            "duplicate_permille": 0,
            "rate_bps_down": 100_000_000,
            "rate_bps_up": 10_000_000,
            "mtu_bytes": 65_536,
            "outage_schedule": [{"start_ms": 0, "dur_ms": 10_000}],
            "ack_delay_ms": 0,
            "seed": 1000
        });
        let profile = profile_from(&document, Path::new("t0.json")).expect("profile");
        assert_eq!(profile.rtt_ms, 10.0);
        assert_eq!(profile.seed, 1000);
        assert_eq!(profile.outage_schedule.len(), 1);
    }

    #[test]
    fn a_step_is_read_from_the_corpuss_own_json() {
        let profiles = BTreeMap::new();
        let step =
            step_from(&json!({"Advance": 500}), 0, Path::new("s.json"), &profiles).expect("step");
        assert_eq!(step, Step::Advance(500));
    }

    #[test]
    fn an_unknown_step_is_refused_with_its_reason() {
        let profiles = BTreeMap::new();
        let error = step_from(&json!({"Nonsense": 1}), 0, Path::new("s.json"), &profiles)
            .expect_err("unknown step");
        assert!(error.message().contains("is not a step"));
    }

    #[test]
    fn an_unknown_invariant_is_refused() {
        let profiles = BTreeMap::new();
        let error = step_from(
            &json!({"Assert": "INV-99"}),
            0,
            Path::new("s.json"),
            &profiles,
        )
        .expect_err("unknown invariant");
        assert!(error.message().contains("out of range"));
    }

    #[test]
    fn the_corpus_directory_is_named_by_the_environment_or_the_flag() {
        let options = crate::cli::test_options(&[("corpus", Some("/nowhere/at/all"))]);
        let error = conformance(&options, true).expect_err("no corpus");
        assert_eq!(error.exit_code(), crate::cli::EXIT_INPUT);
    }
}
