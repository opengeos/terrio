//! CRS keyword and root-identifier behavior, compared with GDAL 3.13.3.

use terrio_crs::SpatialRef;

#[test]
fn engineering_authority_does_not_enable_an_earth_transform() {
    let earth = SpatialRef::from_epsg(4326).unwrap();
    for root in ["LOCAL_CS", "ENGCRS", "ENGINEERINGCRS"] {
        let text = format!("{root}[\"site grid\",UNIT[\"metre\",1],AUTHORITY[\"EPSG\",\"32615\"]]");
        let local = SpatialRef::parse_literal(&text).unwrap();
        assert!(terrio_crs::Transformer::new(&local, &earth).is_err(), "{root}");
        assert!(terrio_crs::Transformer::new(&earth, &local).is_err(), "{root}");
        let mut explicit = local.clone();
        explicit.epsg = None;
        explicit.proj4 = Some("+proj=longlat +datum=WGS84".into());
        assert!(terrio_crs::Transformer::new(&explicit, &earth).is_ok(), "explicit PROJ override of {root}");
    }
}

#[test]
fn engineering_definitions_keep_their_wkt_without_an_epsg_guess() {
    let definitions = [
        "LOCAL_CS[\"site grid\",LOCAL_DATUM[\"site\",32767],UNIT[\"metre\",1],AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]]",
        "ENGCRS[\"site grid\",EDATUM[\"site\"],CS[Cartesian,2],AXIS[\"(E)\",east,ORDER[1]],AXIS[\"(N)\",north,ORDER[2]],LENGTHUNIT[\"metre\",1]]",
        "ENGINEERINGCRS[\"site grid\",ENGINEERINGDATUM[\"site\"],CS[Cartesian,2],AXIS[\"(E)\",east,ORDER[1]],AXIS[\"(N)\",north,ORDER[2]],LENGTHUNIT[\"metre\",1,ID[\"EPSG\",9001]]]",
    ];
    for definition in definitions {
        for text in [definition.to_string(), definition.to_ascii_lowercase()] {
            for parsed in [SpatialRef::parse(&text), SpatialRef::parse_literal(&text)] {
                let parsed = parsed.unwrap();
                assert!(!parsed.is_unknown());
                assert_eq!(parsed.epsg, None);
                assert_eq!(parsed.name().as_deref(), Some("site grid"));
                assert_eq!(parsed.to_wkt().as_deref(), Some(text.as_str()));
            }
        }
    }
}

#[test]
fn long_vertical_keyword_uses_the_crs_identifier_instead_of_nested_identifiers() {
    for nested in ["", ",ID[\"EPSG\",9001]"] {
        for identifier in ["", ",ID[\"EPSG\",5703]"] {
            let body = format!(
                "[\"NAVD88 height\",VDATUM[\"North American Vertical Datum 1988\",ID[\"EPSG\",5103]],CS[vertical,1],AXIS[\"gravity-related height (H)\",up],LENGTHUNIT[\"metre\",1{nested}]{identifier}]"
            );
            let short = SpatialRef::parse_literal(&format!("VERTCRS{body}")).unwrap();
            for keyword in ["VERTICALCRS", "verticalcrs"] {
                let text = format!("{keyword}{body}");
                for parsed in [SpatialRef::parse(&text), SpatialRef::parse_literal(&text)] {
                    let parsed = parsed.unwrap();
                    assert_eq!(parsed.epsg, if identifier.is_empty() { None } else { Some(5703) });
                    assert_eq!(parsed.epsg, short.epsg);
                    assert_eq!(parsed.to_wkt().as_deref(), Some(text.as_str()));
                }
            }
        }
    }
}

#[test]
fn recognized_wkt_keywords_do_not_bypass_syntax_validation() {
    for text in ["LOCAL_CS[\"broken\"", "ENGCRS[\"broken\"", "ENGINEERINGCRS[\"broken\"", "VERTICALCRS[\"broken\",ID[\"EPSG\",9001]"] {
        assert!(SpatialRef::parse_literal(text).is_err(), "{text}");
    }
}

/// Long vertical components retain the same identity as GDAL 3.13.3's short spelling.
#[test]
fn compound_long_vertical_component_retains_identity_and_wkt() {
    let horizontal = SpatialRef::from_epsg(4326).unwrap().to_wkt().unwrap();
    let vertical = "VERTICALCRS[\"NAVD88 height\",VDATUM[\"North American Vertical Datum 1988\"],CS[vertical,1],AXIS[\"height\",up],LENGTHUNIT[\"metre\",1],ID[\"EPSG\",5703]]";
    let text = format!("COMPOUNDCRS[\"WGS 84 and NAVD88\",{horizontal},{vertical}]");
    let parsed = SpatialRef::parse_literal(&text).unwrap();
    assert_eq!(parsed.compound_parts(), Some((4326, 5703)));
    assert_eq!(parsed.to_wkt().as_deref(), Some(text.as_str()));
}
