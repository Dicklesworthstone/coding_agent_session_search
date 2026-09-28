//! The README and AGENTS dependency tables must name the exact registry pins
//! that Cargo.toml actually uses (coding_agent_session_search-2l1b0.69).
//!
//! Observed drift: on 2026-09-26 the manifest moved franken-agent-detection
//! to `=0.3.2` while both tables still said `=0.3.1`, and the 2026-09-23 audit
//! found the tables two releases stale.

use std::collections::BTreeMap;
use std::path::Path;

/// `[dependencies]` entries pinned to an exact version: key -> version.
fn exact_pins(manifest: &str) -> BTreeMap<String, String> {
    let manifest: toml::Table = manifest.parse().expect("Cargo.toml parses");
    let deps = manifest["dependencies"]
        .as_table()
        .expect("[dependencies] table");
    deps.iter()
        .filter_map(|(key, spec)| {
            let version = match spec {
                toml::Value::String(version) => version.as_str(),
                toml::Value::Table(table) => table.get("version")?.as_str()?,
                _ => return None,
            };
            let exact = version.strip_prefix('=')?;
            Some((key.clone(), exact.trim().to_owned()))
        })
        .collect()
}

/// Every mismatch between a doc's pin rows and the manifest, plus how many
/// rows were checked. A row is `| names | crates.io `=X` ... |`; its pin is
/// matched against the first backticked name that is an exactly pinned
/// dependency (`frankentui` rows are keyed by `ftui`).
fn pin_mismatches(doc: &str, pins: &BTreeMap<String, String>) -> (Vec<String>, usize) {
    let mut mismatches = Vec::new();
    let mut checked = 0;
    for line in doc.lines() {
        let Some(rest) = line.strip_prefix('|') else {
            continue;
        };
        let mut cells = rest.split('|');
        let (Some(names), Some(source)) = (cells.next(), cells.next()) else {
            continue;
        };
        let Some(documented) = source
            .trim()
            .strip_prefix("crates.io `=")
            .and_then(|pin| pin.split('`').next())
        else {
            continue;
        };
        let dependency = names
            .split('`')
            .skip(1)
            .step_by(2)
            .find(|name| pins.contains_key(*name));
        let Some(dependency) = dependency else {
            mismatches.push(format!(
                "row names no exactly pinned dependency: {}",
                names.trim()
            ));
            continue;
        };
        checked += 1;
        if pins[dependency] != documented {
            mismatches.push(format!(
                "{dependency}: documented `={documented}`, Cargo.toml pins `={}`",
                pins[dependency]
            ));
        }
    }
    (mismatches, checked)
}

fn read(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(name))
        .unwrap_or_else(|error| panic!("read {name}: {error}"))
}

#[test]
fn readme_and_agents_dependency_pins_match_cargo_toml() {
    let pins = exact_pins(&read("Cargo.toml"));
    for doc in ["README.md", "AGENTS.md"] {
        let (mismatches, checked) = pin_mismatches(&read(doc), &pins);
        assert!(
            mismatches.is_empty(),
            "{doc} dependency table disagrees with Cargo.toml:\n{}",
            mismatches.join("\n")
        );
        // frankensqlite, franken-agent-detection, asupersync, frankensearch,
        // frankentui and toon each have a row today.
        assert!(checked >= 6, "{doc}: only {checked} pin rows found");
    }
}

#[test]
fn a_stale_pin_row_is_reported() {
    let pins = exact_pins(&read("Cargo.toml"));
    let current = &pins["franken-agent-detection"];
    let stale = read("README.md").replace(
        &format!("| `franken-agent-detection` | crates.io `={current}`"),
        "| `franken-agent-detection` | crates.io `=0.0.1`",
    );
    let (mismatches, _) = pin_mismatches(&stale, &pins);
    assert_eq!(
        mismatches,
        vec![format!(
            "franken-agent-detection: documented `=0.0.1`, Cargo.toml pins `={current}`"
        )]
    );
}
