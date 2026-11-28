//! WHIQ: the answer-size rule (item 26, owner 2026-11-28: "Answer Size - Agreed, unify it"): the White Hat IQ platform's own
//! long-result rule (White-Hat-IQ-2 shared/chat-engine/server/resultCap.ts), held here too so that the engine keeps it when it
//! runs without the platform (the desktop app, an outside agent on its MCP). An answer over the limit is cut BY STRUCTURE —
//! lists to their first items with a count of the rest, long text clipped in the middle — and the whole answer is saved to a
//! file whose path the answer names (`result_truncated.full_result_path`). It is never cut to an item offset to call again:
//! calling a tool that changes the workbook again repeats its changes. Behind the platform an answer cut here already fits the
//! platform's rule, so it is never cut twice.

use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

// AGREED LIMIT (owner, 2026-11-28): 131,072 characters — an answer over it is cut by structure and saved whole to a file — why: item 26 rule 2, "Answer Size - Agreed, unify it" — the platform's own number (RESULT_CAP_CHARS), arbitrary by agreement.
pub const MAX_ANSWER_CHARS: usize = 128 * 1024;
/// The platform's ladder: lists 50 → 20 → 8 → 3 → 1 items, text 4,000 → 1,500 → 500 → 200 → 80 characters; the first step
/// that fits is taken. Nothing fits (one enormous text, a flat object with thousands of keys): a plain cut of the JSON, never
/// under the platform's floor of 8,192 characters.
const LADDER: [(usize, usize); 5] = [(50, 4000), (20, 1500), (8, 500), (3, 200), (1, 80)];
const FLOOR_CHARS: usize = 8 * 1024;

/// Characters of an answer as the platform counts them (UTF-16 units of the compact JSON).
pub fn answer_chars(v: &Value) -> usize {
    serde_json::to_string(v).map(|s| s.encode_utf16().count()).unwrap_or(0)
}

fn shrink(v: &Value, keep: usize, text: usize) -> Value {
    match v {
        Value::String(s) => {
            let n = s.chars().count();
            if n <= text {
                return v.clone();
            }
            let head = text * 3 / 4;
            let tail = text - head;
            let h: String = s.chars().take(head).collect();
            let t: String = s.chars().skip(n - tail).collect();
            Value::String(format!("{h} …[{} characters cut]… {t}", n - text))
        }
        Value::Array(a) => {
            let mut out: Vec<Value> = a.iter().take(keep).map(|x| shrink(x, keep, text)).collect();
            if a.len() > keep {
                out.push(Value::String(format!("… +{} more (the whole answer is in the file named by result_truncated)", a.len() - keep)));
            }
            Value::Array(out)
        }
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), shrink(x, keep, text))).collect()),
        other => other.clone(),
    }
}

/// `v` (its JSON `text` over `max` characters) cut to at most `max` characters of JSON, by structure; with the list length kept.
fn cut(v: &Value, text: &str, max: usize) -> (Value, usize, usize) {
    for (keep, t) in LADDER {
        let c = shrink(v, keep, t);
        let n = answer_chars(&c);
        if n <= max {
            return (c, n, keep);
        }
    }
    // the plain cut is a JSON string of the answer's JSON: its quotes and backslashes are escaped again, so it is sized by its
    // escaped length (what the platform counts), not by its characters
    let limit = max.max(FLOOR_CHARS).saturating_sub(200);
    let (mut used, mut taken) = (0usize, 0usize);
    for c in text.chars() {
        let w = escaped_units(c);
        if used + w > limit {
            break;
        }
        used += w;
        taken += 1;
    }
    let plain: String = text.chars().take(taken).collect();
    let s = format!("{plain} …[{} characters cut]", text.chars().count() - taken);
    let n = answer_chars(&Value::String(s.clone()));
    (Value::String(s), n, 0)
}

/// UTF-16 units a character takes inside a JSON string, as serde_json writes it.
fn escaped_units(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf16(),
    }
}

/// A UTC stamp for the file name (yyyymmdd-hhmmss), from the system clock.
fn stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // civil from days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The whole answer written to `<tool>-result-<stamp>.json` in `dir` (a name taken gets -2, -3 …); the path, or why not.
fn save(dir: &Path, tool: &str, text: &str) -> Result<PathBuf, String> {
    let safe: String = tool.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    let base = format!("{}-result-{}", if safe.is_empty() { "result".to_string() } else { safe }, stamp());
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut n = 1u32;
    loop {
        let p = dir.join(if n == 1 { format!("{base}.json") } else { format!("{base}-{n}.json") });
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(mut f) => {
                use std::io::Write;
                return f.write_all(text.as_bytes()).map(|()| p).map_err(|e| e.to_string());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e.to_string()),
        }
    }
}

// AGREED LIMIT (owner, 2026-11-28): 25 — a list in the answer of a tool that changes the workbook shows its first 25, with
// `<list>_total` and `<list>_note` naming the file that holds every item — why: item 26 rule 3, "Lists inside Answers, Agreed" — the
// platform's and the P6 clone's list rule, arbitrary by agreement. Reading tools return what is asked (and page).
pub const LISTED: usize = 25;

/// The list rule on an answer: each top-level list longer than [`LISTED`] is cut to its first [`LISTED`] items, with `<list>_total`
/// and `<list>_note`; the whole answer is written once to `<tool>-result-<stamp>.json` in `dir` (else the system temp folder).
pub fn listed(v: Value, tool: &str, dir: Option<&Path>) -> Value {
    let Value::Object(mut o) = v else { return v };
    let long: Vec<String> = o.iter().filter(|(_, x)| x.as_array().is_some_and(|a| a.len() > LISTED)).map(|(k, _)| k.clone()).collect();
    if long.is_empty() {
        return Value::Object(o);
    }
    let folder = dir.map(Path::to_path_buf).unwrap_or_else(|| std::env::temp_dir().join("gridcraft-results"));
    let saved = save(&folder, tool, &serde_json::to_string(&Value::Object(o.clone())).unwrap_or_default());
    for k in long {
        let Some(Value::Array(items)) = o.remove(&k) else { continue };
        let n = items.len();
        o.insert(k.clone(), Value::Array(items.into_iter().take(LISTED).collect()));
        o.insert(format!("{k}_total"), json!(n));
        o.insert(
            format!("{k}_note"),
            json!(match &saved {
                Ok(p) => format!("{LISTED} of {n} shown; all {n} in {}.", p.display()),
                Err(e) => format!("{LISTED} of {n} shown; the whole list could not be written to a file ({e})."),
            }),
        );
    }
    Value::Object(o)
}

/// The rule applied to a tool's answer: `v` as it is when it fits; else the cut answer with `result_truncated`
/// {original_chars, kept_chars, full_result_path | full_result_missing, note} (the platform's words), the whole answer saved
/// in `dir` (the write root; else the system temp folder). `ask`: how to ask the tool for less, when it can be asked.
pub fn cap(v: Value, tool: &str, dir: Option<&Path>, ask: &str) -> Value {
    cap_at(v, tool, dir, ask, MAX_ANSWER_CHARS)
}

pub fn cap_at(v: Value, tool: &str, dir: Option<&Path>, ask: &str, max: usize) -> Value {
    let text = serde_json::to_string(&v).unwrap_or_default();
    let original = text.encode_utf16().count();
    if original <= max {
        return v;
    }
    let folder = dir.map(Path::to_path_buf).unwrap_or_else(|| std::env::temp_dir().join("gridcraft-results"));
    let saved = save(&folder, tool, &text);
    // room left for the note that says so
    let (value, kept, items) = cut(&v, &text, max.saturating_sub(2048.min(max / 4)));
    let mut body: Map<String, Value> = match value {
        Value::Object(o) => o,
        other => {
            let mut m = Map::new();
            m.insert("result".into(), other);
            m
        }
    };
    let what = if items > 0 { format!("lists were cut to their first {items} items and long text clipped") } else { "it was cut to its first part".to_string() };
    let kb = (original as f64 / 1024.0).round();
    let note = match &saved {
        Ok(p) => json!({
            "original_chars": original, "kept_chars": kept, "full_result_path": p.display().to_string(),
            "note": format!("This answer was {kb} KB — too large to read in one turn, so {what}. The whole answer is saved at {}: read the file for the rest{ask}.", p.display()),
        }),
        Err(e) => json!({
            "original_chars": original, "kept_chars": kept, "full_result_missing": format!("the whole answer could not be written to a file ({e})"),
            "note": format!("This answer was {kb} KB — too large to read in one turn, so {what}. The whole answer could NOT be saved, so the cut part is not available anywhere: do not treat this as the whole answer{}.", if ask.is_empty() { " — ask the tool for less" } else { ask }),
        }),
    };
    body.insert("result_truncated".into(), note);
    Value::Object(body)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_long_list_shows_25_with_its_total_and_the_file() {
        let dir = std::env::temp_dir().join(format!("gc-list-{}", std::process::id()));
        let v = json!({"errors": (0..40).collect::<Vec<_>>(), "count": 40, "short": [1, 2]});
        let out = listed(v, "recalculate", Some(&dir));
        assert_eq!(out["errors"].as_array().unwrap().len(), 25);
        assert_eq!(out["errors_total"], 40);
        assert_eq!(out["short"], json!([1, 2]));
        let note = out["errors_note"].as_str().unwrap();
        let path = note.rsplit_once(" in ").unwrap().1.trim_end_matches('.');
        let whole: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(whole["errors"].as_array().unwrap().len(), 40);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn small_answers_pass_whole() {
        let v = json!({"values": [1, 2, 3]});
        assert_eq!(cap(v.clone(), "read_range", None, ""), v);
    }

    #[test]
    fn a_big_answer_is_cut_by_structure_and_saved_whole() {
        let dir = std::env::temp_dir().join(format!("gc-cap-{}", std::process::id()));
        let rows: Vec<Value> = (0..20_000).map(|i| json!({"row": i, "text": "x".repeat(30)})).collect();
        let v = json!({"sheet": "Data", "rows": rows, "note": "kept"});
        let out = cap_at(v.clone(), "batch", Some(&dir), "", 50_000);
        assert!(answer_chars(&out) <= 50_000);
        assert_eq!(out["sheet"], "Data");
        assert_eq!(out["note"], "kept");
        let t = &out["result_truncated"];
        assert_eq!(t["original_chars"].as_u64().unwrap() as usize, answer_chars(&v));
        let path = t["full_result_path"].as_str().unwrap();
        assert!(path.contains("batch-result-") && path.ends_with(".json"));
        let whole: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(whole, v);
        let list = out["rows"].as_array().unwrap();
        assert!(list.last().unwrap().as_str().unwrap().starts_with("… +"));
        assert!(t["note"].as_str().unwrap().contains("read the file for the rest"));
        assert!(out.get("continue_from").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_plain_cut_of_quote_heavy_json_fits_once_escaped() {
        // a flat object of many keys: no list or text to shrink, so the plain cut; every quote of its JSON is escaped again
        let dir = std::env::temp_dir().join(format!("gc-cap3-{}", std::process::id()));
        let o: Map<String, Value> = (0..20_000).map(|i| (format!("k{i}"), json!("\"q\""))).collect();
        let out = cap_at(Value::Object(o), "x", Some(&dir), "", 50_000);
        assert!(answer_chars(&out) <= 50_000, "{}", answer_chars(&out));
        assert!(out["result"].as_str().unwrap().ends_with("characters cut]"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_enormous_text_is_clipped_and_the_floor_holds() {
        let dir = std::env::temp_dir().join(format!("gc-cap2-{}", std::process::id()));
        let v = json!({"html": "y".repeat(400_000)});
        let out = cap_at(v, "x", Some(&dir), "", 20_000);
        let s = out["html"].as_str().unwrap();
        assert!(s.contains("characters cut"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
