//! P71c: a loose GeoTIFF's `.aux.xml` supplies nodata and statistics
//! only. Georeferencing, CRS, transform, mask and scale never come from it: a sidecar stating any of
//! them is a typed refusal, and an accepted sidecar leaves the raster's own placement unchanged.

use dss_vortex::import::{open_source, OpenOptions};
use dss_vortex::source::raster::{RasterFile, ReadOptions};
use dss_vortex::VortexError;
use std::path::Path;

const BAND: &str = "<PAMRasterBand band=\"1\">";

#[test]
fn aux_xml_supplies_nodata_and_statistics_only() {
    let raster = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/raster");
    let tif = std::fs::read(raster.join("crs_4326.tif")).unwrap();
    let dir = std::env::temp_dir().join(format!("p71c_d80_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.tif");
    let aux = dir.join("a.tif.aux.xml");
    std::fs::write(&path, &tif).unwrap();
    let own = RasterFile::from_bytes("a.tif", &tif, None, ReadOptions::default()).unwrap();
    let own = own.frame().parts();

    // Accepted: nodata and statistics. Placement is the raster's own.
    let stated = own.nodata.map_or(-9999.0, f64::from);
    std::fs::write(
        &aux,
        format!(
            "<PAMDataset>{BAND}<NoDataValue>{stated:e}</NoDataValue><Metadata>\
             <MDI key=\"STATISTICS_MINIMUM\">0</MDI><MDI key=\"STATISTICS_MAXIMUM\">9</MDI>\
             </Metadata></PAMRasterBand></PAMDataset>"
        ),
    )
    .unwrap();
    let read = RasterFile::open(&path).unwrap();
    let p = read.frame().parts();
    assert_eq!((p.transform, &p.crs), (own.transform, &own.crs));
    assert_eq!(p.nodata.map(f64::from), Some(stated));

    // Refused: everything that places, masks or scales the grid, wherever the sidecar states it.
    let wrap = |dataset: &str, band: &str| {
        format!("<PAMDataset>{dataset}{BAND}{band}</PAMRasterBand></PAMDataset>")
    };
    let cases = [
        ("CRS", wrap("<SRS>GEOGCS[\"WGS 84\"]</SRS>", "")),
        ("CRS, mapping", wrap("<SRS dataAxisToSRSAxisMapping=\"2,1\">EPSG:4326</SRS>", "")),
        ("transform", wrap("<GeoTransform>0,1,0,0,0,-1</GeoTransform>", "")),
        (
            "ground control points",
            wrap("<GCPList Projection=\"EPSG:4326\"><GCP Id=\"1\" Pixel=\"0\" Line=\"0\" X=\"1\" Y=\"2\"/></GCPList>", ""),
        ),
        (
            "dataset metadata",
            wrap("<Metadata><MDI key=\"AREA_OR_POINT\">Point</MDI></Metadata>", ""),
        ),
        ("mask band", wrap("", "<MaskBand><PAMRasterBand band=\"1\"/></MaskBand>")),
        ("scale", wrap("", "<Scale>2</Scale>")),
        ("offset", wrap("", "<Offset>1</Offset>")),
        (
            "band metadata other than statistics",
            wrap("", "<Metadata><MDI key=\"UNITTYPE\">mm</MDI></Metadata>"),
        ),
        (
            "metadata domain",
            wrap("", "<Metadata domain=\"IMAGE_STRUCTURE\"><MDI key=\"STATISTICS_MINIMUM\">0</MDI></Metadata>"),
        ),
        ("unit type", wrap("", "<UnitType>mm</UnitType>")),
        (
            "another band",
            "<PAMDataset><PAMRasterBand band=\"2\"><NoDataValue>0</NoDataValue></PAMRasterBand></PAMDataset>".to_string(),
        ),
    ];
    for (what, xml) in &cases {
        std::fs::write(&aux, xml).unwrap();
        match RasterFile::open(&path) {
            Err(e @ VortexError::Refused { .. }) => {
                assert!(e.to_string().contains(".aux.xml"), "{what}: {e}")
            }
            other => panic!("{what}: {other:?}"),
        }
        assert!(
            open_source(&path, &OpenOptions::default()).is_err(),
            "{what}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
