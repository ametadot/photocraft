use super::*;
use photocraft_codecs::{EncodeOptions, Image, SampleType, encode};

fn xmp(attributes: &str) -> String {
    format!(
        "<r:RDF xmlns:r=\"{}\" xmlns:c=\"{}\" xmlns:x=\"{}\"><r:Description {attributes}/></r:RDF>",
        photocraft_xmp::RDF_NS,
        photocraft_xmp::CAMERA_RAW_NS,
        photocraft_xmp::XMP_NS
    )
}

fn photograph(sample: SampleType) -> Image {
    Image::from_normalized(2, 2, ChannelLayout::Rgb, sample, &[0.1, 0.2, 0.3, 0.2, 0.3, 0.4, 0.3, 0.4, 0.5, 0.4, 0.5, 0.6])
        .unwrap()
        .with_icc(Some(photocraft_cms::Builtin::AdobeRgbCompat.profile().to_bytes().to_vec()))
}

#[test]
fn settings_detection_requires_current_active_adjustments() {
    for attributes in ["", "x:Rating=\"5\"", "c:Version=\"15.0\" c:ProcessVersion=\"11.0\"", "c:HasSettings=\"False\" c:Exposure2012=\"2\""] {
        assert!(!has_settings(xmp(attributes).as_bytes()).unwrap());
    }
    assert!(has_settings(xmp("c:Exposure2012=\"2\"").as_bytes()).unwrap());
    assert!(has_settings(xmp("c:HasSettings=\"True\"").as_bytes()).unwrap());
    assert!(has_settings(xmp("c:HasSettings=\"maybe\"").as_bytes()).is_err());
    let historical = xmp("c:Exposure2012=\"2\"").replace("<r:Description ", "<r:Description r:about=\"history\" ");
    assert!(!has_settings(historical.as_bytes()).unwrap());
}

#[test]
fn actual_sensor_tiff_content_precedes_rendered_tiff_routing() {
    let raw = photocraft_raw::testgen::tiff_ep("NIKON CORPORATION", 16, 16, &[1024; 256], [0, 1, 1, 2], 12, vec![]);
    let found = inspect(&raw).unwrap();
    assert_eq!(found.kind, SourceKind::Raw);
    assert!(found.supported);
    let png = encode(&photograph(SampleType::U8), Format::Png, &EncodeOptions::default()).unwrap();
    assert_eq!(inspect(&png).unwrap().kind, SourceKind::Other);
    assert!(initial_recipe(&png, None).is_err());
}

#[test]
fn jpeg_and_tiff_read_embedded_settings_and_sidecar_precedence() {
    for format in [Format::Jpeg, Format::Tiff] {
        let mut image = photograph(SampleType::U8);
        image.meta.xmp = Some(xmp("c:Exposure2012=\"0.75\" x:Rating=\"4\" x:Label=\"Client review\""));
        let bytes = encode(&image, format, &EncodeOptions::default()).unwrap();
        let found = inspect(&bytes).unwrap();
        assert!(found.supported && found.has_settings);
        let recipe = initial_recipe(&bytes, None).unwrap();
        assert_eq!(recipe.adjustments.exposure, 0.75);
        assert_eq!(recipe.metadata.rating, Some(4));
        assert_eq!(recipe.metadata.label, "Client review");
        assert_eq!(recipe.processing.source_kind, if format == Format::Jpeg { SourceKind::Jpeg } else { SourceKind::Tiff });
        assert_eq!(recipe.processing.demosaic, "none");
        assert!((recipe.adjustments.encoding_gamma - 563.0 / 256.0).abs() < 0.0001);
        let sidecar = xmp("c:HasSettings=\"False\" c:Exposure2012=\"2\" x:Rating=\"1\"");
        assert!(!inspect_with_sidecar(&bytes, Some(sidecar.as_bytes())).unwrap().has_settings);
        let sidecar_recipe = initial_recipe(&bytes, Some(sidecar.as_bytes())).unwrap();
        assert_eq!(sidecar_recipe.adjustments.exposure, 0.0);
        assert_eq!(sidecar_recipe.metadata.rating, Some(1));
    }
}

#[test]
fn layered_tiff_is_rejected_before_pixels_or_metadata_are_modified() {
    let mut image = photograph(SampleType::U16);
    image.meta.photoshop_layers = Some(b"Adobe Photoshop Document Data Block\0damaged layer payload".to_vec());
    let bytes = encode(&image, Format::Tiff, &EncodeOptions::default()).unwrap();
    let found = inspect(&bytes).unwrap();
    assert_eq!(found.kind, SourceKind::Tiff);
    assert!(found.layered_tiff);
    assert!(!found.supported);
    assert!(capture_processing(&bytes).is_err());
    assert!(initial_recipe(&bytes, None).is_err());
}

#[test]
fn rendered_admission_keeps_native_depth_profile_and_metadata() {
    for sample in [SampleType::U8, SampleType::U16, SampleType::F32] {
        let mut image = photograph(sample);
        image.meta.xmp = Some(xmp("x:Rating=\"3\""));
        let bytes = encode(&image, Format::Tiff, &EncodeOptions::default()).unwrap();
        let recipe = initial_recipe(&bytes, None).unwrap();
        let imported = import_rendered_recipe("misnamed.NEF", &bytes, &recipe).unwrap().document;
        let expected = match sample {
            SampleType::U8 => photocraft_color::SampleType::U8,
            SampleType::U16 => photocraft_color::SampleType::U16,
            _ => photocraft_color::SampleType::F32,
        };
        assert_eq!(imported.depth, expected);
        assert_eq!(imported.icc_profile.as_deref().map(Vec::as_slice), image.icc.as_deref());
        assert_eq!(imported.metadata.xmp, image.meta.xmp);
        let mut raw_recipe = recipe;
        raw_recipe.processing.source_kind = SourceKind::Raw;
        assert!(import_rendered_recipe("misnamed.NEF", &bytes, &raw_recipe).is_err());
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn rendered_gray_and_float_development_retains_alpha_depth_and_hdr() {
    use crate::raw_session::controller::render_recipe;
    let image = Image::from_f32(1, 1, ChannelLayout::GrayA, &[0.25, 0.375]).unwrap().with_icc(Some(photocraft_cms::builtin::linear_gray().to_bytes().to_vec()));
    let bytes = encode(&image, Format::Tiff, &EncodeOptions::default()).unwrap();
    let mut recipe = initial_recipe(&bytes, None).unwrap();
    assert!((recipe.adjustments.encoding_gamma - 1.0).abs() < 0.0001);
    recipe.adjustments.exposure = 1.0;
    let imported = render_recipe("gray.tif", &bytes, &recipe).unwrap().document;
    assert_eq!(imported.depth, photocraft_color::SampleType::F32);
    assert_eq!(imported.mode, photocraft_color::ColorMode::Grayscale);
    let pixel = imported.layers[0].surface().unwrap().pixel(0, 0);
    assert!(pixel[0] > 0.3 && pixel[0] < 0.4, "{pixel:?}");
    assert!((pixel[1] - 0.375).abs() < 0.0001, "{pixel:?}");
    assert_eq!(imported.icc_profile.as_deref().map(Vec::as_slice), image.icc.as_deref());

    let image =
        Image::from_f32(1, 1, ChannelLayout::Rgb, &[0.75, 0.8, 0.9]).unwrap().with_icc(Some(photocraft_cms::Builtin::LinearSrgb.profile().to_bytes().to_vec()));
    let bytes = encode(&image, Format::Tiff, &EncodeOptions::default()).unwrap();
    let mut recipe = initial_recipe(&bytes, None).unwrap();
    recipe.adjustments.exposure = 1.0;
    let imported = render_recipe("hdr.tif", &bytes, &recipe).unwrap().document;
    let pixel = imported.layers[0].surface().unwrap().pixel(0, 0);
    assert!(pixel[0] > 1.0 && pixel[1] > 1.0 && pixel[2] > 1.0, "{pixel:?}");
}

#[test]
fn mismatched_profile_and_sensor_options_are_rejected() {
    let image = photograph(SampleType::U8).with_icc(Some(photocraft_cms::Builtin::GrayGamma22.profile().to_bytes().to_vec()));
    let bytes = encode(&image, Format::Tiff, &EncodeOptions::default()).unwrap();
    assert!(initial_recipe(&bytes, None).is_err());
    let bytes = encode(&photograph(SampleType::U8), Format::Jpeg, &EncodeOptions::default()).unwrap();
    let mut recipe = initial_recipe(&bytes, None).unwrap();
    recipe.processing.demosaic = "mhc".into();
    assert!(recipe.validate().is_err());
}
