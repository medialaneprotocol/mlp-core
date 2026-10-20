//! `ack build` and `ack inspect`: the receiver's acknowledgement.

use serde_json::{Value, json};

use crate::cli::wire::{decode_ack, encode_ack, parse_hex};
use crate::cli::{CliError, Options, Produced, hex, json_line, table};
use crate::media::MediaAck;

/// Build the receiver's acknowledgement and print its bytes.
pub fn build(options: &Options, json: bool) -> Result<Produced, CliError> {
    let ack = MediaAck {
        highest_applied_update_id: options
            .unsigned::<u64>("highest-applied-update-id")?
            .unwrap_or(0),
        ack_bitmap: bitmap(options)?,
        chain_epoch: options.unsigned::<u32>("chain-epoch")?.unwrap_or(0),
        applied_cell_count: options.unsigned::<u32>("applied-cell-count")?.unwrap_or(0),
        stale_cells: options.unsigned::<u32>("stale-cells")?.unwrap_or(0),
        converged: options.boolean("converged")?.unwrap_or(false),
        render_latency_ms: options.unsigned::<u64>("render-latency-ms")?.unwrap_or(0) as f64,
        has_render_latency_ms: options.value("render-latency-ms").is_some(),
        echo_of_input_seq: options.unsigned::<u64>("echo-of-input-seq")?.unwrap_or(0),
        ack_epoch_ms: options.unsigned::<u64>("ack-epoch-ms")?.unwrap_or(0),
    };
    ack.validate().map_err(|reason| {
        CliError::Input(format!(
            "the acknowledgement is not valid: {}",
            reason.as_str()
        ))
    })?;
    let bytes = encode_ack(&ack);
    if json {
        Ok(Produced::ok(json_line(&json!({
            "command": "ack.build",
            "ack": ack_value(&ack),
            "window": window_value(&ack),
            "cell_counts": cell_counts(&ack),
            "bytes_hex": hex(&bytes),
            "bytes_len": bytes.len(),
        }))))
    } else {
        Ok(Produced::ok(render(&ack, Some(&bytes))))
    }
}

/// Read an acknowledgement's bytes and print its fields and its window.
pub fn inspect(options: &Options, json: bool) -> Result<Produced, CliError> {
    let bytes = read_input(options)?;
    let ack = decode_ack(&bytes)?;
    ack.validate().map_err(|reason| {
        CliError::Input(format!(
            "the acknowledgement is not valid: {}",
            reason.as_str()
        ))
    })?;
    if json {
        Ok(Produced::ok(json_line(&json!({
            "command": "ack.inspect",
            "ack": ack_value(&ack),
            "window": window_value(&ack),
            "cell_counts": cell_counts(&ack),
            "bytes_len": bytes.len(),
        }))))
    } else {
        Ok(Produced::ok(render(&ack, None)))
    }
}

/// Read the acknowledgement's bytes from `--hex`, `--file` or standard input.
fn read_input(options: &Options) -> Result<Vec<u8>, CliError> {
    let mut sources = 0usize;
    if options.value("hex").is_some() {
        sources += 1;
    }
    if options.value("file").is_some() {
        sources += 1;
    }
    if options.flag("stdin").is_some() {
        sources += 1;
    }
    if sources == 0 {
        return Err(CliError::Usage(
            "one of `--hex`, `--file` or `--stdin` is required".to_string(),
        ));
    }
    if sources > 1 {
        return Err(CliError::Usage(
            "only one of `--hex`, `--file` and `--stdin` may be given".to_string(),
        ));
    }
    if let Some(text) = options.value("hex") {
        return parse_hex(text);
    }
    if let Some(path) = options.value("file") {
        let text = std::fs::read_to_string(path).map_err(|error| {
            CliError::Input(format!("the file {path:?} could not be read: {error}"))
        })?;
        return parse_hex(&text);
    }
    let text = crate::cli::read_stdin()?;
    parse_hex(&text)
}

/// The bitmap, read as hexadecimal.
fn bitmap(options: &Options) -> Result<u64, CliError> {
    match options.value("ack-bitmap") {
        None => Ok(0),
        Some(text) => {
            let trimmed = text.strip_prefix("0x").unwrap_or(text);
            u64::from_str_radix(trimmed, 16).map_err(|error| {
                CliError::Usage(format!(
                    "flag `--ack-bitmap`: {text:?} is not hexadecimal ({error})"
                ))
            })
        }
    }
}

/// The JSON form of an acknowledgement's fields.
fn ack_value(ack: &MediaAck) -> Value {
    json!({
        "highest_applied_update_id": ack.highest_applied_update_id,
        "ack_bitmap": format!("{:016x}", ack.ack_bitmap),
        "chain_epoch": ack.chain_epoch,
        "applied_cell_count": ack.applied_cell_count,
        "stale_cells": ack.stale_cells,
        "converged": ack.converged,
        "render_latency_ms": ack.render_latency_ms,
        "has_render_latency_ms": ack.has_render_latency_ms,
        "echo_of_input_seq": ack.echo_of_input_seq,
        "ack_epoch_ms": ack.ack_epoch_ms,
    })
}

/// The JSON form of the acknowledgement's 64-update window.
fn window_value(ack: &MediaAck) -> Value {
    let applied: Vec<u64> = ack.applied_updates().collect();
    json!({
        "size": 64,
        "highest_applied_update_id": ack.highest_applied_update_id,
        "applied_count": applied.len(),
        "applied_update_ids": applied,
        "empty": ack.is_empty(),
    })
}

/// The JSON form of the acknowledgement's cell counts.
fn cell_counts(ack: &MediaAck) -> Value {
    json!({
        "applied_cell_count": ack.applied_cell_count,
        "stale_cells": ack.stale_cells,
        "converged": ack.converged,
    })
}

/// The human form of an acknowledgement.
fn render(ack: &MediaAck, bytes: Option<&[u8]>) -> String {
    let mut text = String::new();
    if let Some(bytes) = bytes {
        text.push_str("acknowledgement built\n");
        text.push_str(&format!("  bytes            {}\n", hex(bytes)));
    } else {
        text.push_str("acknowledgement\n");
    }
    let rows = vec![
        vec![
            "highest applied".to_string(),
            ack.highest_applied_update_id.to_string(),
        ],
        vec!["bitmap".to_string(), format!("{:016x}", ack.ack_bitmap)],
        vec!["chain epoch".to_string(), ack.chain_epoch.to_string()],
        vec![
            "applied cells".to_string(),
            ack.applied_cell_count.to_string(),
        ],
        vec!["stale cells".to_string(), ack.stale_cells.to_string()],
        vec!["converged".to_string(), ack.converged.to_string()],
        vec![
            "render latency".to_string(),
            format!("{} ms", ack.render_latency_ms),
        ],
        vec![
            "echo of input".to_string(),
            ack.echo_of_input_seq.to_string(),
        ],
        vec!["ack epoch".to_string(), ack.ack_epoch_ms.to_string()],
    ];
    text.push('\n');
    text.push_str(&table(&["field", "value"], &rows));
    text.push('\n');
    let applied: Vec<String> = ack
        .applied_updates()
        .map(|update| update.to_string())
        .collect();
    text.push_str(&format!(
        "  window           64 updates, highest {}\n",
        ack.highest_applied_update_id
    ));
    text.push_str(&format!(
        "  applied updates  {}\n",
        if applied.is_empty() {
            "none".to_string()
        } else {
            applied.join(", ")
        }
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_options;

    #[test]
    fn an_acknowledgement_is_built_and_printed_as_hex() {
        let options = test_options(&[
            ("highest-applied-update-id", Some("10")),
            ("ack-bitmap", Some("15")),
            ("applied-cell-count", Some("510")),
            ("converged", Some("true")),
        ]);
        let outcome = build(&options, false).expect("builds");
        assert!(outcome.text.contains("acknowledgement built"));
        assert!(outcome.text.contains("highest applied"));
    }

    #[test]
    fn a_built_acknowledgement_reads_back() {
        let options = test_options(&[
            ("highest-applied-update-id", Some("10")),
            ("ack-bitmap", Some("15")),
            ("applied-cell-count", Some("510")),
        ]);
        let built = build(&options, true).expect("builds");
        let document: Value = serde_json::from_str(&built.text).expect("json");
        let bytes = document["bytes_hex"].as_str().expect("hex");
        let inspect_options = test_options(&[("hex", Some(bytes))]);
        let read = inspect(&inspect_options, true).expect("inspects");
        assert!(read.text.contains("\"highest_applied_update_id\": 10"));
        assert!(read.text.contains("\"applied_count\": 3"));
    }

    #[test]
    fn a_bitmap_above_the_highest_update_is_refused() {
        let options = test_options(&[("ack-bitmap", Some("01"))]);
        let error = build(&options, true).expect_err("bad bitmap");
        assert_eq!(error.exit_code(), crate::cli::EXIT_INPUT);
        assert!(error.message().contains("ack_bad_bitmap"));
    }

    #[test]
    fn stale_cells_above_the_applied_count_are_refused() {
        let options = test_options(&[
            ("highest-applied-update-id", Some("3")),
            ("ack-bitmap", Some("01")),
            ("applied-cell-count", Some("4")),
            ("stale-cells", Some("5")),
        ]);
        let error = build(&options, true).expect_err("bad counts");
        assert!(error.message().contains("ack_bad_count"));
    }

    #[test]
    fn a_bad_bitmap_is_a_usage_error() {
        let options = test_options(&[("ack-bitmap", Some("zz"))]);
        let error = build(&options, true).expect_err("bad bitmap");
        assert_eq!(error.exit_code(), crate::cli::EXIT_USAGE);
    }
}
