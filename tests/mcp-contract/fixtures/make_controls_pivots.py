#!/usr/bin/env python3
"""Writes two WHIQ 0.7.6 fixtures (original content, no Excel or customer file):

- form-controls-2010.xlsx: a sheet "Form" with two check boxes laid out as Excel 2010 saves form
  controls: the legacy VML shapes (_x0000_s1025, _x0000_s1026), the `<controls>` element with each
  control's `ctrlProp` part and anchor (inside `mc:AlternateContent Requires="x14"`), and the
  DrawingML twin of each control in xl/drawings/drawing1.xml (`mc:AlternateContent Requires="a14"`
  holding a hidden `xdr:sp` whose `a14:compatExt spid` names the VML shape).
- pivot-shared-cache.xlsx: pivot-slicer.xlsx (make_pivot_slicer.py) plus a third sheet "Pivot2"
  with a second PivotTable that reads the same pivot cache as PivotTable1 (one cache, two tables,
  as Excel writes them).

Run: python3 make_controls_pivots.py (writes both files next to this script; needs
pivot-slicer.xlsx there).
"""
import os
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
DECL = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
MAIN = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
MC = "http://schemas.openxmlformats.org/markup-compatibility/2006"
X14 = "http://schemas.microsoft.com/office/spreadsheetml/2009/9/main"
XDR = "http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"
A = "http://schemas.openxmlformats.org/drawingml/2006/main"
A14 = "http://schemas.microsoft.com/office/drawing/2010/main"
REL = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"


def rels(items):
    body = "".join(f'<Relationship Id="{i}" Type="{t}" Target="{g}"/>' for i, t, g in items)
    return DECL + f'<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{body}</Relationships>'


def marker(tag, col, row):
    return f"<xdr:{tag}><xdr:col>{col}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{row}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:{tag}>"


def form_controls():
    boxes = [(1025, "Check Box 1", "Approved", 3, 1), (1026, "Check Box 2", "Paid", 3, 3)]
    rows = "".join(
        f'<row r="{r}"><c r="A{r}" t="inlineStr"><is><t>{label}</t></is></c><c r="B{r}"><v>{r * 10}</v></c></row>'
        for r, label in [(2, "Approved"), (4, "Paid")]
    )
    controls = "".join(
        f'<mc:AlternateContent xmlns:mc="{MC}"><mc:Choice Requires="x14">'
        f'<control shapeId="{sid}" r:id="rId{3 + k}" name="{name}"><controlPr defaultSize="0" autoFill="0" autoLine="0" autoPict="0">'
        f'<anchor moveWithCells="1">'
        f'<from><xdr:col>{col}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{row}</xdr:row><xdr:rowOff>0</xdr:rowOff></from>'
        f'<to><xdr:col>{col + 1}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{row + 1}</xdr:row><xdr:rowOff>0</xdr:rowOff></to>'
        f"</anchor></controlPr></control></mc:Choice></mc:AlternateContent>"
        for k, (sid, name, _, col, row) in enumerate(boxes)
    )
    sheet = (
        DECL
        + f'<worksheet xmlns="{MAIN}" xmlns:r="{R}" xmlns:xdr="{XDR}" xmlns:x14="{X14}" xmlns:mc="{MC}">'
        + f'<dimension ref="A2:B4"/><sheetData>{rows}</sheetData>'
        + '<pageMargins left="0.7" right="0.7" top="0.75" bottom="0.75" header="0.3" footer="0.3"/>'
        + '<drawing r:id="rId1"/><legacyDrawing r:id="rId2"/>'
        + f'<mc:AlternateContent xmlns:mc="{MC}"><mc:Choice Requires="x14"><controls>{controls}</controls></mc:Choice></mc:AlternateContent>'
        + "</worksheet>"
    )
    twins = "".join(
        f'<mc:AlternateContent xmlns:mc="{MC}"><mc:Choice xmlns:a14="{A14}" Requires="a14">'
        f'<xdr:twoCellAnchor editAs="oneCell">{marker("from", col, row)}{marker("to", col + 1, row + 1)}'
        f'<xdr:sp macro="" textlink=""><xdr:nvSpPr><xdr:cNvPr id="{sid}" name="{name}" hidden="1"><a:extLst>'
        f'<a:ext uri="{{63B3BB69-23CF-44E3-9099-C40C66FF867C}}"><a14:compatExt spid="_x0000_s{sid}"/></a:ext></a:extLst></xdr:cNvPr><xdr:cNvSpPr/></xdr:nvSpPr>'
        f'<xdr:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:sp>'
        f"<xdr:clientData/></xdr:twoCellAnchor></mc:Choice><mc:Fallback/></mc:AlternateContent>"
        for sid, name, _, col, row in boxes
    )
    drawing = DECL + f'<xdr:wsDr xmlns:xdr="{XDR}" xmlns:a="{A}">{twins}</xdr:wsDr>'
    shapes = "".join(
        f'<v:shape id="_x0000_s{sid}" type="#_x0000_t201" style="position:absolute;margin-left:200pt;margin-top:{15 + 30 * k}pt;width:60pt;height:15pt;z-index:{k + 1}" filled="f" stroked="f" o:insetmode="auto">'
        f'<v:textbox style="mso-direction-alt:auto" o:singleclick="f"><div style="text-align:left"><font face="Tahoma" size="160" color="auto">{label}</font></div></v:textbox>'
        f"<x:ClientData ObjectType=\"Checkbox\"><x:Anchor>{col}, 0, {row}, 0, {col + 1}, 0, {row + 1}, 0</x:Anchor><x:AutoFill>False</x:AutoFill><x:AutoLine>False</x:AutoLine>"
        f"<x:TextVAlign>Center</x:TextVAlign><x:NoThreeD/></x:ClientData></v:shape>"
        for k, (sid, _, label, col, row) in enumerate(boxes)
    )
    vml = (
        '<xml xmlns:v="urn:schemas-microsoft-com:vml" xmlns:o="urn:schemas-microsoft-com:office:office" xmlns:x="urn:schemas-microsoft-com:office:excel">'
        '<o:shapelayout v:ext="edit"><o:idmap v:ext="edit" data="1"/></o:shapelayout>'
        '<v:shapetype id="_x0000_t201" coordsize="21600,21600" o:spt="201" path="m,l,21600r21600,l21600,xe"><v:stroke joinstyle="miter"/>'
        '<v:path shadowok="f" o:extrusionok="f" strokeok="f" fillok="f" o:connecttype="rect"/><o:lock v:ext="edit" shapetype="t"/></v:shapetype>'
        f"{shapes}</xml>"
    )
    ctrl = DECL + f'<formControlPr xmlns="{X14}" objectType="CheckBox" lockText="1" noThreeD="1"/>'
    wbx = DECL + f'<workbook xmlns="{MAIN}" xmlns:r="{R}"><bookViews><workbookView/></bookViews><sheets><sheet name="Form" sheetId="1" r:id="rId1"/></sheets></workbook>'
    styles = DECL + f'<styleSheet xmlns="{MAIN}"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf/></cellStyleXfs><cellXfs count="1"><xf/></cellXfs></styleSheet>'
    ct = (
        DECL
        + '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>'
        + '<Default Extension="vml" ContentType="application/vnd.openxmlformats-officedocument.vmlDrawing"/>'
        + '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
        + '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
        + '<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>'
        + '<Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>'
        + '<Override PartName="/xl/ctrlProps/ctrlProp1.xml" ContentType="application/vnd.ms-excel.controlproperties+xml"/>'
        + '<Override PartName="/xl/ctrlProps/ctrlProp2.xml" ContentType="application/vnd.ms-excel.controlproperties+xml"/>'
        + "</Types>"
    )
    parts = [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", rels([("rId1", f"{REL}/officeDocument", "xl/workbook.xml")])),
        ("xl/workbook.xml", wbx),
        ("xl/_rels/workbook.xml.rels", rels([("rId1", f"{REL}/worksheet", "worksheets/sheet1.xml"), ("rId2", f"{REL}/styles", "styles.xml")])),
        ("xl/styles.xml", styles),
        ("xl/worksheets/sheet1.xml", sheet),
        ("xl/worksheets/_rels/sheet1.xml.rels", rels([
            ("rId1", f"{REL}/drawing", "../drawings/drawing1.xml"),
            ("rId2", f"{REL}/vmlDrawing", "../drawings/vmlDrawing1.vml"),
            ("rId3", f"{REL}/ctrlProp", "../ctrlProps/ctrlProp1.xml"),
            ("rId4", f"{REL}/ctrlProp", "../ctrlProps/ctrlProp2.xml"),
        ])),
        ("xl/drawings/drawing1.xml", drawing),
        ("xl/drawings/vmlDrawing1.vml", vml),
        ("xl/ctrlProps/ctrlProp1.xml", ctrl),
        ("xl/ctrlProps/ctrlProp2.xml", ctrl),
    ]
    with zipfile.ZipFile(os.path.join(HERE, "form-controls-2010.xlsx"), "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in parts:
            z.writestr(name, data)


def shared_cache():
    src = os.path.join(HERE, "pivot-slicer.xlsx")
    with zipfile.ZipFile(src) as z:
        parts = {n: z.read(n) for n in z.namelist()}
    table = parts["xl/pivotTables/pivotTable1.xml"].decode()
    table2 = table.replace('name="PivotTable1"', 'name="PivotTable2"', 1)
    parts["xl/pivotTables/pivotTable2.xml"] = table2.encode()
    parts["xl/pivotTables/_rels/pivotTable2.xml.rels"] = rels([("rId1", f"{REL}/pivotCacheDefinition", "../pivotCache/pivotCacheDefinition1.xml")]).encode()
    sheet2 = parts["xl/worksheets/sheet2.xml"].decode()
    rows = sheet2[sheet2.index("<sheetData>"):sheet2.index("</sheetData>") + len("</sheetData>")]
    dim = sheet2[sheet2.index("<dimension"):sheet2.index("/>", sheet2.index("<dimension")) + 2]
    parts["xl/worksheets/sheet3.xml"] = (DECL + f'<worksheet xmlns="{MAIN}" xmlns:r="{R}">{dim}{rows}</worksheet>').encode()
    parts["xl/worksheets/_rels/sheet3.xml.rels"] = rels([("rId1", f"{REL}/pivotTable", "../pivotTables/pivotTable2.xml")]).encode()
    wbx = parts["xl/workbook.xml"].decode()
    wbx = wbx.replace('<sheet name="Pivot" sheetId="2" r:id="rId2"/>', '<sheet name="Pivot" sheetId="2" r:id="rId2"/><sheet name="Pivot2" sheetId="3" r:id="rId9"/>', 1)
    parts["xl/workbook.xml"] = wbx.encode()
    wrels = parts["xl/_rels/workbook.xml.rels"].decode()
    wrels = wrels.replace("</Relationships>", f'<Relationship Id="rId9" Type="{REL}/worksheet" Target="worksheets/sheet3.xml"/></Relationships>')
    parts["xl/_rels/workbook.xml.rels"] = wrels.encode()
    ct = parts["[Content_Types].xml"].decode()
    ct = ct.replace(
        "</Types>",
        '<Override PartName="/xl/worksheets/sheet3.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
        '<Override PartName="/xl/pivotTables/pivotTable2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.pivotTable+xml"/></Types>',
    )
    parts["[Content_Types].xml"] = ct.encode()
    with zipfile.ZipFile(os.path.join(HERE, "pivot-shared-cache.xlsx"), "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in parts.items():
            z.writestr(name, data)


if __name__ == "__main__":
    form_controls()
    shared_cache()
