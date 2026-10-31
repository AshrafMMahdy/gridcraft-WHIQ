//! WHIQ: scatter series with their own line / markers / width write a solid `a:ln` (or
//! `a:noFill`) and a marker or `c:symbol none`, and read back the same.

use gridcraft_model::{Anchor, Chart, ChartKind, Color, LegendPos, Series, Sheet, Workbook};
use std::sync::Arc;

use crate::{read_xlsx, write_xlsx};

fn series(line: Option<bool>, markers: Option<bool>, width: Option<f32>, color: Option<Color>) -> Series {
    Series {
        categories: Some("Data!$A$1:$A$3".into()),
        values: "Data!$B$1:$B$3".into(),
        color,
        line,
        markers,
        line_width: width,
        ..Series::default()
    }
}

fn chart_xml(kind: ChartKind, series: Vec<Series>) -> (String, Chart) {
    let mut wb = Workbook::default();
    let mut s = Sheet::new("Data");
    s.charts.push(Chart {
        id: 1,
        kind,
        anchor: Anchor { width: 400.0, height: 300.0, ..Anchor::default() },
        title: None,
        series,
        legend: LegendPos::Bottom,
        data_labels: false,
        gridlines: true,
        style: 2,
        x_title: None,
        y_title: None,
        y2_title: None,
        source: None,
        by_rows: false,
        pivot: None,
    });
    wb.sheets = vec![Arc::new(s)];
    let bytes = write_xlsx(&wb).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("xl/charts/chart1.xml").unwrap(), &mut xml).unwrap();
    let (back, _) = read_xlsx(&bytes).unwrap();
    (xml, back.sheets[0].charts[0].clone())
}

#[test]
fn styled_scatter_series_write_solid_lines() {
    let red = Color::from_hex("C00000");
    let (xml, back) =
        chart_xml(ChartKind::ScatterLines, vec![series(Some(true), Some(false), Some(3.0), red), series(Some(true), Some(true), None, None)]);
    assert!(xml.contains("<c:scatterStyle val=\"lineMarker\"/>"), "{xml}");
    assert!(xml.contains("<c:spPr><a:ln w=\"38100\" cap=\"rnd\"><a:solidFill><a:srgbClr val=\"C00000\"/></a:solidFill><a:round/></a:ln></c:spPr><c:marker><c:symbol val=\"none\"/></c:marker>"), "{xml}");
    // No colour: the theme's second accent, on the line and the marker.
    assert_eq!(xml.matches("<a:ln w=\"28575\" cap=\"rnd\"><a:solidFill><a:srgbClr").count(), 1, "{xml}");
    assert!(xml.contains("<c:symbol val=\"circle\"/>"), "{xml}");
    assert!(!xml.contains("noFill"), "{xml}");
    assert_eq!(back.kind, ChartKind::ScatterLines);
    assert_eq!((back.series[0].line, back.series[0].markers, back.series[0].line_width), (None, Some(false), Some(3.0)));
    assert_eq!(back.series[0].color, red);
    assert_eq!(back.series[1].markers, Some(true));
    assert!(back.series[1].color.is_some());
}

#[test]
fn markers_only_and_unstyled_scatter_keep_their_xml() {
    let (xml, back) = chart_xml(ChartKind::Scatter, vec![series(Some(false), Some(true), None, None)]);
    assert!(xml.contains("<a:ln w=\"19050\"><a:noFill/></a:ln>") && xml.contains("<c:symbol val=\"circle\"/>"), "{xml}");
    assert_eq!(back.kind, ChartKind::Scatter);
    // Without the options a scatter is written exactly as before.
    let (plain, back) = chart_xml(ChartKind::ScatterLines, vec![series(None, None, None, None)]);
    assert!(!plain.contains("<c:spPr><a:ln") && plain.contains("<c:marker><c:symbol val=\"none\"/></c:marker>"), "{plain}");
    assert_eq!((back.series[0].line, back.series[0].markers, back.series[0].line_width), (None, None, None));
    let (pts, _) = chart_xml(ChartKind::Scatter, vec![series(None, None, None, None)]);
    assert!(pts.contains("<c:spPr><a:ln w=\"19050\"><a:noFill/></a:ln></c:spPr>") && !pts.contains("<c:marker>"), "{pts}");
}
