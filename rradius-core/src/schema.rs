//! The JSON Schema of the configuration file, which the `schema` command prints: the sections the
//! core reads, from the structures that read them, with the sections of the program's extensions
//! added where they belong.

use crate::extension::Extensions;
use serde_json::{Map, Value};

/// Where each top-level key is described in the usage document.
const USAGE: &str = "https://github.com/embark-systems/rradius/blob/main/docs/usage.md";
const CHAPTERS: &[(&str, &str)] = &[
    ("dictionary", "#2-設定檔總覽"),
    ("lua", "#21-lua"),
    ("cluster", "#22-cluster"),
    ("radius", "#23-radius-與-tacacs"),
    ("tacacs", "#23-radius-與-tacacs"),
    ("runtime", "#24-過載與重送"),
    ("metrics", "#28-統計-metrics"),
    ("log", "#210-記錄-log"),
    ("accounts", "#214-帳號管理-accounts"),
    ("auth", "#3-認證串接-auth"),
];

/// The schema of the configuration file of `program`, with the sections of `extensions`.
pub fn build(program: &str, extensions: &Extensions) -> Result<Value, String> {
    let mut root = crate::config::core_schema().to_value();
    without_descriptions(&mut root);
    let object = root.as_object_mut().ok_or("the core schema is not an object")?;
    object.insert("title".to_string(), Value::String(format!("{program} configuration")));
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for (key, chapter) in CHAPTERS {
            if let Some(Value::Object(property)) = properties.get_mut(*key) {
                property.insert("description".to_string(), Value::String(format!("{USAGE}{chapter}")));
            }
        }
    }
    for extension in extensions.iter() {
        for (path, schema) in extension.schemas() {
            let mut schema = schema.to_value();
            // A link the extension gave its section stays.
            let link = schema.get("description").and_then(Value::as_str).filter(|text| text.starts_with("https://")).map(str::to_string);
            without_descriptions(&mut schema);
            if let (Some(link), Some(object)) = (link, schema.as_object_mut()) {
                object.insert("description".to_string(), Value::String(link));
            }
            add(&mut root, path, schema).map_err(|e| format!("extension {}: {e}", extension.name()))?;
        }
    }
    Ok(root)
}

/// Removes the descriptions schemars made of the code's comments, which are written for its
/// readers and not for those who write the configuration file.
fn without_descriptions(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    object.remove("description");
    for (keyword, inner) in object.iter_mut() {
        match keyword.as_str() {
            // Maps from names to schemas.
            "properties" | "$defs" => {
                if let Some(named) = inner.as_object_mut() {
                    named.values_mut().for_each(without_descriptions);
                }
            }
            "anyOf" | "oneOf" | "allOf" => {
                if let Some(list) = inner.as_array_mut() {
                    list.iter_mut().for_each(without_descriptions);
                }
            }
            "items" | "additionalProperties" | "not" => without_descriptions(inner),
            _ => {}
        }
    }
}

/// Adds `section` at the dotted `path` of `root`: the last part becomes a property of the object
/// the parts before it lead to, through `$ref`s into `$defs`.
fn add(root: &mut Value, path: &str, mut section: Value) -> Result<(), String> {
    let parts: Vec<&str> = path.split('.').collect();
    let Some((key, parents)) = parts.split_last() else {
        return Err(format!("`{path}`: an empty path"));
    };
    // The section's definitions move to the root's.
    let definitions = match section.as_object_mut() {
        Some(object) => {
            object.remove("$schema");
            object.remove("title");
            object.remove("$defs")
        }
        None => None,
    };
    if let Some(Value::Object(definitions)) = definitions {
        let root_definitions = root.as_object_mut().ok_or("the schema is not an object")?.entry("$defs").or_insert_with(|| Value::Object(Map::new()));
        let root_definitions = root_definitions.as_object_mut().ok_or("$defs is not an object")?;
        for (name, definition) in definitions {
            match root_definitions.get(&name) {
                Some(existing) if *existing != definition => return Err(format!("`{path}`: a different definition of `{name}` is already there")),
                Some(_) => {}
                None => {
                    root_definitions.insert(name, definition);
                }
            }
        }
    }
    // The object the parents lead to: the root, or a definition.
    let mut place: Option<String> = None;
    for parent in parents {
        let object = match &place {
            None => &*root,
            Some(name) => root.get("$defs").and_then(|definitions| definitions.get(name)).ok_or_else(|| format!("`{path}`: no definition `{name}`"))?,
        };
        let property = object.get("properties").and_then(|properties| properties.get(*parent)).ok_or_else(|| format!("`{path}`: there is no `{parent}` to add it to"))?;
        place = Some(reference(property).ok_or_else(|| format!("`{path}`: `{parent}` is not a section"))?);
    }
    let object = match &place {
        None => root.as_object_mut(),
        Some(name) => root.get_mut("$defs").and_then(|definitions| definitions.get_mut(name)).and_then(Value::as_object_mut),
    }
    .ok_or_else(|| format!("`{path}`: its parent is not an object"))?;
    let properties = object.entry("properties").or_insert_with(|| Value::Object(Map::new())).as_object_mut().ok_or_else(|| format!("`{path}`: its parent's properties are not an object"))?;
    if properties.contains_key(*key) {
        return Err(format!("`{path}`: already in the schema"));
    }
    properties.insert(key.to_string(), section);
    Ok(())
}

/// The definition a property's schema points to, directly or as the one branch of an `anyOf`
/// besides `null` (an optional section).
fn reference(property: &Value) -> Option<String> {
    if let Some(target) = property.get("$ref").and_then(Value::as_str) {
        return target.strip_prefix("#/$defs/").map(str::to_string);
    }
    let branches = property.get("anyOf")?.as_array()?;
    let mut found = branches.iter().filter_map(reference);
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::Extension;
    use crate::test_support::schema::{toml_text, validate};
    use std::sync::Arc;

    #[derive(schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Probe {
        name: String,
    }

    struct ProbeExtension(&'static str);

    impl Extension for ProbeExtension {
        fn name(&self) -> &'static str {
            "probe"
        }
        fn schemas(&self) -> Vec<(&'static str, schemars::Schema)> {
            vec![(self.0, schemars::schema_for!(Probe))]
        }
    }

    const BASE: &str = "dictionary = \"d\"\n[lua]\nscript = \"s.lua\"\n";

    #[test]
    fn test_an_extension_adds_a_section() -> Result<(), String> {
        let extensions = Extensions::new(vec![Arc::new(ProbeExtension("radius.probe"))])?;
        let schema = build("probe-program", &extensions)?;
        assert_eq!(schema.get("title").and_then(Value::as_str), Some("probe-program configuration"));
        validate(&schema, &toml_text(&format!("{BASE}[radius.probe]\nname = \"x\"\n"))?)?;
        let missing = validate(&schema, &toml_text(&format!("{BASE}[radius.probe]\n"))?);
        assert!(missing.as_ref().is_err_and(|e| e.contains("missing property `name`")), "{missing:?}");
        // Without the extension, the section is unknown.
        let core = build("rradius-server", &crate::extensions::builtin())?;
        let unknown = validate(&core, &toml_text(&format!("{BASE}[radius.probe]\nname = \"x\"\n"))?);
        assert!(unknown.as_ref().is_err_and(|e| e.contains("unknown property `probe`")), "{unknown:?}");
        Ok(())
    }

    /// In the core's schema `[mfa]` is unknown, and a client's `mfa` takes only the two ways,
    /// which the core program then refuses as needing rradius-enterprise.
    #[test]
    fn test_mfa_in_the_core_schema() -> Result<(), String> {
        let core = build("rradius-server", &crate::extensions::builtin())?;
        let client = "[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        let section = validate(&core, &toml_text(&format!("{BASE}{client}[mfa]\nkey_file = \"mfa.key\"\n"))?);
        assert!(section.as_ref().is_err_and(|e| e.contains("unknown property `mfa`")), "{section:?}");
        validate(&core, &toml_text(&format!("{BASE}{client}mfa = \"challenge\"\n"))?)?;
        let other = validate(&core, &toml_text(&format!("{BASE}{client}mfa = \"sms\"\n"))?);
        assert!(other.is_err(), "{other:?}");
        Ok(())
    }

    #[test]
    fn test_a_path_with_no_parent() -> Result<(), String> {
        let extensions = Extensions::new(vec![Arc::new(ProbeExtension("nowhere.probe"))])?;
        let error = build("probe-program", &extensions).err().unwrap_or_default();
        assert!(error.starts_with("extension probe: `nowhere.probe`"), "{error}");
        let extensions = Extensions::new(vec![Arc::new(ProbeExtension("radius.auth_port"))])?;
        let error = build("probe-program", &extensions).err().unwrap_or_default();
        assert!(error.contains("already in the schema"), "{error}");
        Ok(())
    }

    #[test]
    fn test_top_level_sections_link_the_usage_document() -> Result<(), String> {
        let schema = build("rradius-server", &crate::extensions::builtin())?;
        // The only descriptions are those links.
        let text = schema.to_string();
        let links = CHAPTERS.iter().filter(|(key, _)| schema.get("properties").and_then(|p| p.get(*key)).is_some()).count();
        assert_eq!(text.matches("\"description\"").count(), links, "{text}");
        let properties = schema.get("properties").and_then(Value::as_object).ok_or("no properties")?;
        for (key, property) in properties {
            let description = property.get("description").and_then(Value::as_str).unwrap_or_default();
            assert!(description.starts_with("https://"), "{key}: {description:?}");
        }
        Ok(())
    }
}
