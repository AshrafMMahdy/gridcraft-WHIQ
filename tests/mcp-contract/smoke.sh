#!/usr/bin/env bash
# WHIQ MCP smoke test: drives a built gridcraft-cli over stdio end to end, the way the platform's
# supervisor does (roots on a chat folder, author set, one JSON-RPC message per line).
#
#   cargo build --release -p gridcraft-cli
#   tests/mcp-contract/smoke.sh [path/to/gridcraft-cli]
#
# Exit 0 when every step answered as expected; the folder with the outputs is printed.
set -u
BIN="${1:-$(dirname "$0")/../../target/release/gridcraft-cli}"
[ -x "$BIN" ] || { echo "no gridcraft-cli at $BIN (cargo build --release -p gridcraft-cli)"; exit 2; }
DIR="$(mktemp -d "${TMPDIR:-/tmp}/gc-smoke-XXXXXX")"
REQ="$DIR/requests.jsonl"
OUT="$DIR/responses.jsonl"
cat > "$REQ" <<'JSON'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"server_status","arguments":{}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"build_workbook","arguments":{"path":"report.xlsx","title":"تقرير","sheets":[{"name":"البنود","rtl":true,"header":["البند","الكمية","السعر","الإجمالي"],"rows":[["أعمال الخرسانة",120,350.5,"=B2*C2"],["Steel works حديد",45,1200,"=B3*C3"],["المجموع","=SUM(B2:B3)",null,"=SUM(D2:D3)"]],"columns":[{"width":22},{"format":"#,##0"},{"format":"#,##0.00"},{"format":"#,##0.00"}],"freeze_header":true,"table":"TableStyleMedium2"}],"charts":[{"sheet":"البنود","kind":"col","title":"الكمية","series":[{"name":"الكمية","catRange":"$A$2:$A$3","valRange":"$B$2:$B$3"}],"at":"F2"}]}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read_range","arguments":{"range":"D4"}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"render_sheet","arguments":{"path":"report.png","max_pixels":1600}}}
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"export_pdf","arguments":{"path":"report.pdf","paper":"A4","orientation":"landscape","fit_width":true,"repeat_header_rows":"1:1"}}}
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"save_workbook","arguments":{"path":"customer.xlsx"}}}
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"save_workbook","arguments":{"path":"/etc/nope.xlsx"}}}
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"readRange","arguments":{"Range":"A1:B2","bogus":1}}}
{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"recalculate","arguments":{}}}
{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"cancel","arguments":{}}}
{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"open_workbook","arguments":{"path":"pivot.xlsx"}}}
{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"execute_command","arguments":{"command":"slicer.list","params":{"sheet":"Pivot"}}}}
{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"save_workbook","arguments":{"path":"pivot-out.xlsx"}}}
JSON
# An Excel-shaped workbook with a PivotTable and two slicers.
cp "$(dirname "$0")/fixtures/pivot-slicer.xlsx" "$DIR/pivot.xlsx"
# A file the customer already had: saving over it must keep it as OLD_customer.xlsx.
printf 'the customer file' > "$DIR/customer.xlsx"
# One request at a time, waiting for its answer (as an MCP client does).
coproc GC { "$BIN" mcp --automation-read-root "$DIR" --automation-write-root "$DIR" --author "Smoke Test" 2> "$DIR/stderr.log"; }
: > "$OUT"
while IFS= read -r req; do
  printf '%s\n' "$req" >&"${GC[1]}"
  case "$req" in *'"id":'*) ;; *) continue ;; esac
  if IFS= read -r -t 120 resp <&"${GC[0]}"; then printf '%s\n' "$resp" >> "$OUT"; else echo "no answer to: $req" | cut -c1-200; fi
done < "$REQ"
exec {GC[1]}>&-
wait "$GC_PID" 2>/dev/null
fail=0
check() { # id, jq-free grep pattern, what
  local line; line="$(grep -E "\"id\":$1[,}]" "$OUT" | head -1)"
  if printf '%s' "$line" | grep -qE "$2"; then echo "ok   $3"; else echo "FAIL $3"; echo "     $line" | cut -c1-400; fail=1; fi
}
check 1 '"serverInfo":\{[^}]*"name":"gridcraft"' "initialize"
check 2 '"name":"build_workbook".*"name":"render_range"|"name":"render_range".*"name":"build_workbook"' "tools/list has the WHIQ tools"
check 3 '"server":"gridcraft"' "server_status"
check 3 '"faces":[1-9]' "the binary embeds craft-fonts"
check 3 '"rss_bytes":[1-9]' "memory is measured"
check 4 '"isError":false.*"charts":1' "build_workbook (Arabic, RTL sheet, table, chart, formulas)"
check 5 '"values":\[\[96060' "read_range of a recalculated formula"
check 6 '"isError":false.*"width"' "render_sheet PNG"
check 7 '"isError":false.*"pages"' "export_pdf with page setup"
check 8 'OLD_customer.xlsx' "save over an existing file renames it OLD_"
check 9 '"code":"outside_root"' "a path outside the write root is refused with a next step"
check 10 '"isError":false.*"warnings"' "forgiving tool name and arguments"
check 11 '"errorCount":0' "recalculate"
check 12 '"code":"not_running"' "cancel with nothing running says so"
check 13 '"isError":false' "open a workbook with a PivotTable and slicers"
check 14 'Pivot!PivotTable1.*Bolts|Bolts.*Pivot!PivotTable1' "the PivotTable slicers are connected"
check 15 '"isError":false' "save it again"
for f in report.xlsx customer.xlsx OLD_customer.xlsx report.png report.pdf pivot-out.xlsx; do
  if [ -s "$DIR/$f" ]; then echo "ok   $f written"; else echo "FAIL $f missing"; fail=1; fi
done
if command -v pdffonts >/dev/null; then
  if pdffonts "$DIR/report.pdf" | grep -q "NotoSansArabic"; then echo "ok   PDF embeds the Arabic subset font"; else echo "FAIL PDF fonts"; fail=1; fi
  if pdffonts "$DIR/report.pdf" | grep -q "+Carlito-"; then echo "ok   PDF embeds Latin text too (Carlito for Calibri)"; else echo "FAIL PDF Latin font"; fail=1; fi
  if pdffonts "$DIR/report.pdf" | tail -n +3 | grep -qv " yes yes "; then echo "FAIL a PDF font is not embedded as a subset"; fail=1; else echo "ok   every PDF font embedded as a subset"; fi
fi
if command -v pdftotext >/dev/null; then
  if pdftotext "$DIR/report.pdf" - | grep -q "الإجمالي"; then echo "ok   PDF text is extractable Arabic"; else echo "FAIL PDF text"; fail=1; fi
fi
if command -v unzip >/dev/null; then
  if unzip -l "$DIR/pivot-out.xlsx" | grep -q "xl/slicerCaches/slicerCache2.xml"; then echo "ok   the saved file keeps both slicer caches"; else echo "FAIL slicer caches"; fail=1; fi
fi
"$BIN" check "$DIR/pivot-out.xlsx" > "$DIR/check.txt" 2>&1 && echo "ok   gridcraft-cli check passes on it" || { echo "FAIL check: $(head -5 "$DIR/check.txt")"; fail=1; }
echo "outputs: $DIR"
exit $fail
