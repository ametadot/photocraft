//! Shared read-only inspection representation for CLI, engine and desktop UI.
//! This is intentionally not an Adobe-to-PhotoCraft development-settings map.

use crate::{CAMERA_RAW_NS, ValueState, XmpPacket, XmpValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationStatus {
    /// Parsed exactly as metadata, never applied to image rendering.
    NotApplied,
    /// Opaque structure/resource cannot be interpreted as a scalar slider.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedSetting<'a> {
    pub name: &'a str,
    pub value: ValueState<'a>,
    pub application: ApplicationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmpInspection<'a> {
    pub settings: Vec<InspectedSetting<'a>>,
    pub snapshots: Vec<Option<&'a str>>,
    pub active_description_count: usize,
    pub unsupported_fields: usize,
    pub warnings: Vec<&'static str>,
}

impl XmpPacket {
    /// The engine, desktop and CLI should all consume this same inspection.
    /// This is not a rendering compatibility promise and does not open files.
    pub fn inspection(&self) -> XmpInspection<'_> {
        let mut settings = Vec::new();
        let mut unsupported_fields = 0usize;
        let mut white_balance_present = false;
        let mut process_version_present = false;
        for (name, value) in self.active_properties() {
            if name.namespace_uri != CAMERA_RAW_NS {
                continue;
            }
            white_balance_present |= matches!(name.local_name.as_str(), "WhiteBalance" | "Temperature" | "CustomTemperature" | "Tint" | "CustomTint");
            process_version_present |= name.local_name == "ProcessVersion";
            let (value, application) = match value {
                XmpValue::Text(v) => (ValueState::Text(v.as_str()), ApplicationStatus::NotApplied),
                XmpValue::Resource(v) => (ValueState::Resource(v.as_str()), ApplicationStatus::Unsupported),
                XmpValue::Opaque { .. } => (ValueState::Opaque, ApplicationStatus::Unsupported),
            };
            if application == ApplicationStatus::Unsupported {
                unsupported_fields += 1;
            }
            settings.push(InspectedSetting { name: name.local_name.as_str(), value, application });
        }
        let mut warnings = vec!["Metadata inspection only; no Adobe settings were applied to PhotoCraft's renderer"];
        if process_version_present {
            warnings.push("The Camera Raw process version is preserved but not mapped to PhotoCraft rendering");
        }
        if white_balance_present {
            warnings.push("Adobe absolute Kelvin and tint have not been mapped to PhotoCraft relative controls");
        }
        if unsupported_fields > 0 {
            warnings.push("Structured or resource-valued Camera Raw metadata remains unsupported and unchanged");
        }
        XmpInspection {
            settings,
            snapshots: self.snapshots().iter().map(|s| s.name.as_deref()).collect(),
            active_description_count: self.active_description_count(),
            unsupported_fields,
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RDF_NS;

    #[test]
    fn inspection_keeps_rendering_unapplied() -> Result<(), Box<dyn std::error::Error>> {
        let xml = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}">
           <rdf:Description crs:ProcessVersion="11.0" crs:Exposure2012="0.7853281" crs:Temperature="6017" />
           </rdf:RDF>"#
        );
        let packet = XmpPacket::parse(xml.as_bytes())?;
        let summary = packet.inspection();
        assert!(summary.settings.iter().all(|s| s.application == ApplicationStatus::NotApplied));
        assert!(summary.warnings.iter().any(|w| w.contains("absolute Kelvin")));
        assert!(summary.warnings.iter().any(|w| w.contains("process version")));
        Ok(())
    }

    #[test]
    fn structured_field_is_unsupported_not_zero() -> Result<(), Box<dyn std::error::Error>> {
        let xml = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}">
           <rdf:Description><crs:ToneCurvePV2012><rdf:Seq><rdf:li>0, 0</rdf:li></rdf:Seq></crs:ToneCurvePV2012></rdf:Description>
           </rdf:RDF>"#
        );
        let packet = XmpPacket::parse(xml.as_bytes())?;
        let summary = packet.inspection();
        assert_eq!(summary.unsupported_fields, 1);
        assert_eq!(summary.settings[0].value, ValueState::Opaque);
        assert_eq!(summary.settings[0].application, ApplicationStatus::Unsupported);
        Ok(())
    }
}
