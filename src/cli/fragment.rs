//! `fragment build` and `fragment inspect`: the wire format's own verbs.

use serde_json::{Value, json};

use crate::cli::wire::{parse_hex, sha256_hex};
use crate::cli::{CliError, Options, Produced, hex, json_line, table};
use crate::media::RejectReason;
use crate::media::pdu::{
    Carrier, Fragment, FragmentClass, FragmentFlags, HeaderExt, Priority, tlv,
};

/// Build one fragment from its fields and print its bytes.
pub fn build(options: &Options, json: bool) -> Result<Produced, CliError> {
    let class = parse_class(options.value("class").unwrap_or("media"))?;
    let priority = match options.unsigned::<u8>("priority")? {
        Some(value) => Priority::new(value).ok_or_else(|| {
            CliError::Usage(format!("flag `--priority`: {value} is outside 0..=7"))
        })?,
        None => Priority::new(class.traffic_class().default_priority()).ok_or_else(|| {
            CliError::Usage("the class's default priority is outside 0..=7".to_string())
        })?,
    };
    let flags = parse_flags(options.value("flags").unwrap_or(""))?;
    let object_id = options.unsigned::<u32>("object-id")?.unwrap_or(1);
    let fragment_index = options.unsigned::<u16>("fragment-index")?.unwrap_or(0);
    let fragment_count = options.unsigned::<u16>("fragment-count")?.unwrap_or(1);
    let body = read_body(options)?;
    let ext = build_extension(options)?;

    let fragment = Fragment::new(
        class,
        flags,
        priority,
        object_id,
        fragment_index,
        fragment_count,
        ext,
        body,
    )
    .map_err(|reason| CliError::Input(format!("the fragment is not valid: {}", reason.as_str())))?;

    let bytes = fragment.encode();
    if json {
        let mut document = json!({
            "command": "fragment.build",
            "bytes_hex": hex(&bytes),
            "bytes_len": bytes.len(),
        });
        let object = document.as_object_mut().expect("the document is an object");
        object.insert("fragment".into(), header_value(&fragment));
        object.insert("tlvs".into(), tlv_values(&fragment.ext));
        object.insert("body".into(), body_value(&fragment.payload));
        Ok(Produced::ok(json_line(&document)))
    } else {
        Ok(Produced::ok(render_fragment(&fragment, &bytes, "built")))
    }
}

/// Read a fragment's bytes and print its header, its TLVs and its body.
pub fn inspect(options: &Options, json: bool) -> Result<Produced, CliError> {
    let carrier = parse_carrier(options.value("carrier").unwrap_or("ws"))?;
    let bytes = read_input(options)?;
    let (fragment, consumed) = Fragment::parse(&bytes, carrier).map_err(|reason| {
        CliError::Input(format!("the fragment is not valid: {}", reason.as_str()))
    })?;
    if consumed != bytes.len() {
        return Err(CliError::Input(format!(
            "the input holds {} trailing byte(s) after the fragment",
            bytes.len() - consumed
        )));
    }
    if json {
        Ok(Produced::ok(json_line(&json!({
            "command": "fragment.inspect",
            "carrier": carrier.as_str(),
            "bytes_len": bytes.len(),
            "header": header_value(&fragment),
            "tlvs": tlv_values(&fragment.ext),
            "body": body_value(&fragment.payload),
        }))))
    } else {
        Ok(Produced::ok(render_fragment(
            &fragment,
            &bytes,
            "inspected",
        )))
    }
}

/// Read the fragment's bytes from `--hex`, `--file` or standard input.
pub fn read_input(options: &Options) -> Result<Vec<u8>, CliError> {
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

/// Read the class body from `--body` or `--body-file`.
fn read_body(options: &Options) -> Result<Vec<u8>, CliError> {
    if options.value("body").is_some() && options.value("body-file").is_some() {
        return Err(CliError::Usage(
            "only one of `--body` and `--body-file` may be given".to_string(),
        ));
    }
    if let Some(text) = options.value("body") {
        return parse_hex(text);
    }
    if let Some(path) = options.value("body-file") {
        return std::fs::read(path).map_err(|error| {
            CliError::Input(format!("the file {path:?} could not be read: {error}"))
        });
    }
    Ok(Vec::new())
}

/// Build the header extension from the `--tlv` and `--tlv-raw` flags.
fn build_extension(options: &Options) -> Result<HeaderExt, CliError> {
    let mut ext = HeaderExt::new();
    for entry in options.values("tlv") {
        let (name, text) = entry
            .split_once('=')
            .ok_or_else(|| CliError::Usage(format!("flag `--tlv`: {entry:?} is not NAME=HEX")))?;
        let kind = tlv::from_name(name).ok_or_else(|| {
            CliError::Input(format!(
                "unknown TLV name {name:?}; the registry names {:?}",
                tlv_names()
            ))
        })?;
        let value = parse_hex(text)?;
        ext.push_raw(kind, value).map_err(|reason| {
            CliError::Input(format!(
                "the header extension is not valid: {}",
                reason.as_str()
            ))
        })?;
    }
    for entry in options.values("tlv-raw") {
        let (kind, text) = entry.split_once('=').ok_or_else(|| {
            CliError::Usage(format!("flag `--tlv-raw`: {entry:?} is not TYPE=HEX"))
        })?;
        let kind: u8 = kind.parse().map_err(|_| {
            CliError::Usage(format!("flag `--tlv-raw`: {kind:?} is not a type byte"))
        })?;
        let value = parse_hex(text)?;
        ext.push_raw(kind, value).map_err(|reason| {
            CliError::Input(format!(
                "the header extension is not valid: {}",
                reason.as_str()
            ))
        })?;
    }
    Ok(ext)
}

/// Every name the TLV registry knows.
fn tlv_names() -> Vec<&'static str> {
    (tlv::UPDATE_ID..=tlv::COST_MS)
        .filter_map(tlv::name)
        .collect()
}

/// The transport class a name names.
pub fn parse_class(name: &str) -> Result<FragmentClass, CliError> {
    match name {
        "control" => Ok(FragmentClass::Control),
        "cursor" => Ok(FragmentClass::Cursor),
        "media" => Ok(FragmentClass::Media),
        "feedback" => Ok(FragmentClass::Feedback),
        "path" => Ok(FragmentClass::Path),
        "datagram" => Ok(FragmentClass::Datagram),
        "fec" => Ok(FragmentClass::Fec),
        other => Err(CliError::Usage(format!(
            "unknown class {other:?}; the classes are control, cursor, media, feedback and path"
        ))),
    }
}

/// The carrier a name names.
pub fn parse_carrier(name: &str) -> Result<Carrier, CliError> {
    match name {
        "ws" => Ok(Carrier::Ws),
        "grpc" => Ok(Carrier::Grpc),
        "dc" => Ok(Carrier::DataChannel),
        "dc-repair" => Ok(Carrier::RepairChannel),
        "dc-control" => Ok(Carrier::ControlChannel),
        other => Err(CliError::Usage(format!(
            "unknown carrier {other:?}; the carriers are ws, grpc, dc, dc-repair and dc-control"
        ))),
    }
}

/// The flag bits a comma-separated list names.
fn parse_flags(text: &str) -> Result<FragmentFlags, CliError> {
    let mut flags = FragmentFlags::NONE;
    for name in text
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let bit = match name {
            "fin" => FragmentFlags::FIN,
            "anchor" => FragmentFlags::ANCHOR,
            "retransmit" => FragmentFlags::RETRANSMIT,
            "dup" => FragmentFlags::DUP,
            "echo-requested" => FragmentFlags::ECHO_REQUESTED,
            "probe" => FragmentFlags::PROBE,
            "urgent" => FragmentFlags::URGENT,
            other => {
                return Err(CliError::Usage(format!(
                    "unknown flag {other:?}; the flags are fin, anchor, retransmit, dup, \
                     echo-requested, probe and urgent"
                )));
            }
        };
        flags = flags.union(bit);
    }
    Ok(flags)
}

/// The flag bits' names, in the order the flags byte names them.
fn flag_names(flags: FragmentFlags) -> Vec<&'static str> {
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

/// The JSON form of a fragment's fixed header.
pub fn header_value(fragment: &Fragment) -> Value {
    json!({
        "version": fragment.header.version,
        "class": fragment.header.class.as_str(),
        "flags": flag_names(fragment.header.flags),
        "priority": fragment.header.priority.get(),
        "object_id": fragment.header.object_id,
        "fragment_index": fragment.header.fragment_index,
        "fragment_count": fragment.header.fragment_count,
        "payload_len": fragment.header.payload_len,
        "header_ext_len": fragment.header.header_ext_len,
    })
}

/// The JSON form of a fragment's body.
pub fn body_value(payload: &[u8]) -> Value {
    json!({
        "length": payload.len(),
        "sha256": sha256_hex(payload),
    })
}

/// The JSON form of the header extension.
pub fn tlv_values(ext: &HeaderExt) -> Value {
    let mut entries = Vec::new();
    for entry in ext.entries() {
        let mut value = json!({
            "kind": entry.kind,
            "name": tlv::name(entry.kind),
            "classification": classification(entry.kind),
            "length": entry.value.len(),
            "value_hex": hex(&entry.value),
        });
        if let Some(decoded) = decode_tlv(entry.kind, &entry.value) {
            value
                .as_object_mut()
                .expect("the value is an object")
                .insert("value".into(), decoded);
        }
        entries.push(value);
    }
    Value::Array(entries)
}

/// How the registry treats a type.
fn classification(kind: u8) -> &'static str {
    if tlv::is_known(kind) {
        "named"
    } else if tlv::is_reserved(kind) {
        "reserved"
    } else if tlv::is_experimental(kind) {
        "experimental"
    } else {
        "unknown"
    }
}

/// The decoded value of a named TLV, when its length is the one the registry
/// declares.
fn decode_tlv(kind: u8, value: &[u8]) -> Option<Value> {
    let number = |length: usize| -> Option<u64> {
        if value.len() != length {
            return None;
        }
        let mut bytes = [0_u8; 8];
        bytes[..length].copy_from_slice(value);
        Some(u64::from_le_bytes(bytes))
    };
    match kind {
        tlv::UPDATE_ID => number(8).map(|value| json!({ "update_id": value })),
        tlv::UPDATE_BASE => number(8).map(|value| json!({ "update_base": value })),
        tlv::PART => {
            if value.len() != 4 {
                return None;
            }
            let index = u16::from_le_bytes([value[0], value[1]]);
            let count = u16::from_le_bytes([value[2], value[3]]);
            Some(json!({ "part_index": index, "part_count": count }))
        }
        tlv::UPDATE_BYTES => number(4).map(|value| json!({ "update_bytes": value })),
        tlv::CHAIN_EPOCH => number(4).map(|value| json!({ "chain_epoch": value })),
        tlv::REFLECTS_INPUT_SEQ => number(8).map(|value| json!({ "reflects_input_seq": value })),
        tlv::SEND_EPOCH_MS => number(8).map(|value| json!({ "send_epoch_ms": value })),
        tlv::DEADLINE_EPOCH_MS => number(8).map(|value| json!({ "deadline_epoch_ms": value })),
        tlv::ECHO_OF_OBJECT_ID => number(4).map(|value| json!({ "echo_of_object_id": value })),
        tlv::BUDGET_BYTES => number(4).map(|value| json!({ "budget_bytes": value })),
        tlv::PACE_BPS => number(4).map(|value| json!({ "pace_bps": value })),
        tlv::PATH_HINT => {
            if value.len() != 1 {
                return None;
            }
            Some(json!({ "path_hint": crate::media::MediaPath::from_u8(value[0]).as_str() }))
        }
        tlv::INPUT_SEQ => number(8).map(|value| json!({ "input_seq": value })),
        tlv::CELL_COUNT => number(4).map(|value| json!({ "cell_count": value })),
        tlv::COST_MS => number(2).map(|value| json!({ "cost_ms": value })),
        _ => None,
    }
}

/// The human form of a fragment.
fn render_fragment(fragment: &Fragment, bytes: &[u8], verb: &str) -> String {
    let mut text = String::new();
    text.push_str(&format!("fragment {verb}\n"));
    text.push_str(&format!("  bytes            {}\n", hex(bytes)));
    text.push_str(&format!("  version          {}\n", fragment.header.version));
    text.push_str(&format!(
        "  class            {}\n",
        fragment.header.class.as_str()
    ));
    let flags = flag_names(fragment.header.flags);
    text.push_str(&format!(
        "  flags            {}\n",
        if flags.is_empty() {
            "none".to_string()
        } else {
            flags.join(", ")
        }
    ));
    text.push_str(&format!(
        "  priority         {}\n",
        fragment.header.priority.get()
    ));
    text.push_str(&format!(
        "  object id        {}\n",
        fragment.header.object_id
    ));
    text.push_str(&format!(
        "  fragment         {} of {}\n",
        fragment.header.fragment_index, fragment.header.fragment_count
    ));
    text.push_str(&format!(
        "  header extension {} bytes\n",
        fragment.header.header_ext_len
    ));
    text.push_str(&format!(
        "  body             {} bytes\n",
        fragment.payload.len()
    ));
    text.push_str(&format!(
        "  body sha256      {}\n",
        sha256_hex(&fragment.payload)
    ));
    if !fragment.ext.is_empty() {
        text.push('\n');
        let rows: Vec<Vec<String>> = fragment
            .ext
            .entries()
            .iter()
            .map(|entry| {
                vec![
                    tlv::name(entry.kind)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("0x{:02x}", entry.kind)),
                    format!("0x{:02x}", entry.kind),
                    classification(entry.kind).to_string(),
                    hex(&entry.value),
                ]
            })
            .collect();
        text.push_str(&table(&["tlv", "type", "class", "value"], &rows));
    }
    text
}

/// The rejection reason's stable name, for the JSON output.
pub fn reject_name(reason: RejectReason) -> &'static str {
    reason.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_options;

    #[test]
    fn a_class_name_names_a_class() {
        assert_eq!(parse_class("media").expect("class"), FragmentClass::Media);
        assert_eq!(
            parse_class("datagram").expect("class"),
            FragmentClass::Datagram
        );
        assert!(parse_class("nonsense").is_err());
    }

    #[test]
    fn a_carrier_name_names_a_carrier() {
        assert_eq!(parse_carrier("ws").expect("carrier"), Carrier::Ws);
        assert!(parse_carrier("nonsense").is_err());
    }

    #[test]
    fn a_flag_list_names_the_flag_bits() {
        let flags = parse_flags("fin,anchor").expect("flags");
        assert!(flags.contains(FragmentFlags::FIN));
        assert!(flags.contains(FragmentFlags::ANCHOR));
        assert!(!flags.contains(FragmentFlags::DUP));
        assert!(parse_flags("nonsense").is_err());
    }

    #[test]
    fn an_unknown_tlv_name_is_refused_with_its_reason() {
        let error =
            build_extension(&test_options(&[("tlv", Some("NOPE=00"))])).expect_err("unknown name");
        assert_eq!(error.exit_code(), crate::cli::EXIT_INPUT);
        assert!(error.message().contains("unknown TLV name"));
    }

    #[test]
    fn a_named_tlv_carries_its_value() {
        let ext = build_extension(&test_options(&[(
            "tlv",
            Some("UPDATE_ID=0100000000000000"),
        )]))
        .expect("extension");
        assert_eq!(ext.update_id(), Some(1));
    }

    #[test]
    fn a_tlv_whose_length_is_wrong_has_no_decoded_value() {
        let ext =
            build_extension(&test_options(&[("tlv", Some("UPDATE_ID=01"))])).expect("extension");
        let values = tlv_values(&ext);
        let entries = values.as_array().expect("an array");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].get("value").is_none());
    }
}
