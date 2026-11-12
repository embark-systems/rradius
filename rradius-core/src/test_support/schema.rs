//! A small JSON Schema validator for the tests of the configuration schema. It knows the keywords
//! schemars writes and fails on any other, so that a schema it does not understand is never passed
//! as valid.

use serde_json::{Map, Value};

/// Keywords that only describe and never make a value invalid.
const ANNOTATIONS: &[&str] = &["$schema", "$id", "title", "description", "default", "format", "deprecated", "examples", "$comment", "readOnly", "writeOnly"];

/// Checks `value` against `schema`, whose `$ref`s point into its own `$defs`. The error names the
/// path of the first invalid value, such as `/radius/auth_port`.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, schema, value, "")
}

/// Reads a TOML configuration file into the JSON value the schema describes.
pub fn toml_file(path: &std::path::Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    toml_text(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads TOML text into the JSON value the schema describes.
pub fn toml_text(text: &str) -> Result<Value, String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    serde_json::to_value(table).map_err(|e| e.to_string())
}

fn check(root: &Value, schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let schema = match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(format!("{}: not allowed", shown(path))),
        Value::Object(schema) => schema,
        _ => return Err(format!("{}: a schema that is neither an object nor a boolean", shown(path))),
    };
    for (keyword, rule) in schema {
        match keyword.as_str() {
            k if ANNOTATIONS.contains(&k) => {}
            "$defs" => {}
            "$ref" => check(root, resolve(root, rule)?, value, path)?,
            "type" => check_type(rule, value, path)?,
            "enum" => {
                let allowed = rule.as_array().ok_or("enum is not an array")?;
                if !allowed.contains(value) {
                    return Err(format!("{}: {value} is not one of {rule}", shown(path)));
                }
            }
            "const" => {
                if rule != value {
                    return Err(format!("{}: {value} is not {rule}", shown(path)));
                }
            }
            "properties" => {
                if let (Some(properties), Value::Object(object)) = (rule.as_object(), value) {
                    for (name, inner) in object {
                        if let Some(property) = properties.get(name) {
                            check(root, property, inner, &format!("{path}/{name}"))?;
                        }
                    }
                }
            }
            "additionalProperties" => {
                if let Value::Object(object) = value {
                    let known = schema.get("properties").and_then(Value::as_object);
                    for (name, inner) in object {
                        if known.is_some_and(|known| known.contains_key(name)) {
                            continue;
                        }
                        match rule {
                            Value::Bool(false) => return Err(format!("{}: unknown property `{name}`", shown(path))),
                            other => check(root, other, inner, &format!("{path}/{name}"))?,
                        }
                    }
                }
            }
            "required" => {
                if let Value::Object(object) = value {
                    for name in rule.as_array().ok_or("required is not an array")? {
                        let name = name.as_str().ok_or("a required name that is not a string")?;
                        if !object.contains_key(name) {
                            return Err(format!("{}: missing property `{name}`", shown(path)));
                        }
                    }
                }
            }
            "items" => {
                if let Value::Array(items) = value {
                    for (index, item) in items.iter().enumerate() {
                        check(root, rule, item, &format!("{path}/{index}"))?;
                    }
                }
            }
            "minItems" => {
                if let (Some(least), Value::Array(items)) = (rule.as_u64(), value) {
                    if (items.len() as u64) < least {
                        return Err(format!("{}: fewer than {least} items", shown(path)));
                    }
                }
            }
            "uniqueItems" => {}
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => check_bound(keyword, rule, value, path)?,
            "anyOf" => {
                let options = rule.as_array().ok_or("anyOf is not an array")?;
                let results: Vec<Result<(), String>> = options.iter().map(|option| check(root, option, value, path)).collect();
                if !results.iter().any(Result::is_ok) {
                    let reasons: Vec<&str> = results.iter().filter_map(|result| result.as_ref().err()).map(String::as_str).collect();
                    return Err(format!("{}: {value} matches none of anyOf ({})", shown(path), reasons.join("; ")));
                }
            }
            "oneOf" => {
                let options = rule.as_array().ok_or("oneOf is not an array")?;
                let results: Vec<Result<(), String>> = options.iter().map(|option| check(root, option, value, path)).collect();
                let matching = results.iter().filter(|result| result.is_ok()).count();
                if matching != 1 {
                    // With one failure only, its reason is the useful one.
                    let reasons: Vec<&String> = results.iter().filter_map(|result| result.as_ref().err()).collect();
                    return Err(match reasons.as_slice() {
                        [only] if matching == 0 => (*only).clone(),
                        _ => format!("{}: {value} matches {matching} of oneOf, not 1 ({})", shown(path), reasons.iter().map(|r| r.as_str()).collect::<Vec<_>>().join("; ")),
                    });
                }
            }
            "allOf" => {
                for option in rule.as_array().ok_or("allOf is not an array")? {
                    check(root, option, value, path)?;
                }
            }
            "not" => {
                if check(root, rule, value, path).is_ok() {
                    return Err(format!("{}: {value} matches what `not` excludes", shown(path)));
                }
            }
            other => return Err(format!("{}: the validator does not know the keyword `{other}`", shown(path))),
        }
    }
    Ok(())
}

fn shown(path: &str) -> &str {
    if path.is_empty() {
        "/"
    } else {
        path
    }
}

fn resolve<'a>(root: &'a Value, reference: &Value) -> Result<&'a Value, String> {
    let reference = reference.as_str().ok_or("$ref is not a string")?;
    let name = reference.strip_prefix("#/$defs/").ok_or_else(|| format!("$ref {reference} does not point into $defs"))?;
    root.get("$defs").and_then(|defs| defs.get(name)).ok_or_else(|| format!("$ref {reference} has no definition"))
}

fn check_type(rule: &Value, value: &Value, path: &str) -> Result<(), String> {
    let names: Vec<&str> = match rule {
        Value::String(name) => vec![name.as_str()],
        Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
        _ => return Err("type is neither a string nor an array".to_string()),
    };
    if names.iter().any(|name| is_type(name, value)) {
        Ok(())
    } else {
        Err(format!("{}: {value} is not of type {rule}", shown(path)))
    }
}

fn is_type(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0),
        _ => false,
    }
}

fn check_bound(keyword: &str, rule: &Value, value: &Value, path: &str) -> Result<(), String> {
    let (Some(bound), Some(number)) = (rule.as_f64(), value.as_f64()) else {
        return Ok(());
    };
    let fine = match keyword {
        "minimum" => number >= bound,
        "maximum" => number <= bound,
        "exclusiveMinimum" => number > bound,
        _ => number < bound,
    };
    if fine {
        Ok(())
    } else {
        Err(format!("{}: {value} is outside {keyword} {rule}", shown(path)))
    }
}

/// A schema object, for the tests below and of callers that build small schemas.
pub fn object(entries: &[(&str, Value)]) -> Value {
    Value::Object(entries.iter().map(|(key, value)| (key.to_string(), value.clone())).collect::<Map<String, Value>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fails(schema: Value, value: Value) -> String {
        match validate(&schema, &value) {
            Ok(()) => panic!("{value} passed {schema}"),
            Err(e) => e,
        }
    }

    #[test]
    fn test_type() {
        assert!(validate(&json!({"type": "integer"}), &json!(3)).is_ok());
        assert!(validate(&json!({"type": ["string", "null"]}), &json!("a")).is_ok());
        assert!(fails(json!({"type": "integer"}), json!("3")).contains("is not of type"));
    }

    #[test]
    fn test_enum_and_const() {
        assert!(validate(&json!({"enum": ["off", "soft"]}), &json!("soft")).is_ok());
        assert!(fails(json!({"enum": ["off", "soft"]}), json!("hard")).contains("is not one of"));
        assert!(validate(&json!({"const": "sql"}), &json!("sql")).is_ok());
        assert!(fails(json!({"const": "sql"}), json!("lua")).contains("is not"));
    }

    #[test]
    fn test_properties_and_required() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "integer"}}, "required": ["a"], "additionalProperties": false});
        assert!(validate(&schema, &json!({"a": 1})).is_ok());
        assert!(fails(schema.clone(), json!({"a": "x"})).starts_with("/a:"));
        assert!(fails(schema.clone(), json!({})).contains("missing property `a`"));
        assert!(fails(schema, json!({"a": 1, "b": 2})).contains("unknown property `b`"));
        let map = json!({"type": "object", "additionalProperties": {"type": "string"}});
        assert!(validate(&map, &json!({"x": "y"})).is_ok());
        assert!(fails(map, json!({"x": 1})).starts_with("/x:"));
    }

    #[test]
    fn test_items_and_bounds() {
        let schema = json!({"type": "array", "items": {"type": "integer", "minimum": 1, "maximum": 3}, "minItems": 1});
        assert!(validate(&schema, &json!([1, 3])).is_ok());
        assert!(fails(schema.clone(), json!([0])).starts_with("/0:"));
        assert!(fails(schema.clone(), json!([4])).contains("maximum"));
        assert!(fails(schema, json!([])).contains("fewer than 1"));
        assert!(fails(json!({"exclusiveMinimum": 0}), json!(0)).contains("exclusiveMinimum"));
        assert!(fails(json!({"exclusiveMaximum": 1}), json!(1)).contains("exclusiveMaximum"));
    }

    #[test]
    fn test_refs_and_combinations() {
        let schema = json!({"$ref": "#/$defs/A", "$defs": {"A": {"type": "string"}}});
        assert!(validate(&schema, &json!("x")).is_ok());
        assert!(fails(schema, json!(1)).contains("is not of type"));
        assert!(fails(json!({"$ref": "#/$defs/B", "$defs": {}}), json!(1)).contains("no definition"));
        let any = json!({"anyOf": [{"type": "string"}, {"type": "array"}]});
        assert!(validate(&any, &json!([])).is_ok());
        assert!(fails(any, json!(1)).contains("none of anyOf"));
        let one = json!({"oneOf": [{"const": "a"}, {"type": "string"}]});
        assert!(validate(&one, &json!("b")).is_ok());
        assert!(fails(one, json!("a")).contains("matches 2 of oneOf"));
        let all = json!({"allOf": [{"type": "integer"}, {"minimum": 2}]});
        assert!(validate(&all, &json!(2)).is_ok());
        assert!(fails(all, json!(1)).contains("minimum"));
        assert!(fails(json!({"not": {"type": "string"}}), json!("x")).contains("not"));
        assert!(validate(&json!(true), &json!(1)).is_ok());
        assert!(fails(json!(false), json!(1)).contains("not allowed"));
    }

    #[test]
    fn test_unknown_keyword_fails() {
        assert!(fails(json!({"patternProperties": {}}), json!({})).contains("does not know the keyword `patternProperties`"));
        assert!(validate(&object(&[("type", json!("object")), ("description", json!("x"))]), &json!({})).is_ok());
    }

    #[test]
    fn test_toml_text() {
        assert_eq!(toml_text("a = 1\n[b]\nc = [\"x\"]\n").ok(), Some(json!({"a": 1, "b": {"c": ["x"]}})));
    }
}
