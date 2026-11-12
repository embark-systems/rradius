//! The `schema` command and the schema it prints: the Schema Command, Schema Follows the Parser and
//! Enterprise Sections in the Schema requirements, for this program.

use rradius_core::test_support::schema::{toml_file, toml_text, validate};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

/// The schema the program prints, run in an empty directory.
fn printed_schema() -> Result<Value, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let output = Command::new(PROGRAM).arg("schema").current_dir(dir.path()).output().map_err(|e| e.to_string())?;
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())
}

#[test]
fn test_printing_the_schema() -> Result<(), String> {
    let schema = printed_schema()?;
    assert_eq!(schema.get("$schema").and_then(Value::as_str), Some("https://json-schema.org/draft/2020-12/schema"));
    assert_eq!(schema.get("title").and_then(Value::as_str), Some("rradius-server configuration"));
    assert_eq!(schema.get("required"), Some(&serde_json::json!(["dictionary"])));
    Ok(())
}

#[test]
fn test_an_extra_argument() -> Result<(), String> {
    let output = Command::new(PROGRAM).args(["schema", "extra"]).output().map_err(|e| e.to_string())?;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: rradius-server schema"));
    assert!(output.stdout.is_empty());
    Ok(())
}

/// The `.toml` files of the examples and of every scenario.
fn example_files() -> Result<Vec<PathBuf>, String> {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut files = Vec::new();
    let mut add = |dir: &Path| -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().is_some_and(|extension| extension == "toml") {
                files.push(path);
            }
        }
        Ok(())
    };
    add(&examples)?;
    for entry in std::fs::read_dir(examples.join("scenarios")).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            add(&path)?;
        }
    }
    files.sort();
    Ok(files)
}

#[test]
fn test_examples_validate() -> Result<(), String> {
    let schema = printed_schema()?;
    let files = example_files()?;
    assert!(files.len() > 10, "{files:?}");
    let failures: Vec<String> = files.iter().filter_map(|file| toml_file(file).and_then(|value| validate(&schema, &value)).err().map(|e| format!("{}: {e}", file.display()))).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

const BASE: &str = "dictionary = \"dictionary\"\n[lua]\nscript = \"auth.lua\"\n";

fn check(schema: &Value, extra: &str) -> Result<(), String> {
    validate(schema, &toml_text(&format!("{BASE}{extra}"))?)
}

#[test]
fn test_mistakes_are_found() -> Result<(), String> {
    let schema = printed_schema()?;
    let cases = [
        ("[radius]\nauth_prot = 1812\n", "unknown property `auth_prot`"),
        ("[radius]\nauth_port = \"1812\"\n", "/radius/auth_port"),
        ("[[auth.handlers]]\ntype = \"nis\"\n", "/auth/handlers/0/type: \"nis\" is not"),
    ];
    for (extra, expected) in cases {
        let result = check(&schema, extra);
        assert!(result.as_ref().is_err_and(|e| e.contains(expected)), "{extra:?}: {result:?}");
    }
    // The old place of the script, before the tables as TOML has it.
    let result = validate(&schema, &toml_text(&format!("script = \"auth.lua\"\n{BASE}"))?);
    assert!(result.as_ref().is_err_and(|e| e.contains("unknown property `script`")), "{result:?}");
    check(&schema, "[radius]\nbind = [\"127.0.0.1\", \"::1\"]\n")?;
    check(&schema, "[radius]\nbind = \"0.0.0.0\"\n")?;
    Ok(())
}

#[test]
fn test_enterprise_sections_are_unknown() -> Result<(), String> {
    let schema = printed_schema()?;
    for (extra, unknown) in [
        ("[cluster]\ndata_dir = \"data\"\n[cluster.backup]\n", "unknown property `backup`"),
        ("[radius.tls]\ncert_file = \"c.pem\"\n", "unknown property `tls`"),
        ("[proxy]\n", "unknown property `proxy`"),
    ] {
        let result = check(&schema, extra);
        assert!(result.as_ref().is_err_and(|e| e.contains(unknown)), "{extra:?}: {result:?}");
    }
    Ok(())
}
