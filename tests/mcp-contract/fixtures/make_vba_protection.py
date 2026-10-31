#!/usr/bin/env python3
"""Writes the WHIQ 0.7.6 fixtures with XlsxWriter (no Excel file, no customer file):

- vba-button.xlsm: a macro-enabled workbook as XlsxWriter writes one.
  - workbookPr codeName="ThisWorkbook" (plus XlsxWriter's filterPrivacy and defaultThemeVersion);
  - sheet "Data": sheetPr codeName="Sheet1", outlinePr summaryBelow="0", fit to one page wide,
    a tab colour, and an old-style form button (VML only, no <controls>) whose FmlaMacro is
    "[0]!RunReport";
  - sheet "Inputs": sheetPr codeName="InputSheet" (renamed module);
  - xl/vbaProject.bin: a placeholder blob (not a real VBA project; GridCraft carries it as bytes).
- protected-perms.xlsx: sheet "Locked" protected with objects and scenarios left editable
  (no objects / scenarios attribute), insertHyperlinks="0" and pivotTables="0" (both allowed),
  formatCells="0"; B2:B4 unlocked.

The theme part XlsxWriter bundles (derived from Office's default theme) is taken out of both
packages, with its relationship and content type: no Office theme XML is committed.

Run: /usr/bin/python3 make_vba_protection.py (writes both files next to this script).
"""
import os
import re
import tempfile
import zipfile

import xlsxwriter

HERE = os.path.dirname(os.path.abspath(__file__))


def vba_button():
    with tempfile.TemporaryDirectory() as tmp:
        blob = os.path.join(tmp, "vbaProject.bin")
        with open(blob, "wb") as f:
            f.write(b"GridCraft test placeholder: not a VBA project\n" * 4)
        wb = xlsxwriter.Workbook(os.path.join(HERE, "vba-button.xlsm"))
        wb.add_vba_project(blob)
        wb.set_vba_name("ThisWorkbook")
        data = wb.add_worksheet("Data")
        data.set_vba_name("Sheet1")
        data.outline_settings(True, False, True, False)
        data.fit_to_pages(1, 0)
        data.set_tab_color("#1F77B4")
        data.write_row(0, 0, ["Region", "Units"])
        data.write_row(1, 0, ["North", 10])
        data.write_row(2, 0, ["South", 20])
        data.write_formula(3, 1, "=SUM(B2:B3)", None, 30)
        data.insert_button("D2", {"macro": "RunReport", "caption": "Run report", "width": 96, "height": 24})
        inputs = wb.add_worksheet("Inputs")
        inputs.set_vba_name("InputSheet")
        inputs.write(0, 0, "Rate")
        inputs.write(0, 1, 0.07)
        wb.close()


def protected_perms():
    wb = xlsxwriter.Workbook(os.path.join(HERE, "protected-perms.xlsx"))
    ws = wb.add_worksheet("Locked")
    unlocked = wb.add_format({"locked": False})
    ws.write_row(0, 0, ["Item", "Qty"])
    for r, (item, qty) in enumerate([("Bolts", 4), ("Nuts", 9), ("Washers", 2)], start=1):
        ws.write(r, 0, item)
        ws.write(r, 1, qty, unlocked)
    ws.protect("", {"objects": True, "scenarios": True, "insert_hyperlinks": True, "pivot_tables": True, "format_cells": True})
    wb.add_worksheet("Open").write(0, 0, "free")
    wb.close()


def without_theme(path):
    with zipfile.ZipFile(path) as z:
        parts = [(i, z.read(i.filename)) for i in z.infolist() if not i.filename.startswith("xl/theme/")]
    out = []
    for info, data in parts:
        if info.filename == "[Content_Types].xml":
            data = re.sub(rb'<Override PartName="/xl/theme/[^"]*"[^>]*/>', b"", data)
        elif info.filename == "xl/_rels/workbook.xml.rels":
            data = re.sub(rb'<Relationship [^>]*Target="theme/[^"]*"/>', b"", data)
        out.append((info, data))
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for info, data in out:
            z.writestr(info, data)


if __name__ == "__main__":
    vba_button()
    protected_perms()
    for name in ("vba-button.xlsm", "protected-perms.xlsx"):
        without_theme(os.path.join(HERE, name))
