//! `object assemble`: the receiver's own view of a set of fragments.

use std::time::Instant;

use serde_json::{Value, json};

use crate::cli::fragment::{body_value, parse_carrier, tlv_values};
use crate::cli::wire::parse_hex;
use crate::cli::{CliError, Options, Produced, json_line, table};
use crate::media::object::{AssemblyOutcome, MediaObject, ObjectAssembler};
use crate::media::pdu::Fragment;

/// Assemble fragments into one object and report what the assembler decided.
pub fn assemble(options: &Options, json: bool) -> Result<Produced, CliError> {
    let carrier = parse_carrier(options.value("carrier").unwrap_or("ws"))?;
    let session_ms = options.unsigned::<u64>("session-ms")?.unwrap_or(0);
    let fragments = read_fragments(options)?;

    let mut assembler = ObjectAssembler::new(Default::default());
    let now = Instant::now();
    let mut completed = None;
    let mut rejected = None;
    let mut dropped_fragments = 0_usize;

    for (index, bytes) in fragments.iter().enumerate() {
        let (fragment, consumed) = Fragment::parse(bytes, carrier).map_err(|reason| {
            CliError::Input(format!(
                "fragment {index} is not valid: {}",
                reason.as_str()
            ))
        })?;
        if consumed != bytes.len() {
            return Err(CliError::Input(format!(
                "fragment {index} holds {} trailing byte(s)",
                bytes.len() - consumed
            )));
        }
        match assembler.accept(fragment, session_ms, now) {
            AssemblyOutcome::Object(object) => completed = Some(object),
            AssemblyOutcome::Pending => {}
            AssemblyOutcome::Dropped { reason, object_id } => {
                dropped_fragments += 1;
                if rejected.is_none() {
                    rejected = Some((reason, object_id));
                }
            }
        }
    }

    let status = if rejected.is_some() {
        "rejected"
    } else if completed.is_some() {
        "complete"
    } else {
        "pending"
    };
    let reason = rejected.as_ref().map(|(reason, _)| reason.as_str());

    if json {
        let mut document = json!({
            "command": "object.assemble",
            "status": status,
            "reason": reason,
            "fragments": fragments.len(),
            "dropped_fragments": dropped_fragments,
            "open_objects": assembler.open_objects(),
            "open_bytes": assembler.open_bytes(),
        });
        let object = document.as_object_mut().expect("the document is an object");
        match &completed {
            Some(completed) => {
                object.insert("object".into(), object_value(completed));
            }
            None => {
                object.insert("object".into(), Value::Null);
            }
        }
        Ok(Produced::ok(json_line(&document)))
    } else {
        Ok(Produced::ok(render(
            status,
            reason,
            fragments.len(),
            dropped_fragments,
            completed.as_ref(),
            &assembler,
        )))
    }
}

/// Read every fragment's bytes, in the order they were given.
fn read_fragments(options: &Options) -> Result<Vec<Vec<u8>>, CliError> {
    let mut fragments = Vec::new();
    for text in options.values("hex") {
        fragments.push(parse_hex(text)?);
    }
    if let Some(path) = options.value("file") {
        let text = std::fs::read_to_string(path).map_err(|error| {
            CliError::Input(format!("the file {path:?} could not be read: {error}"))
        })?;
        fragments.extend(read_lines(&text)?);
    }
    if options.flag("stdin").is_some() {
        let text = crate::cli::read_stdin()?;
        fragments.extend(read_lines(&text)?);
    }
    if fragments.is_empty() {
        return Err(CliError::Usage(
            "no fragments were given; use `--hex`, `--file` or `--stdin`".to_string(),
        ));
    }
    Ok(fragments)
}

/// Read one fragment's hex per line, ignoring blank lines.
fn read_lines(text: &str) -> Result<Vec<Vec<u8>>, CliError> {
    let mut fragments = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        fragments.push(parse_hex(line).map_err(|error| {
            CliError::Input(format!("line {}: {}", number + 1, error.message()))
        })?);
    }
    Ok(fragments)
}

/// The JSON form of an assembled object.
fn object_value(object: &MediaObject) -> Value {
    json!({
        "object_id": object.object_id,
        "class": object.class.as_str(),
        "flags": flag_names(object.flags),
        "priority": object.priority.get(),
        "payload_len": object.payload.len(),
        "payload_sha256": body_value(&object.payload)["sha256"].clone(),
        "is_anchor": object.is_anchor(),
        "is_duplicate": object.is_duplicate(),
        "echo_of_object_id": object.echo_of(),
        "deadline_epoch_ms": object.deadline_epoch_ms(),
        "update": object.update_meta().map(|meta| json!({
            "update_id": meta.update_id,
            "update_base": meta.update_base,
            "part_index": meta.part_index,
            "part_count": meta.part_count,
            "update_bytes": meta.update_bytes,
            "chain_epoch": meta.chain_epoch,
            "reflects_input_seq": meta.reflects_input_seq,
            "stream_id": meta.stream_id,
            "send_epoch_ms": meta.send_epoch_ms,
        })),
        "tlvs": tlv_values(&object.ext),
    })
}

/// The flag bits' names, in the order the flags byte names them.
fn flag_names(flags: crate::media::pdu::FragmentFlags) -> Vec<&'static str> {
    use crate::media::pdu::FragmentFlags;
    let mut names = Vec::new();
    for (flag, name) in [
        (FragmentFlags::FIN, "fin"),
        (FragmentFlags::ANCHOR, "anchor"),
        (FragmentFlags::RETRANSMIT, "retransmit"),
        (FragmentFlags::DUP, "dup"),
        (FragmentFlags::ECHO_REQUESTED, "echo-requested"),
        (FragmentFlags::PROBE, "probe"),
        (FragmentFlags::URGENT, "urgent"),
    ] {
        if flags.contains(flag) {
            names.push(name);
        }
    }
    names
}

/// The human form of the assembler's decision.
fn render(
    status: &str,
    reason: Option<&str>,
    fragments: usize,
    dropped: usize,
    completed: Option<&MediaObject>,
    assembler: &ObjectAssembler,
) -> String {
    let mut text = String::new();
    text.push_str(&format!("assembly {status}\n"));
    text.push_str(&format!("  fragments        {fragments}\n"));
    if let Some(reason) = reason {
        text.push_str(&format!("  reason           {reason}\n"));
    }
    text.push_str(&format!("  dropped          {dropped}\n"));
    text.push_str(&format!(
        "  open objects     {}\n",
        assembler.open_objects()
    ));
    text.push_str(&format!("  open bytes       {}\n", assembler.open_bytes()));
    if let Some(object) = completed {
        text.push('\n');
        text.push_str(&format!("  object id        {}\n", object.object_id));
        text.push_str(&format!("  class            {}\n", object.class.as_str()));
        let flags = flag_names(object.flags);
        text.push_str(&format!(
            "  flags            {}\n",
            if flags.is_empty() {
                "none".to_string()
            } else {
                flags.join(", ")
            }
        ));
        text.push_str(&format!("  priority         {}\n", object.priority.get()));
        text.push_str(&format!(
            "  payload          {} bytes\n",
            object.payload.len()
        ));
        text.push_str(&format!(
            "  payload sha256   {}\n",
            body_value(&object.payload)["sha256"]
                .as_str()
                .unwrap_or_default()
        ));
        if let Some(meta) = object.update_meta() {
            text.push('\n');
            let rows = vec![
                vec!["update id".to_string(), meta.update_id.to_string()],
                vec!["update base".to_string(), meta.update_base.to_string()],
                vec![
                    "part".to_string(),
                    format!("{} of {}", meta.part_index, meta.part_count),
                ],
                vec!["update bytes".to_string(), meta.update_bytes.to_string()],
                vec!["chain epoch".to_string(), meta.chain_epoch.to_string()],
                vec![
                    "reflects input".to_string(),
                    meta.reflects_input_seq.to_string(),
                ],
                vec!["stream id".to_string(), meta.stream_id.to_string()],
                vec!["send epoch ms".to_string(), meta.send_epoch_ms.to_string()],
            ];
            text.push_str(&table(&["update", "value"], &rows));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_options;

    #[test]
    fn a_complete_object_is_reported_as_complete() {
        let options =
            test_options(&[("hex", Some("4d4c5000010201060100000000000100000000000000"))]);
        let outcome = assemble(&options, true).expect("assembles");
        assert!(outcome.text.contains("\"status\": \"complete\""));
    }

    #[test]
    fn an_incomplete_object_is_reported_as_pending() {
        let options =
            test_options(&[("hex", Some("4d4c5000010201060100000000000200000000000000"))]);
        let outcome = assemble(&options, true).expect("assembles");
        assert!(outcome.text.contains("\"status\": \"pending\""));
    }

    #[test]
    fn a_duplicate_fragment_is_reported_as_rejected() {
        let fragment = "4d4c5000010201060100000000000100000000000000";
        let options = test_options(&[("hex", Some(fragment)), ("hex", Some(fragment))]);
        let outcome = assemble(&options, true).expect("assembles");
        assert!(outcome.text.contains("\"status\": \"rejected\""));
        assert!(outcome.text.contains("\"reason\": \"duplicate\""));
    }

    #[test]
    fn a_bad_fragment_is_an_input_error() {
        let options = test_options(&[("hex", Some("00"))]);
        let error = assemble(&options, true).expect_err("bad fragment");
        assert_eq!(error.exit_code(), crate::cli::EXIT_INPUT);
    }

    #[test]
    fn no_fragment_at_all_is_a_usage_error() {
        let options = test_options(&[]);
        let error = assemble(&options, true).expect_err("no fragment");
        assert_eq!(error.exit_code(), crate::cli::EXIT_USAGE);
    }
}
