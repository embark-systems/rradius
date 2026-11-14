//! The JSON Schema of the configuration file, which the `schema` command prints: the sections the
//! core reads, from the structures that read them, with the sections of the program's extensions
//! added where they belong.

use crate::extension::Extensions;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The descriptions of the core's fields, in each language.
const CORE_DESCRIPTIONS: &str = include_str!("schema_descriptions.toml");

/// The language of the fields' descriptions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    ZhTw,
}

impl Lang {
    /// The language a `--lang` value names.
    pub fn parse(name: &str) -> Option<Lang> {
        match name {
            "en" => Some(Lang::En),
            "zh-TW" => Some(Lang::ZhTw),
            _ => None,
        }
    }
}

/// A description table: the text of each configuration path, and prefixes that share another's.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Descriptions {
    #[serde(default)]
    fields: BTreeMap<String, Texts>,
    #[serde(default)]
    same_as: BTreeMap<String, String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Texts {
    en: String,
    #[serde(rename = "zh-TW")]
    zh_tw: String,
    /// Whether the setting holds a secret, which `config show` and `config diff` hide.
    #[serde(default)]
    secret: bool,
}

impl Descriptions {
    /// Reads a table in its TOML form.
    pub fn parse(text: &str) -> Result<Descriptions, String> {
        toml::from_str(text).map_err(|e| format!("a description table: {e}"))
    }

    /// Adds the paths of `other`; a path this table has already keeps its text and is returned.
    pub fn merge(&mut self, other: Descriptions) -> Vec<String> {
        let mut repeated = Vec::new();
        for (path, texts) in other.fields {
            match self.fields.entry(path) {
                std::collections::btree_map::Entry::Occupied(entry) => repeated.push(entry.key().clone()),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(texts);
                }
            }
        }
        for (from, to) in other.same_as {
            match self.same_as.entry(from) {
                std::collections::btree_map::Entry::Occupied(entry) => repeated.push(entry.key().clone()),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(to);
                }
            }
        }
        repeated
    }

    /// The text of `path` in `lang`: its own, or through `same_as` that of the path it shares.
    fn text(&self, path: &str, lang: Lang) -> Option<&str> {
        self.entry(path).map(|texts| match lang {
            Lang::En => texts.en.as_str(),
            Lang::ZhTw => texts.zh_tw.as_str(),
        })
    }

    /// Whether the setting at `path` is marked `secret`, its own entry or the one it shares.
    pub fn is_secret(&self, path: &str) -> bool {
        self.entry(path).is_some_and(|texts| texts.secret)
    }

    /// The entry of `path`: its own, or through `same_as` that of the path it shares.
    fn entry(&self, path: &str) -> Option<&Texts> {
        let mut path = path.to_string();
        // A bound on the hops, in case two prefixes point at each other.
        for _ in 0..8 {
            if let Some(texts) = self.fields.get(&path) {
                return Some(texts);
            }
            let shared = self.same_as.iter().find_map(|(from, to)| {
                let rest = path.strip_prefix(from.as_str())?;
                (rest.is_empty() || rest.starts_with('.')).then(|| format!("{to}{rest}"))
            })?;
            path = shared;
        }
        None
    }

    /// The paths and prefixes of this table that name no property of `schema`, a schema `build`
    /// made: left over from a field renamed or removed.
    pub fn unknown_paths(&self, schema: &Value) -> Vec<String> {
        let mut paths = property_paths(schema);
        // A prefix may also be a whole top-level section.
        paths.extend(schema.get("properties").and_then(Value::as_object).into_iter().flat_map(|properties| properties.keys().cloned()));
        let is_path = |path: &str| paths.iter().any(|known| known == path || known.strip_prefix(path).is_some_and(|rest| rest.starts_with('.')));
        let mut unknown: Vec<String> = self.fields.keys().filter(|path| !is_path(path)).cloned().collect();
        unknown.extend(self.same_as.iter().flat_map(|(from, to)| [from, to]).filter(|prefix| !is_path(prefix)).cloned());
        unknown
    }
}

/// The configuration path of every property below the top level of `schema`, a schema `build`
/// made, in the order met.
pub fn property_paths(schema: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    visit(schema, "", true, &mut |path, _| paths.push(path.to_string()));
    paths
}

/// The paths below the top level of `schema` whose property has no description.
pub fn undescribed(schema: &Value) -> Vec<String> {
    let mut missing = Vec::new();
    visit(schema, "", true, &mut |path, property| {
        if property.get("description").and_then(Value::as_str).is_none_or(str::is_empty) {
            missing.push(path.to_string());
        }
    });
    missing
}

/// Calls `found` with the path and schema of each property under `node`, whose own path is
/// `path`, and goes on into it; the properties of the root are passed over but gone into.
fn visit(node: &Value, path: &str, root: bool, found: &mut impl FnMut(&str, &Value)) {
    let Some(object) = node.as_object() else {
        return;
    };
    if let Some(Value::Object(properties)) = object.get("properties") {
        for (name, property) in properties {
            let inner = join(path, name);
            if !root {
                found(&inner, property);
            }
            visit(property, &inner, false, found);
        }
    }
    if let Some(items) = object.get("items") {
        visit(items, path, false, found);
    }
    if let Some(values @ Value::Object(_)) = object.get("additionalProperties") {
        visit(values, &join(path, "*"), false, found);
    }
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(branches)) = object.get(keyword) {
            branches.iter().for_each(|branch| visit(branch, path, false, found));
        }
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() { name.to_string() } else { format!("{path}.{name}") }
}

/// Gives each property below the top level of `node` its description in `lang`, when `table`
/// has one.
fn describe(node: &mut Value, path: &str, root: bool, table: &Descriptions, lang: Lang) {
    let Some(object) = node.as_object_mut() else {
        return;
    };
    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for (name, property) in properties.iter_mut() {
            let inner = join(path, name);
            if !root {
                if let (Some(text), Some(property)) = (table.text(&inner, lang), property.as_object_mut()) {
                    // The link an extension gave its section stays.
                    property.entry("description").or_insert_with(|| Value::String(text.to_string()));
                }
            }
            describe(property, &inner, false, table, lang);
        }
    }
    if let Some(items) = object.get_mut("items") {
        describe(items, path, false, table, lang);
    }
    if let Some(values @ Value::Object(_)) = object.get_mut("additionalProperties") {
        describe(values, &join(path, "*"), false, table, lang);
    }
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(branches)) = object.get_mut(keyword) {
            branches.iter_mut().for_each(|branch| describe(branch, path, false, table, lang));
        }
    }
}

/// Puts each definition in place of the `$ref`s to it and drops `$defs`, so that each property
/// has one path. The definitions do not refer to themselves; a chain deeper than any of them is
/// refused as such.
fn expand(root: &mut Value) -> Result<(), String> {
    let definitions = match root.as_object_mut().and_then(|object| object.remove("$defs")) {
        Some(Value::Object(definitions)) => definitions,
        Some(_) => return Err("$defs is not an object".to_string()),
        None => return Ok(()),
    };
    inline(root, &definitions, 0)
}

fn inline(node: &mut Value, definitions: &Map<String, Value>, depth: usize) -> Result<(), String> {
    if depth > 32 {
        return Err("the definitions refer to themselves".to_string());
    }
    match node {
        Value::Object(object) => {
            if let Some(Value::String(target)) = object.remove("$ref") {
                let name = target.strip_prefix("#/$defs/").ok_or_else(|| format!("a $ref outside $defs: {target}"))?;
                let Some(Value::Object(definition)) = definitions.get(name) else {
                    return Err(format!("no definition `{name}`"));
                };
                for (keyword, value) in definition {
                    object.entry(keyword.clone()).or_insert_with(|| value.clone());
                }
                return inline(node, definitions, depth + 1);
            }
            object.values_mut().try_for_each(|value| inline(value, definitions, depth))
        }
        Value::Array(list) => list.iter_mut().try_for_each(|value| inline(value, definitions, depth)),
        _ => Ok(()),
    }
}

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
    ("shutdown", "#216-停止-shutdown"),
    ("log", "#210-記錄-log"),
    ("accounts", "#214-帳號管理-accounts"),
    ("auth", "#3-認證串接-auth"),
];

/// The schema of the configuration file of `program`, with the sections of `extensions`, its
/// fields described in English.
pub fn build(program: &str, extensions: &Extensions) -> Result<Value, String> {
    build_in(program, extensions, Lang::En)
}

/// The core's description table.
pub fn core_descriptions() -> Result<Descriptions, String> {
    Descriptions::parse(CORE_DESCRIPTIONS)
}

/// The core's table with those of `extensions`, and the paths more than one table names, whose
/// first text is kept.
pub fn descriptions(extensions: &Extensions) -> Result<(Descriptions, Vec<String>), String> {
    let mut table = core_descriptions()?;
    let mut repeated = Vec::new();
    for extension in extensions.iter() {
        let text = extension.schema_descriptions();
        if !text.is_empty() {
            let theirs = Descriptions::parse(text).map_err(|e| format!("extension {}: {e}", extension.name()))?;
            repeated.extend(table.merge(theirs));
        }
    }
    Ok((table, repeated))
}

/// The schema of the configuration file of `program`, with the sections of `extensions`, its
/// fields described in `lang`.
pub fn build_in(program: &str, extensions: &Extensions, lang: Lang) -> Result<Value, String> {
    let mut root = without_field_descriptions(program, extensions)?;
    expand(&mut root)?;
    let (table, _) = descriptions(extensions)?;
    describe(&mut root, "", true, &table, lang);
    Ok(root)
}

/// The schema before the fields are described: the core's, the extensions' sections added.
fn without_field_descriptions(program: &str, extensions: &Extensions) -> Result<Value, String> {
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

/// Settings whose names look like a secret but hold none: SQL, a switch, an attribute name.
pub const NOT_SECRET: &[&str] = &["passwd", "verifies_password", "password_attribute", "password_check"];

/// The paths of `schema` whose names look like a secret but that `table` does not mark.
#[doc(hidden)]
pub fn unmarked_secrets(schema: &Value, table: &Descriptions) -> Vec<String> {
    property_paths(schema)
        .into_iter()
        .filter(|path| {
            let name = path.rsplit('.').next().unwrap_or(path);
            let looks = name.contains("secret") || name.contains("password") || name.contains("passwd") || name == "access_key";
            looks && !name.ends_with("_file") && !NOT_SECRET.contains(&name) && !table.is_secret(path)
        })
        .collect()
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
        fn schema_descriptions(&self) -> &'static str {
            "[fields.\"radius.probe\"]\nen = \"probe\"\nzh-TW = \"探測\"\n\
             [fields.\"radius.probe.name\"]\nen = \"name\"\nzh-TW = \"名稱\"\n\
             [fields.\"radius.auth_port\"]\nen = \"again\"\nzh-TW = \"重複\"\n"
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
        // Its fields are described in both languages.
        let missing = undescribed(&schema);
        assert!(!missing.iter().any(|path| path.starts_with("radius.probe")), "{missing:?}");
        assert!(schema.to_string().contains(r#""description":"name""#));
        let zh = build_in("probe-program", &extensions, Lang::ZhTw)?.to_string();
        assert!(zh.contains(r#""description":"名稱""#) && zh.contains(r#""description":"探測""#));
        // The core's text is kept, and the repeated path reported.
        assert!(!schema.to_string().contains(r#""description":"again""#));
        assert_eq!(descriptions(&extensions)?.1, ["radius.auth_port"]);
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
        let schema = build_in("rradius-server", &crate::extensions::builtin(), Lang::ZhTw)?;
        let properties = schema.get("properties").and_then(Value::as_object).ok_or("no properties")?;
        for (key, property) in properties {
            let description = property.get("description").and_then(Value::as_str).unwrap_or_default();
            assert!(description.starts_with("https://"), "{key}: {description:?}");
        }
        Ok(())
    }

    /// The paths of a table: fields, items of an array of tables, values of a map, `same_as`
    /// and a path of its own winning over it; a path the table lacks gets no description.
    #[test]
    fn test_description_paths() -> Result<(), String> {
        let table = Descriptions::parse(
            "[fields.\"radius.clients.secret\"]\nen = \"secret\"\nzh-TW = \"密碼\"\n\
             [fields.\"tacacs.clients.address\"]\nen = \"own\"\nzh-TW = \"自己的\"\n\
             [fields.\"auth.chains.*.policy\"]\nen = \"policy\"\nzh-TW = \"策略\"\n\
             [same_as]\n\"tacacs.clients\" = \"radius.clients\"\n",
        )?;
        assert_eq!(table.text("radius.clients.secret", Lang::En), Some("secret"));
        assert_eq!(table.text("radius.clients.secret", Lang::ZhTw), Some("密碼"));
        assert_eq!(table.text("tacacs.clients.secret", Lang::ZhTw), Some("密碼"), "through same_as");
        assert_eq!(table.text("tacacs.clients.address", Lang::En), Some("own"), "its own path wins");
        assert_eq!(table.text("tacacs.clientsx.secret", Lang::En), None, "a prefix ends at a dot");
        assert_eq!(table.text("radius.clients.name", Lang::En), None);
        let mut schema = without_field_descriptions("rradius-server", &crate::extensions::builtin())?;
        expand(&mut schema)?;
        assert!(!schema.to_string().contains("$ref") && schema.get("$defs").is_none());
        describe(&mut schema, "", true, &table, Lang::ZhTw);
        let paths = property_paths(&schema);
        for path in ["radius.clients.secret", "tacacs.clients.secret", "auth.chains.*.policy", "radius.auth_port"] {
            assert!(paths.iter().any(|known| known == path), "{path}: {paths:?}");
        }
        let missing = undescribed(&schema);
        for path in ["radius.clients.secret", "tacacs.clients.secret", "tacacs.clients.address", "auth.chains.*.policy"] {
            assert!(!missing.iter().any(|known| known == path), "{path} has no description");
        }
        assert!(missing.iter().any(|known| known == "radius.auth_port"));
        assert!(Descriptions::parse("[fields.\"x\"]\nen = \"x\"\n").is_err(), "both languages");
        let stale = Descriptions::parse("[fields.\"radius.nothing\"]\nen = \"x\"\nzh-TW = \"x\"\n[same_as]\n\"nowhere\" = \"radius\"\n")?;
        // `radius`, a top-level section, is known; `nowhere` is not.
        assert_eq!(stale.unknown_paths(&schema), ["radius.nothing", "nowhere"]);
        Ok(())
    }

    #[test]
    fn test_secret_settings_are_marked() -> Result<(), String> {
        let extensions = crate::extensions::builtin();
        let schema = build("rradius-server", &extensions)?;
        let (table, _) = descriptions(&extensions)?;
        assert_eq!(unmarked_secrets(&schema, &table), Vec::<String>::new());
        for path in ["radius.clients.secret", "tacacs.clients.secret", "cluster.secret_raft", "cluster.secret_api", "auth.handlers.bind_password"] {
            assert!(table.is_secret(path), "{path}");
        }
        assert!(!table.is_secret("radius.clients.secret_file") && !table.is_secret("radius.auth_port"));
        Ok(())
    }

    #[test]
    fn test_every_core_field_is_described() -> Result<(), String> {
        for lang in [Lang::En, Lang::ZhTw] {
            let schema = build_in("rradius-server", &crate::extensions::builtin(), lang)?;
            assert_eq!(undescribed(&schema), Vec::<String>::new(), "{lang:?}");
            assert_eq!(core_descriptions()?.unknown_paths(&schema), Vec::<String>::new());
            assert_eq!(descriptions(&crate::extensions::builtin())?.1, Vec::<String>::new());
        }
        Ok(())
    }
}
