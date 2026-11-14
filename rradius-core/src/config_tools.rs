//! `rradius-server config`: the effective configuration with its defaults filled in (`show`),
//! the differences between two configurations (`diff`), and a starting configuration (`init`).
//! All three follow the configuration's JSON Schema, so a setting the schema has is covered
//! without more code here. None of them writes a file.

use crate::extension::Extensions;
use crate::schema::{Descriptions, Lang};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

pub const USAGE: &str = "usage: rradius-server config show [--config <path>]\n\
                         usage: rradius-server config diff <a.toml> <b.toml>\n\
                         usage: rradius-server config init [--lang en|zh-TW]";

/// What `show` prints in place of a secret.
pub const HIDDEN: &str = "<hidden>";

/// The settings that are expected to differ between the nodes of one cluster.
pub const NODE_SPECIFIC: &[&str] = &["cluster.node_id", "cluster.listen"];

/// The lists whose elements `diff` matches by name, or by address without one, since their order
/// does not matter.
pub const KEYED_LISTS: &[&str] = &["radius.clients", "tacacs.clients"];

/// The length of the secrets `init` makes.
pub const SECRET_LEN: usize = 32;

/// The `config` command. Returns the exit status.
pub fn run(args: &[String], extensions: &Extensions) -> i32 {
    crate::logging::init_for_command();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["show"] => show(crate::config::default_config_path(), extensions),
        ["show", "--config", path] => show(path, extensions),
        ["diff", a, b] => diff(a, b, extensions),
        ["init"] => init(Lang::En, extensions),
        ["init", "--lang", name] => match Lang::parse(name) {
            Some(lang) => init(lang, extensions),
            None => usage(),
        },
        _ => usage(),
    };
    match result {
        Ok((text, status)) => {
            print!("{text}");
            status
        }
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

fn usage() -> Result<(String, i32), String> {
    Err(crate::cli::named(USAGE))
}

/// What `show` prints for the configuration at `path`.
fn show(path: &str, extensions: &Extensions) -> Result<(String, i32), String> {
    let (table, computed) = loaded(path, extensions)?;
    let (_, descriptions) = schema_and_descriptions(extensions)?;
    let mut table = table;
    hide_secrets(&mut table, &descriptions);
    let mut text = String::new();
    if !computed.is_empty() {
        let list: Vec<String> = computed.iter().map(|(path, value)| format!("{path} = {value}")).collect();
        let _ = writeln!(text, "# computed on this machine: {}", list.join(", "));
    }
    text.push_str(&toml::to_string(&table).map_err(|e| format!("{path}: {e}"))?);
    Ok((text, 0))
}

/// What `diff` prints for the configurations at `a` and `b`, and its status: 1 when they differ.
fn diff(a: &str, b: &str, extensions: &Extensions) -> Result<(String, i32), String> {
    let (left, _) = loaded(a, extensions)?;
    let (right, _) = loaded(b, extensions)?;
    let (_, descriptions) = schema_and_descriptions(extensions)?;
    let (left, right) = (flatten(&left), flatten(&right));
    let mut differences = String::new();
    let mut node = String::new();
    let paths: std::collections::BTreeSet<&String> = left.keys().chain(right.keys()).collect();
    for path in paths {
        let (one, other) = (left.get(path), right.get(path));
        if one == other {
            continue;
        }
        let line = if descriptions.is_secret(&setting_path(path)) {
            format!("{path}: differs (hidden)\n")
        } else {
            format!("{path}: {} | {}\n", shown(one), shown(other))
        };
        if NODE_SPECIFIC.contains(&path.as_str()) {
            node.push_str("  ");
            node.push_str(&line);
        } else {
            differences.push_str(&line);
        }
    }
    let status = if differences.is_empty() { 0 } else { 1 };
    let mut text = if differences.is_empty() { "no differences\n".to_string() } else { differences };
    if !node.is_empty() {
        text.push_str("node-specific:\n");
        text.push_str(&node);
    }
    Ok((text, status))
}

fn shown(value: Option<&toml::Value>) -> String {
    value.map_or_else(|| "(absent)".to_string(), toml::Value::to_string)
}

/// The schema of the configuration, with the sections of `extensions`, and their descriptions.
fn schema_and_descriptions(extensions: &Extensions) -> Result<(Value, Descriptions), String> {
    let schema = crate::schema::build(crate::cli::program_name(), extensions)?;
    let (descriptions, _) = crate::schema::descriptions(extensions)?;
    Ok((schema, descriptions))
}

/// The configuration at `path`, checked as the server loads it, with its defaults filled in; and
/// the settings filled in with values computed on this machine.
/// Settings filled in with values computed on this machine, and those values.
type Computed = Vec<(&'static str, String)>;

fn loaded(path: &str, extensions: &Extensions) -> Result<(toml::Table, Computed), String> {
    let config = crate::config::load_with(Path::new(path), extensions).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut table: toml::Table = text.parse().map_err(|e| format!("{path}: {e}"))?;
    // Defaults the configuration computes as it loads, from this machine; the schema holds the
    // value of the machine that built it, or none.
    let missing = |table: &toml::Table, section: &str, name: &str| table.get(section).and_then(toml::Value::as_table).is_some_and(|section| !section.contains_key(name));
    let mut computed = Vec::new();
    if let Some(radius) = config.radius.as_ref().filter(|_| missing(&table, "radius", "sockets")) {
        computed.push(("radius", "sockets", radius.sockets as i64));
    }
    if missing(&table, "cluster", "read_connections") {
        computed.push(("cluster", "read_connections", config.cluster.read_connections as i64));
    }
    let (schema, _) = schema_and_descriptions(extensions)?;
    fill(&mut table, &schema);
    // `[shutdown]` is in effect without the section, and its delay follows the number of nodes.
    let shutdown = table.entry("shutdown").or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(shutdown) = shutdown {
        let millis = |duration: std::time::Duration| toml::Value::Integer(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX));
        shutdown.insert("ready_delay_ms".to_string(), millis(config.shutdown.ready_delay));
        shutdown.insert("drain_ms".to_string(), millis(config.shutdown.drain));
    }
    let mut named = Vec::new();
    for (section, name, value) in computed {
        if let Some(toml::Value::Table(inner)) = table.get_mut(section) {
            inner.insert(name.to_string(), toml::Value::Integer(value));
            named.push((if section == "radius" { "radius.sockets" } else { "cluster.read_connections" }, value.to_string()));
        }
    }
    Ok((table, named))
}

/// Fills in, in `table` and the tables it has, every setting its `schema` gives a default and
/// the table leaves out. Sections the table does not have are not added, and nor are defaults
/// that are tables themselves.
pub fn fill(table: &mut toml::Table, schema: &Value) {
    let Some(node) = matching_branch(schema, table) else {
        return;
    };
    if let Some(Value::Object(properties)) = node.get("properties") {
        for (name, property) in properties {
            match table.get_mut(name) {
                Some(toml::Value::Table(inner)) => fill(inner, property),
                Some(toml::Value::Array(elements)) => {
                    if let Some(items) = object_schema(property).and_then(|property| property.get("items")) {
                        for element in elements.iter_mut() {
                            if let toml::Value::Table(inner) = element {
                                fill(inner, items);
                            }
                        }
                    }
                }
                Some(_) => {}
                None => {
                    if let Some(value) = property.get("default").and_then(to_toml) {
                        if !matches!(value, toml::Value::Table(_)) {
                            table.insert(name.clone(), value);
                        }
                    }
                }
            }
        }
    }
    // A map of tables, such as the named chains.
    if let Some(values @ Value::Object(_)) = node.get("additionalProperties") {
        for (_, value) in table.iter_mut() {
            if let toml::Value::Table(inner) = value {
                fill(inner, values);
            }
        }
    }
}

/// The schema of an object or array in `schema`: itself, or its first branch that is not null.
fn object_schema(schema: &Value) -> Option<&Value> {
    object_branches(schema).into_iter().next()
}

/// Every schema of an object or array in `schema`: itself, or each of its branches that is not
/// null, such as the kinds of handler.
fn object_branches(schema: &Value) -> Vec<&Value> {
    if schema.get("properties").is_some() || schema.get("items").is_some() || schema.get("additionalProperties").is_some_and(Value::is_object) {
        return vec![schema];
    }
    let mut found = Vec::new();
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(branches)) = schema.get(keyword) {
            found.extend(branches.iter().flat_map(object_branches));
        }
    }
    found
}

/// The branch of `schema` that `table` is: the first whose fixed values (`const`), such as a
/// handler's `type`, the table has.
fn matching_branch<'a>(schema: &'a Value, table: &toml::Table) -> Option<&'a Value> {
    let branches = object_branches(schema);
    let matches = |branch: &&Value| {
        branch.get("properties").and_then(Value::as_object).is_none_or(|properties| {
            properties.iter().all(|(name, property)| match property.get("const") {
                Some(fixed) => table.get(name).is_some_and(|value| to_toml(fixed).as_ref() == Some(value)),
                None => true,
            })
        })
    };
    branches.iter().find(|branch| matches(branch)).or(branches.first()).copied()
}

/// A JSON value as TOML; `None` for null, which TOML cannot hold.
fn to_toml(value: &Value) -> Option<toml::Value> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(flag) => toml::Value::Boolean(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => toml::Value::Integer(integer),
            None => toml::Value::Float(number.as_f64()?),
        },
        Value::String(text) => toml::Value::String(text.clone()),
        Value::Array(values) => toml::Value::Array(values.iter().filter_map(to_toml).collect()),
        Value::Object(map) => toml::Value::Table(map.iter().filter_map(|(key, value)| Some((key.clone(), to_toml(value)?))).collect()),
    })
}

/// Puts `HIDDEN` in place of every secret setting of `table`.
pub fn hide_secrets(table: &mut toml::Table, descriptions: &Descriptions) {
    hide_in(table, "", descriptions);
}

fn hide_in(table: &mut toml::Table, path: &str, descriptions: &Descriptions) {
    for (name, value) in table.iter_mut() {
        let inner = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
        match value {
            toml::Value::Table(nested) => hide_in(nested, &inner, descriptions),
            toml::Value::Array(elements) if elements.iter().all(toml::Value::is_table) && !elements.is_empty() => {
                for element in elements.iter_mut() {
                    if let toml::Value::Table(nested) = element {
                        hide_in(nested, &inner, descriptions);
                    }
                }
            }
            _ if descriptions.is_secret(&inner) => *value = toml::Value::String(HIDDEN.to_string()),
            _ => {}
        }
    }
}

/// Every setting of `table` by its path. Tables of a list are one level each: those of
/// `KEYED_LISTS` under their name or address, `radius.clients[core-switches]`, the others under
/// their position, `auth.handlers[0]`. A list of values is one setting.
pub fn flatten(table: &toml::Table) -> BTreeMap<String, toml::Value> {
    let mut settings = BTreeMap::new();
    flatten_into(table, "", "", &mut settings);
    settings
}

/// `setting` is the path without the list keys, which says what kind of setting it is.
fn flatten_into(table: &toml::Table, path: &str, setting: &str, settings: &mut BTreeMap<String, toml::Value>) {
    for (name, value) in table {
        let inner = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
        let kind = if setting.is_empty() { name.clone() } else { format!("{setting}.{name}") };
        match value {
            toml::Value::Table(nested) => flatten_into(nested, &inner, &kind, settings),
            toml::Value::Array(elements) if !elements.is_empty() && elements.iter().all(toml::Value::is_table) => {
                for (index, element) in elements.iter().enumerate() {
                    let toml::Value::Table(nested) = element else { continue };
                    let key = KEYED_LISTS
                        .contains(&kind.as_str())
                        .then(|| ["name", "address"].iter().find_map(|field| nested.get(*field).and_then(toml::Value::as_str)).map(str::to_string))
                        .flatten()
                        .unwrap_or_else(|| index.to_string());
                    flatten_into(nested, &format!("{inner}[{key}]"), &kind, settings);
                }
            }
            _ => {
                settings.insert(inner, value.clone());
            }
        }
    }
}

/// The path of a setting with its list keys taken out: `radius.clients[a].secret` is
/// `radius.clients.secret`.
fn setting_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut depth = 0;
    for c in path.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// What `init` prints, in `lang`.
fn init(lang: Lang, extensions: &Extensions) -> Result<(String, i32), String> {
    let secrets = [random_secret()?, random_secret()?, random_secret()?];
    Ok((init_text(lang, extensions, &secrets)?, 0))
}

/// `SECRET_LEN` random letters and digits.
pub fn random_secret() -> Result<String, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut secret = String::with_capacity(SECRET_LEN);
    while secret.len() < SECRET_LEN {
        let mut bytes = [0u8; 64];
        getrandom::fill(&mut bytes).map_err(|e| format!("config init: no random values: {e}"))?;
        // Bytes past the last whole run of the alphabet are dropped, so each character is as likely.
        let limit = (256 / ALPHABET.len() * ALPHABET.len()) as u8;
        for byte in bytes.into_iter().filter(|byte| *byte < limit) {
            if secret.len() == SECRET_LEN {
                break;
            }
            secret.push(char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]));
        }
    }
    Ok(secret)
}

/// The starting configuration in `lang`, with `secrets` for the Raft and API secrets and the
/// example client, in that order.
pub fn init_text(lang: Lang, extensions: &Extensions, secrets: &[String; 3]) -> Result<String, String> {
    let schema = crate::schema::build_in(crate::cli::program_name(), extensions, lang)?;
    let working: toml::Table = format!(
        "dictionary = \"/usr/share/rradius/dictionary\"\n\
         [lua]\nscript = \"auth.lua\"\n\
         [cluster]\ndata_dir = \"data\"\nsecret_raft = \"{}\"\nsecret_api = \"{}\"\n\
         [[cluster.nodes]]\nid = 1\nraft = \"127.0.0.1:8100\"\napi = \"127.0.0.1:8200\"\n\
         [radius]\n\
         [[radius.clients]]\nname = \"example\"\naddress = \"127.0.0.1\"\nsecret = \"{}\"\n",
        secrets[0], secrets[1], secrets[2]
    )
    .parse()
    .map_err(|e| format!("config init: {e}"))?;
    let mut out = String::new();
    let heading = match lang {
        Lang::En => "# rradius configuration made by `config init`.\n\
                     # Every node of the cluster needs the same secret_raft and secret_api; the example\n\
                     # client's secret goes into the NAS too.\n\
                     # Point dictionary at the installed dictionary, and create the script auth.lua.\n\
                     # A commented setting shows its default; remove the # to set it.\n",
        Lang::ZhTw => "# 由 `config init` 產生的 rradius 設定檔。\n\
                       # 叢集的每個節點都要用相同的 secret_raft 與 secret_api；範例用戶端的 secret 也要設在 NAS 上。\n\
                       # 把 dictionary 指向實際安裝的字典檔，並自己建立腳本 auth.lua。\n\
                       # 註解掉的設定列出預設值；拿掉 # 即可設定。\n",
    };
    out.push_str(heading);
    out.push('\n');
    write_section(&mut out, &schema, "", Some(&working), false);
    Ok(out)
}

/// Writes the settings of the table at `path`, whose `schema` is given: those `working` has as
/// settings, the others as comments, then its tables. `in_list` makes a header `[[path]]`.
fn write_section(out: &mut String, schema: &Value, path: &str, working: Option<&toml::Table>, in_list: bool) {
    let Some(outer) = object_schema(schema) else {
        return;
    };
    let target = if in_list { outer.get("items").unwrap_or(outer) } else { schema };
    // Each kind, such as each kind of handler, shown on its own when none is given.
    let node = match working {
        Some(table) => matching_branch(target, table),
        None => {
            let branches = object_branches(target);
            if branches.len() > 1 {
                for branch in branches {
                    write_branch(out, branch, path, None, in_list, schema);
                }
                return;
            }
            branches.into_iter().next()
        }
    };
    if let Some(node) = node {
        write_branch(out, node, path, working, in_list, schema);
    }
}

/// `write_section` for one branch `node`; `outer` is the property's own schema, for its
/// description.
fn write_branch(out: &mut String, node: &Value, path: &str, working: Option<&toml::Table>, in_list: bool, outer: &Value) {
    let commented = working.is_none();
    if !path.is_empty() {
        let (open, close) = if in_list { ("[[", "]]") } else { ("[", "]") };
        let _ = writeln!(out, "\n{}{open}{path}{close}", if commented { "# " } else { "" });
        if let Some(description) = outer.get("description").and_then(Value::as_str) {
            let _ = writeln!(out, "# {description}");
        }
    }
    let Some(Value::Object(properties)) = node.get("properties") else {
        return;
    };
    let mut tables = Vec::new();
    for (name, property) in properties {
        let inner = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
        let given = working.and_then(|table| table.get(name));
        let structure = object_schema(property);
        let is_list_of_tables = structure.and_then(|structure| structure.get("items")).and_then(object_schema).is_some_and(|items| items.get("properties").is_some());
        let is_table = structure.is_some_and(|structure| structure.get("properties").is_some());
        if is_table || is_list_of_tables || structure.is_some_and(|structure| structure.get("additionalProperties").is_some_and(Value::is_object) && structure.get("properties").is_none()) {
            tables.push((inner, property, given, is_list_of_tables));
            continue;
        }
        if let Some(description) = property.get("description").and_then(Value::as_str) {
            let _ = writeln!(out, "# {description}");
        }
        match given {
            Some(value) => {
                let _ = writeln!(out, "{name} = {value}");
            }
            None => match property.get("const").or(property.get("default")).and_then(to_toml) {
                Some(value) => {
                    let _ = writeln!(out, "# {name} = {value}");
                }
                None => {
                    let _ = writeln!(out, "# {name} =");
                }
            },
        }
    }
    for (inner, property, given, is_list) in tables {
        match given {
            Some(toml::Value::Array(elements)) => {
                for element in elements {
                    write_section(out, property, &inner, element.as_table(), true);
                }
            }
            Some(toml::Value::Table(table)) => write_section(out, property, &inner, Some(table), false),
            _ if is_list => write_section(out, property, &inner, None, true),
            _ => {
                // A map of named tables, such as the chains, shown with a placeholder name.
                let is_map = object_schema(property).is_some_and(|structure| structure.get("properties").is_none());
                let shown = if is_map { format!("{inner}.<name>") } else { inner };
                let schema = if is_map { object_schema(property).and_then(|structure| structure.get("additionalProperties")).unwrap_or(property) } else { property };
                write_section(out, schema, &shown, None, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn table(text: &str) -> Result<toml::Table, String> {
        text.parse().map_err(|e: toml::de::Error| e.to_string())
    }

    fn sample_schema() -> Value {
        json!({
            "properties": {
                "radius": {"anyOf": [{"type": "null"}, {"properties": {
                    "auth_port": {"type": "integer", "default": 1812},
                    "secretless": {"default": null},
                    "clients": {"type": "array", "items": {"properties": {
                        "address": {"type": "string"},
                        "require_message_authenticator": {"type": "string", "default": "no"}
                    }}}
                }}]},
                "tacacs": {"anyOf": [{"type": "null"}, {"properties": {"port": {"type": "integer", "default": 49}}}]},
                "log": {"properties": {"level": {"type": "string", "default": "info"}}, "default": {"level": "info"}}
            }
        })
    }

    #[test]
    fn test_fill_defaults() -> Result<(), String> {
        let schema = sample_schema();
        let mut config = table("[radius]\n[[radius.clients]]\naddress = \"10.0.0.1\"\n[[radius.clients]]\naddress = \"10.0.0.2\"\nrequire_message_authenticator = \"yes\"\n")?;
        fill(&mut config, &schema);
        let radius = config.get("radius").and_then(toml::Value::as_table).ok_or("no radius")?;
        assert_eq!(radius.get("auth_port"), Some(&toml::Value::Integer(1812)));
        assert!(!radius.contains_key("secretless"), "a null default is left out");
        let clients = radius.get("clients").and_then(toml::Value::as_array).ok_or("no clients")?;
        let policies: Vec<Option<&str>> = clients.iter().map(|client| client.get("require_message_authenticator").and_then(toml::Value::as_str)).collect();
        assert_eq!(policies, [Some("no"), Some("yes")]);
        assert!(!config.contains_key("tacacs"), "a section the file lacks is not added");
        assert!(!config.contains_key("log"), "nor a table default");
        // A value the file sets stays.
        let mut set = table("[radius]\nauth_port = 11812\n")?;
        fill(&mut set, &schema);
        assert_eq!(set.get("radius").and_then(|radius| radius.get("auth_port")), Some(&toml::Value::Integer(11812)));
        Ok(())
    }

    fn secret_table() -> Result<Descriptions, String> {
        Descriptions::parse(
            "[fields.\"radius.clients.secret\"]\nen = \"x\"\nzh-TW = \"x\"\nsecret = true\n\
             [fields.\"cluster.secret_raft\"]\nen = \"x\"\nzh-TW = \"x\"\nsecret = true\n\
             [same_as]\n\"tacacs.clients\" = \"radius.clients\"\n",
        )
    }

    #[test]
    fn test_hide_secrets() -> Result<(), String> {
        let mut config = table("[cluster]\nsecret_raft = \"raft-secret\"\ndata_dir = \"data\"\n[[radius.clients]]\naddress = \"10.0.0.1\"\nsecret = \"s3cret\"\n[[tacacs.clients]]\naddress = \"10.0.0.1\"\nsecret = \"t\"\n")?;
        hide_secrets(&mut config, &secret_table()?);
        let text = toml::to_string(&config).map_err(|e| e.to_string())?;
        assert!(!text.contains("s3cret") && !text.contains("raft-secret") && !text.contains("\"t\""), "{text}");
        assert_eq!(text.matches(HIDDEN).count(), 3, "{text}");
        assert!(text.contains("data_dir = \"data\""), "{text}");
        Ok(())
    }

    #[test]
    fn test_flatten_keys() -> Result<(), String> {
        let one = flatten(&table("[[radius.clients]]\nname = \"a\"\naddress = \"10.0.0.1\"\n[[radius.clients]]\naddress = \"10.0.0.2\"\n[[auth.handlers]]\ntype = \"lua\"\n[radius]\nbind = [\"0.0.0.0\", \"::\"]\n")?);
        let other = flatten(&table("[[radius.clients]]\naddress = \"10.0.0.2\"\n[[radius.clients]]\nname = \"a\"\naddress = \"10.0.0.1\"\n[[auth.handlers]]\ntype = \"lua\"\n[radius]\nbind = [\"0.0.0.0\", \"::\"]\n")?);
        assert_eq!(one, other, "clients in another order are the same");
        assert!(one.contains_key("radius.clients[a].address") && one.contains_key("radius.clients[10.0.0.2].address"), "{one:?}");
        assert!(one.contains_key("auth.handlers[0].type") && one.contains_key("radius.bind"), "{one:?}");
        assert_eq!(setting_path("radius.clients[a].secret"), "radius.clients.secret");
        Ok(())
    }

    #[test]
    fn test_fill_by_kind() -> Result<(), String> {
        let schema = crate::schema::build("rradius-server", &crate::extensions::builtin())?;
        let mut config = table("dictionary = \"d\"\n[[auth.handlers]]\ntype = \"lua\"\nscript = \"h.lua\"\n[[auth.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT 1\"\n")?;
        fill(&mut config, &schema);
        let handlers = config.get("auth").and_then(|auth| auth.get("handlers")).and_then(toml::Value::as_array).ok_or("no handlers")?;
        // Each handler gets the defaults of its own kind.
        assert_eq!(handlers.first().and_then(|lua| lua.get("timeout_ms")), None);
        assert_eq!(handlers.first().and_then(|lua| lua.get("verifies_password")), Some(&toml::Value::Boolean(false)));
        assert_eq!(handlers.get(1).and_then(|sql| sql.get("timeout_ms")), Some(&toml::Value::Integer(5000)));
        Ok(())
    }

    #[test]
    fn test_random_secret() -> Result<(), String> {
        let (one, other) = (random_secret()?, random_secret()?);
        assert_ne!(one, other);
        for secret in [&one, &other] {
            assert!(secret.len() == SECRET_LEN && secret.chars().all(|c| c.is_ascii_alphanumeric()), "{secret}");
        }
        Ok(())
    }

    #[test]
    fn test_init_text() -> Result<(), String> {
        let secrets = ["R".repeat(SECRET_LEN), "A".repeat(SECRET_LEN), "C".repeat(SECRET_LEN)];
        let extensions = crate::extensions::builtin();
        for lang in [Lang::En, Lang::ZhTw] {
            let text = init_text(lang, &extensions, &secrets)?;
            let parsed: toml::Table = text.parse().map_err(|e: toml::de::Error| format!("{e}\n{text}"))?;
            let cluster = parsed.get("cluster").and_then(toml::Value::as_table).ok_or("no cluster")?;
            assert_eq!(cluster.get("secret_raft").and_then(toml::Value::as_str), Some(secrets[0].as_str()));
            assert_eq!(cluster.get("secret_api").and_then(toml::Value::as_str), Some(secrets[1].as_str()));
            assert!(text.contains("\n# max_auth_inflight = 4096\n"), "{text}");
            assert!(text.contains("\n# [tacacs]\n") && text.contains("\n# [[tacacs.clients]]\n"), "{text}");
            let english = text.contains("# Point dictionary at the installed dictionary");
            assert_eq!(english, lang == Lang::En, "{text}");
        }
        Ok(())
    }
}
