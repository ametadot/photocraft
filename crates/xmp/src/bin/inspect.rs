//! Read-only headless inspector: an explicit XMP path is passed by the caller.
//! This is NOT PhotoCraft's authorised engine/GUI file-open operation.
#![forbid(unsafe_code)]

use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use photocraft_xmp::{MAX_XMP_BYTES, ValueState, XmpPacket};

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_field(value: ValueState<'_>) -> String {
    match value {
        ValueState::Absent => "{\"kind\":\"absent\",\"application_status\":\"not_applied\"}".to_owned(),
        ValueState::Text(text) => format!("{{\"kind\":\"text\",\"lexical_value\":{},\"application_status\":\"not_applied\"}}", json_string(text),),
        ValueState::Resource(uri) => {
            format!("{{\"kind\":\"resource\",\"uri\":{},\"application_status\":\"unsupported\",\"reason\":\"rdf_resource_not_mapped\"}}", json_string(uri),)
        }
        ValueState::Opaque => "{\"kind\":\"opaque\",\"application_status\":\"unsupported\",\"reason\":\"structured_rdf_not_interpreted\"}".to_owned(),
    }
}

fn inspection_json(packet: &XmpPacket) -> String {
    let inspection = packet.inspection();
    let mut output = String::from("{\"schema_version\":2,\"read_only\":true,\"settings_applied_to_image\":false,\"camera_raw\":{");
    for (i, field) in inspection.settings.iter().enumerate() {
        if i > 0 {
            output.push(',');
        }
        output.push_str(&json_string(field.name));
        output.push(':');
        output.push_str(&json_field(field.value.clone()));
    }
    output.push_str("},\"snapshots\":[");
    for (i, name) in inspection.snapshots.iter().enumerate() {
        if i > 0 {
            output.push(',');
        }
        output.push_str("{\"name\":");
        output.push_str(&name.map(json_string).unwrap_or_else(|| "null".to_owned()));
        output.push('}');
    }
    output.push_str("],\"active_description_count\":");
    output.push_str(&inspection.active_description_count.to_string());
    output.push_str(",\"extracted_camera_raw_fields\":");
    output.push_str(&inspection.settings.len().to_string());
    output.push_str(",\"unsupported_structured_fields\":");
    output.push_str(&inspection.unsupported_fields.to_string());
    output.push_str(",\"warnings\":[");
    for (i, warning) in inspection.warnings.iter().enumerate() {
        if i > 0 {
            output.push(',');
        }
        output.push_str(&json_string(warning));
    }
    output.push_str("],\"adobe_reopen\":\"NOT_RUN\",\"windows_ui\":\"NOT_RUN\"}");
    output
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args_os().collect();
    if args.len() != 2 {
        return Err("usage: photocraft-xmp-inspect <explicit read-only .xmp path>".into());
    }
    let path = Path::new(&args[1]);
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file).take((MAX_XMP_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    let packet = XmpPacket::parse(&bytes)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", inspection_json(&packet))?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("XMP inspect failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_xmp::{CAMERA_RAW_NS, RDF_NS};

    #[test]
    fn report_makes_read_only_status_and_white_balance_block_explicit() -> Result<(), Box<dyn std::error::Error>> {
        let input = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}">
            <rdf:Description crs:Exposure2012="0.7853281" crs:Temperature="6017" />
        </rdf:RDF>"#
        );
        let packet = XmpPacket::parse(input.as_bytes())?;
        let output = inspection_json(&packet);
        assert!(output.contains("\"settings_applied_to_image\":false"));
        assert!(output.contains("\"lexical_value\":\"0.7853281\""));
        assert!(output.contains("absolute Kelvin"));
        assert!(output.contains("\"windows_ui\":\"NOT_RUN\""));
        Ok(())
    }
}
