//! WHIQ: the craft-fonts build input (https://github.com/storytold/craft-fonts, recipe from its
//! `docs/integration.md`, as WordCraft and DeckCraft use it). With `CRAFT_FONTS_DIR=<checkout>`
//! the fonts in its `fonts/latin-manifest.txt` (Latin presentation fonts: Carlito, Caladea,
//! Liberation…, with real italic and bold-italic faces) and `fonts/manifest.txt` (Arabic, CJK…)
//! are embedded as `CRAFT_FONTS`; unset, `CRAFT_FONTS` is empty and exports use the PDF standard
//! fonts. Web (wasm32) builds embed nothing (size budget).
//!
//! With `CRAFT_FONTS_REQUIRED` (releases), a native build fails when the checkout is missing, a
//! manifest is malformed, or it has no Arabic font or no Latin family with Regular, Bold, Italic
//! and Bold Italic faces. It only reads the local checkout: no network.
use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-env-changed=CRAFT_FONTS_DIR");
    println!("cargo::rerun-if-env-changed=CRAFT_FONTS_REQUIRED");
    let required = std::env::var_os("CRAFT_FONTS_REQUIRED").is_some_and(|v| !v.is_empty() && v != "0");
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "wasm32");
    let mut src = String::from("pub static CRAFT_FONTS: &[CraftFont] = &[\n");
    match std::env::var_os("CRAFT_FONTS_DIR").map(PathBuf::from) {
        Some(dir) if !wasm => match craft_fonts(&dir) {
            Ok((entries, fonts)) => {
                src.push_str(&entries);
                if required {
                    for problem in missing(&fonts) {
                        println!(
                            "cargo::error=CRAFT_FONTS_DIR={}: {problem} (is the checkout at the pinned commit? see WHIQ-CHANGES.md, Fonts)",
                            dir.display()
                        );
                    }
                }
            }
            Err(e) if required => println!("cargo::error=CRAFT_FONTS_DIR={}: {e}", dir.display()),
            Err(e) => println!("cargo::warning=building without craft-fonts: CRAFT_FONTS_DIR={}: {e}", dir.display()),
        },
        None if required && !wasm => println!("cargo::error=CRAFT_FONTS_REQUIRED is set but CRAFT_FONTS_DIR is not"),
        _ => {}
    }
    src.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("craft_fonts.rs");
    if let Err(e) = std::fs::write(&out, src) {
        println!("cargo::error=writing {}: {e}", out.display());
    }
}

/// What a release needs and the embedded fonts lack.
fn missing(fonts: &[(String, String, Vec<String>)]) -> Vec<String> {
    let mut out = Vec::new();
    if !fonts.iter().any(|(_, _, s)| s.iter().any(|x| x == "Arab")) {
        out.push("no Arabic (Arab) font in fonts/manifest.txt".to_string());
    }
    let full_latin = fonts.iter().filter(|(_, _, s)| s.iter().any(|x| x == "Latn")).any(|(family, _, _)| {
        ["Regular", "Bold", "Italic", "Bold Italic"].iter().all(|st| fonts.iter().any(|(f, s, _)| f == family && s.eq_ignore_ascii_case(st)))
    });
    if !full_latin {
        out.push("no Latin family with Regular, Bold, Italic and Bold Italic faces (fonts/latin-manifest.txt)".to_string());
    }
    out
}

/// `CraftFont { .. }` initialisers for both manifests, and (family, style, scripts) of each.
fn craft_fonts(dir: &std::path::Path) -> Result<(String, Vec<(String, String, Vec<String>)>), String> {
    let mut out = String::new();
    let mut fonts = Vec::new();
    let latin = dir.join("fonts/latin-manifest.txt");
    println!("cargo::rerun-if-changed={}", latin.display());
    if latin.exists() {
        manifest(dir, &latin, &mut out, &mut fonts)?;
    }
    manifest(dir, &dir.join("fonts/manifest.txt"), &mut out, &mut fonts)?;
    Ok((out, fonts))
}

fn manifest(
    dir: &std::path::Path,
    manifest: &std::path::Path,
    out: &mut String,
    fonts: &mut Vec<(String, String, Vec<String>)>,
) -> Result<(), String> {
    println!("cargo::rerun-if-changed={}", manifest.display());
    let text = std::fs::read_to_string(manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let f: Vec<&str> = line.split(" | ").map(str::trim).collect();
        let [family, style, file, scripts, ..] = f.as_slice() else {
            return Err(format!("malformed manifest line: {line}"));
        };
        let path = dir.join(file).canonicalize().map_err(|e| format!("{file}: {e}"))?;
        println!("cargo::rerun-if-changed={}", path.display());
        let list: Vec<String> = scripts.split(',').map(|s| s.trim().to_string()).collect();
        let quoted: Vec<String> = list.iter().map(|s| format!("{s:?}")).collect();
        let _ = writeln!(
            out,
            "    CraftFont {{ family: {family:?}, style: {style:?}, scripts: &[{}], bytes: include_bytes!({:?}) }},",
            quoted.join(", "),
            path.display().to_string(),
        );
        fonts.push((family.to_string(), style.to_string(), list));
    }
    Ok(())
}
