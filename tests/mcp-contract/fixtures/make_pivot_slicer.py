#!/usr/bin/env python3
"""Writes pivot-slicer.xlsx: a workbook laid out the way Excel 2016 saves a PivotTable with
slicers, so the reader is tested against Excel's own structure (not GridCraft's writer).

- Data!A1:C9: Region, Product, Units.
- Pivot!A3: PivotTable1 (rows Region, values Sum of Units, style PivotStyleMedium9), its cache
  with cacheId 5 in workbook.xml and x14 pivotCacheId 2 (the id slicer caches use).
- Two PivotTable slicers on Pivot: "Product" (a field on no axis; only Bolts selected, so the
  report shows the Bolts units) and "Region" (a row field; all selected).
- Custom document properties (an unmodelled part that must survive).

Run: python3 make_pivot_slicer.py  (writes pivot-slicer.xlsx next to this script).
"""
import os
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
MAIN = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
X14 = "http://schemas.microsoft.com/office/spreadsheetml/2009/9/main"
DECL = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'

rows = [
    ("North", "Bolts", 10), ("South", "Nuts", 20), ("North", "Nuts", 5), ("East", "Bolts", 7),
    ("South", "Bolts", 3), ("East", "Nuts", 8), ("North", "Bolts", 2), ("West", "Bolts", 4),
]
regions = ["North", "South", "East", "West"]
products = ["Bolts", "Nuts"]

def cell_s(ref, text):
    return f'<c r="{ref}" t="inlineStr"><is><t>{text}</t></is></c>'

def cell_n(ref, v, s=None):
    st = f' s="{s}"' if s is not None else ""
    return f'<c r="{ref}"{st}><v>{v}</v></c>'

data_rows = ['<row r="1">' + cell_s("A1", "Region") + cell_s("B1", "Product") + cell_s("C1", "Units") + "</row>"]
for i, (r, p, u) in enumerate(rows, start=2):
    data_rows.append(f'<row r="{i}">' + cell_s(f"A{i}", r) + cell_s(f"B{i}", p) + cell_n(f"C{i}", u) + "</row>")
sheet1 = DECL + f'<worksheet xmlns="{MAIN}" xmlns:r="{R}"><dimension ref="A1:C9"/><sheetData>' + "".join(data_rows) + "</sheetData></worksheet>"

# The report as Excel last computed it: only Bolts (the Product slicer's selection).
bolts = {}
for r, p, u in rows:
    if p == "Bolts":
        bolts[r] = bolts.get(r, 0) + u
order = [r for r in regions if r in bolts]
rep = ['<row r="3">' + cell_s("A3", "Row Labels") + cell_s("B3", "Sum of Units") + "</row>"]
n = 4
for r in order:
    rep.append(f'<row r="{n}">' + cell_s(f"A{n}", r) + cell_n(f"B{n}", bolts[r]) + "</row>")
    n += 1
rep.append(f'<row r="{n}">' + cell_s(f"A{n}", "Grand Total") + cell_n(f"B{n}", sum(bolts.values())) + "</row>")
last = n
sheet2 = (
    DECL
    + f'<worksheet xmlns="{MAIN}" xmlns:r="{R}" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:x14="{X14}" mc:Ignorable="x14ac">'
    + f'<dimension ref="A3:B{last}"/><sheetData>' + "".join(rep) + "</sheetData>"
    + '<pageMargins left="0.7" right="0.7" top="0.75" bottom="0.75" header="0.3" footer="0.3"/><drawing r:id="rId3"/>'
    + f'<extLst><ext uri="{{A8765BA9-456A-4dab-B4F3-ACF838C121DE}}" xmlns:x14="{X14}"><x14:slicerList><x14:slicer r:id="rId2"/></x14:slicerList></ext></extLst>'
    + "</worksheet>"
)

def shared(values):
    return "".join(f'<s v="{v}"/>' for v in values)

recs = "".join(f'<r><x v="{regions.index(r)}"/><x v="{products.index(p)}"/><n v="{u}"/></r>' for r, p, u in rows)
cache_def = (
    DECL
    + f'<pivotCacheDefinition xmlns="{MAIN}" xmlns:r="{R}" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" mc:Ignorable="xr" r:id="rId1" refreshedBy="Excel" refreshedDate="45000.5" createdVersion="6" refreshedVersion="6" minRefreshableVersion="3" recordCount="{len(rows)}">'
    + '<cacheSource type="worksheet"><worksheetSource ref="A1:C9" sheet="Data"/></cacheSource><cacheFields count="3">'
    + f'<cacheField name="Region" numFmtId="0"><sharedItems count="4">{shared(regions)}</sharedItems></cacheField>'
    + f'<cacheField name="Product" numFmtId="0"><sharedItems count="2">{shared(products)}</sharedItems></cacheField>'
    + '<cacheField name="Units" numFmtId="0"><sharedItems containsSemiMixedTypes="0" containsString="0" containsNumber="1" containsInteger="1" minValue="2" maxValue="20"/></cacheField>'
    + f'</cacheFields><extLst><ext uri="{{725AE2AE-9491-48be-B2B4-4EB974FC3084}}" xmlns:x14="{X14}"><x14:pivotCacheDefinition pivotCacheId="2"/></ext></extLst></pivotCacheDefinition>'
)
cache_rec = DECL + f'<pivotCacheRecords xmlns="{MAIN}" xmlns:r="{R}" count="{len(rows)}">{recs}</pivotCacheRecords>'

region_items = "".join(f'<item x="{i}"/>' for i in range(4)) + '<item t="default"/>'
product_items = '<item x="0"/><item h="1" x="1"/><item t="default"/>'
pivot_table = (
    DECL
    + f'<pivotTableDefinition xmlns="{MAIN}" name="PivotTable1" cacheId="5" applyNumberFormats="0" applyBorderFormats="0" applyFontFormats="0" applyPatternFormats="0" applyAlignmentFormats="0" applyWidthHeightFormats="1" dataCaption="Values" updatedVersion="6" minRefreshableVersion="3" useAutoFormatting="1" itemPrintTitles="1" createdVersion="6" indent="0" outline="1" outlineData="1" multipleFieldFilters="0">'
    + f'<location ref="A3:B{last}" firstHeaderRow="1" firstDataRow="1" firstDataCol="1"/>'
    + '<pivotFields count="3">'
    + f'<pivotField axis="axisRow" showAll="0"><items count="5">{region_items}</items></pivotField>'
    + f'<pivotField showAll="0"><items count="3">{product_items}</items></pivotField>'
    + '<pivotField dataField="1" showAll="0"/></pivotFields>'
    + f'<rowFields count="1"><field x="0"/></rowFields><rowItems count="{len(order) + 1}">'
    + "".join(f'<i><x v="{regions.index(r)}"/></i>' for r in order)
    + '<i t="grand"><x/></i></rowItems><colItems count="1"><i/></colItems>'
    + '<dataFields count="1"><dataField name="Sum of Units" fld="2" baseField="0" baseItem="0"/></dataFields>'
    + '<pivotTableStyleInfo name="PivotStyleMedium9" showRowHeaders="1" showColHeaders="1" showRowStripes="0" showColStripes="0" showLastColumn="1"/>'
    + f'<extLst><ext uri="{{962EF5D1-5CA2-4c93-8EF4-DBF5C05439D2}}" xmlns:x14="{X14}"><x14:pivotTableDefinition hideValuesRow="1" xmlns:xm="http://schemas.microsoft.com/office/excel/2006/main"/></ext></extLst>'
    + "</pivotTableDefinition>"
)

def slicer_cache(name, source, items):
    return (
        DECL
        + f'<slicerCacheDefinition xmlns="{X14}" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" mc:Ignorable="x" xmlns:x="{MAIN}" name="{name}" sourceName="{source}">'
        + '<pivotTables><pivotTable tabId="2" name="PivotTable1"/></pivotTables>'
        + f'<data><tabular pivotCacheId="2"><items count="{len(items)}">'
        + "".join(('<i x="%d" s="1"/>' % i) if s else ('<i x="%d"/>' % i) for i, s in items)
        + "</items></tabular></data></slicerCacheDefinition>"
    )

slicer_cache1 = slicer_cache("Slicer_Product", "Product", [(0, True), (1, False)])
slicer_cache2 = slicer_cache("Slicer_Region", "Region", [(i, True) for i in range(4)])
slicers = (
    DECL
    + f'<slicers xmlns="{X14}" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" mc:Ignorable="x xr10" xmlns:x="{MAIN}" xmlns:xr10="http://schemas.microsoft.com/office/spreadsheetml/2016/revision10">'
    + '<slicer name="Product" cache="Slicer_Product" caption="Product" rowHeight="241300"/>'
    + '<slicer name="Region" cache="Slicer_Region" caption="Region" columnCount="2" rowHeight="241300"/></slicers>'
)

def frame(name, col, row, col2, row2, id_):
    return (
        f'<xdr:twoCellAnchor editAs="oneCell"><xdr:from><xdr:col>{col}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{row}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>'
        + f'<xdr:to><xdr:col>{col2}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{row2}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>'
        + '<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:Choice xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main" Requires="a14">'
        + f'<xdr:graphicFrame macro=""><xdr:nvGraphicFramePr><xdr:cNvPr id="{id_}" name="{name}"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></xdr:xfrm>'
        + f'<a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/drawing/2010/slicer"><sle:slicer xmlns:sle="http://schemas.microsoft.com/office/drawing/2010/slicer" name="{name}"/></a:graphicData></a:graphic></xdr:graphicFrame>'
        + '</mc:Choice><mc:Fallback><xdr:sp macro="" textlink=""><xdr:nvSpPr><xdr:cNvPr id="0" name=""/><xdr:cNvSpPr><a:spLocks noTextEdit="1"/></xdr:cNvSpPr></xdr:nvSpPr><xdr:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="1828800" cy="2524125"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr>'
        + '<xdr:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1100"/><a:t>This shape represents a slicer.</a:t></a:r></a:p></xdr:txBody></xdr:sp></mc:Fallback></mc:AlternateContent><xdr:clientData/></xdr:twoCellAnchor>'
    )

drawing = (
    DECL
    + '<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">'
    + frame("Product", 4, 2, 7, 15, 2) + frame("Region", 8, 2, 11, 15, 3) + "</xdr:wsDr>"
)

workbook = (
    DECL
    + f'<workbook xmlns="{MAIN}" xmlns:r="{R}" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:x15="http://schemas.microsoft.com/office/spreadsheetml/2010/11/main" mc:Ignorable="x15">'
    + '<workbookPr defaultThemeVersion="164011"/><bookViews><workbookView activeTab="1"/></bookViews>'
    + '<sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Pivot" sheetId="2" r:id="rId2"/></sheets>'
    + '<definedNames><definedName name="Slicer_Product">#N/A</definedName><definedName name="Slicer_Region">#N/A</definedName></definedNames>'
    + '<calcPr calcId="162913"/><pivotCaches><pivotCache cacheId="5" r:id="rId3"/></pivotCaches>'
    + f'<extLst><ext uri="{{BBE1A952-AA13-448e-AADC-164F8A28A991}}" xmlns:x14="{X14}"><x14:slicerCaches><x14:slicerCache r:id="rId4"/><x14:slicerCache r:id="rId5"/></x14:slicerCaches></ext></extLst>'
    + "</workbook>"
)

REL = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
def rels(items):
    body = "".join(f'<Relationship Id="{i}" Type="{t}" Target="{g}"/>' for i, t, g in items)
    return DECL + f'<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{body}</Relationships>'

styles = DECL + f'<styleSheet xmlns="{MAIN}"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf/></cellStyleXfs><cellXfs count="1"><xf/></cellXfs></styleSheet>'
custom = DECL + '<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><property fmtid="{D5CDD505-2E9C-101B-9397-08002B2CF9AE}" pid="2" name="Project"><vt:lpwstr>CRC-17A</vt:lpwstr></property></Properties>'

ct = (
    DECL
    + '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>'
    + '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
    + '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
    + '<Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
    + '<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>'
    + '<Override PartName="/xl/pivotTables/pivotTable1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.pivotTable+xml"/>'
    + '<Override PartName="/xl/pivotCache/pivotCacheDefinition1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.pivotCacheDefinition+xml"/>'
    + '<Override PartName="/xl/pivotCache/pivotCacheRecords1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.pivotCacheRecords+xml"/>'
    + '<Override PartName="/xl/slicers/slicer1.xml" ContentType="application/vnd.ms-excel.slicer+xml"/>'
    + '<Override PartName="/xl/slicerCaches/slicerCache1.xml" ContentType="application/vnd.ms-excel.slicerCache+xml"/>'
    + '<Override PartName="/xl/slicerCaches/slicerCache2.xml" ContentType="application/vnd.ms-excel.slicerCache+xml"/>'
    + '<Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>'
    + '<Override PartName="/docProps/custom.xml" ContentType="application/vnd.openxmlformats-officedocument.custom-properties+xml"/>'
    + "</Types>"
)

parts = [
    ("[Content_Types].xml", ct),
    ("_rels/.rels", rels([("rId1", f"{REL}/officeDocument", "xl/workbook.xml"), ("rId2", f"{REL}/custom-properties", "docProps/custom.xml")])),
    ("docProps/custom.xml", custom),
    ("xl/workbook.xml", workbook),
    ("xl/_rels/workbook.xml.rels", rels([
        ("rId1", f"{REL}/worksheet", "worksheets/sheet1.xml"),
        ("rId2", f"{REL}/worksheet", "worksheets/sheet2.xml"),
        ("rId3", f"{REL}/pivotCacheDefinition", "pivotCache/pivotCacheDefinition1.xml"),
        ("rId4", "http://schemas.microsoft.com/office/2007/relationships/slicerCache", "slicerCaches/slicerCache1.xml"),
        ("rId5", "http://schemas.microsoft.com/office/2007/relationships/slicerCache", "slicerCaches/slicerCache2.xml"),
        ("rId6", f"{REL}/styles", "styles.xml"),
    ])),
    ("xl/styles.xml", styles),
    ("xl/worksheets/sheet1.xml", sheet1),
    ("xl/worksheets/sheet2.xml", sheet2),
    ("xl/worksheets/_rels/sheet2.xml.rels", rels([
        ("rId1", f"{REL}/pivotTable", "../pivotTables/pivotTable1.xml"),
        ("rId2", "http://schemas.microsoft.com/office/2007/relationships/slicer", "../slicers/slicer1.xml"),
        ("rId3", f"{REL}/drawing", "../drawings/drawing1.xml"),
    ])),
    ("xl/pivotTables/pivotTable1.xml", pivot_table),
    ("xl/pivotTables/_rels/pivotTable1.xml.rels", rels([("rId1", f"{REL}/pivotCacheDefinition", "../pivotCache/pivotCacheDefinition1.xml")])),
    ("xl/pivotCache/pivotCacheDefinition1.xml", cache_def),
    ("xl/pivotCache/_rels/pivotCacheDefinition1.xml.rels", rels([("rId1", f"{REL}/pivotCacheRecords", "pivotCacheRecords1.xml")])),
    ("xl/pivotCache/pivotCacheRecords1.xml", cache_rec),
    ("xl/slicers/slicer1.xml", slicers),
    ("xl/slicerCaches/slicerCache1.xml", slicer_cache1),
    ("xl/slicerCaches/slicerCache2.xml", slicer_cache2),
    ("xl/drawings/drawing1.xml", drawing),
]
with zipfile.ZipFile(os.path.join(HERE, "pivot-slicer.xlsx"), "w", zipfile.ZIP_DEFLATED) as z:
    for name, text in parts:
        info = zipfile.ZipInfo(name, date_time=(2026, 10, 1, 0, 0, 0))
        info.compress_type = zipfile.ZIP_DEFLATED
        z.writestr(info, text)
print("wrote", os.path.join(HERE, "pivot-slicer.xlsx"))
