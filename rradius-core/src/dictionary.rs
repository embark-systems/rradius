use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Identifies an attribute by vendor (0 for standard attributes) and number path.
/// A plain attribute has a single-element path; TLV children append their number
/// to the parent's path (e.g. `[20, 1]`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttributeKey {
    pub vendor: u32,
    pub number: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    pub data_type: String,
    pub vendor: u32,
    pub number: Vec<u32>,
    /// Trailing option field (e.g. `encrypt=1`, `has_tag`), kept unparsed.
    pub options: Option<String>,
}

/// How a vendor lays out its attributes in Vendor-Specific: `format=` in the dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorFormat {
    pub type_size: usize,
    pub length_size: usize,
    /// An octet whose top bit says the value goes on in the next Vendor-Specific.
    pub continuation: bool,
}

impl Default for VendorFormat {
    fn default() -> Self {
        Self { type_size: 1, length_size: 1, continuation: false }
    }
}

#[derive(Debug)]
pub struct DictionaryError(String);

impl fmt::Display for DictionaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DictionaryError {}

/// Parser state that is scoped to a single dictionary file.
struct FileState {
    vendor: u32,
    tlv_stack: Vec<Vec<u32>>,
    /// The `evs` attribute this block's attributes go under, if any.
    number_prefix: Vec<u32>,
}

pub struct Dictionary {
    pub attributes: HashMap<AttributeKey, Attribute>,
    names: HashMap<String, AttributeKey>,
    vendors: HashMap<String, u32>,
    values: HashMap<String, HashMap<u64, String>>,
    vendor_formats: HashMap<u32, VendorFormat>,
}

impl Attribute {
    /// The trailing options, which FreeRADIUS's files separate by commas and others by spaces.
    pub fn options(&self) -> impl Iterator<Item = &str> {
        self.options.as_deref().unwrap_or_default().split([',', ' ']).filter(|option| !option.is_empty())
    }
}

impl Dictionary {
    pub fn new() -> Self {
        Self {
            attributes: HashMap::new(),
            names: HashMap::new(),
            vendors: HashMap::new(),
            values: HashMap::new(),
            vendor_formats: HashMap::new(),
        }
    }

    pub fn add_attribute(&mut self, name: &str, data_type: &str, vendor: u32, number: Vec<u32>, options: Option<String>) {
        let key = AttributeKey { vendor, number: number.clone() };
        self.names.insert(name.to_string(), key.clone());
        self.attributes.insert(key, Attribute {
            name: name.to_string(),
            data_type: data_type.to_string(),
            vendor,
            number,
            options,
        });
    }

    pub fn get_name(&self, vendor: u32, number: &[u32]) -> Option<&str> {
        let key = AttributeKey { vendor, number: number.to_vec() };
        self.attributes.get(&key).map(|a| a.name.as_str())
    }

    pub fn get_by_name(&self, name: &str) -> Option<&Attribute> {
        self.names.get(name).and_then(|key| self.attributes.get(key))
    }

    /// The attribute of this number, which need not have a name to be looked for by.
    pub fn get(&self, vendor: u32, number: &[u32]) -> Option<&Attribute> {
        self.attributes.get(&AttributeKey { vendor, number: number.to_vec() })
    }

    pub fn vendor_format(&self, vendor: u32) -> VendorFormat {
        self.vendor_formats.get(&vendor).copied().unwrap_or_default()
    }

    pub fn vendor_id(&self, name: &str) -> Option<u32> {
        self.vendors.get(name).copied()
    }

    pub fn value_name(&self, attribute: &str, value: u64) -> Option<&str> {
        self.values.get(attribute)?.get(&value).map(|v| v.as_str())
    }

    pub fn value_number(&self, attribute: &str, name: &str) -> Option<u64> {
        self.values.get(attribute)?.iter().find(|(_, n)| n.as_str() == name).map(|(v, _)| *v)
    }

    pub fn load_from_file<P: AsRef<Path>>(&mut self, path: P) -> Result<(), DictionaryError> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .map_err(|e| DictionaryError(format!("{}: cannot read dictionary: {}", path.display(), e)))?;
        self.load_content(path, &content, &mut Vec::new())
    }

    /// Loads dictionary text as if it were read from `path`, which `$INCLUDE` lines are
    /// relative to.
    pub fn load_from_str<P: AsRef<Path>>(&mut self, path: P, content: &str) -> Result<(), DictionaryError> {
        self.load_content(path.as_ref(), content, &mut Vec::new())
    }

    fn load_content(&mut self, path: &Path, content: &str, loading: &mut Vec<PathBuf>) -> Result<(), DictionaryError> {
        loading.push(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
        let mut state = FileState { vendor: 0, tlv_stack: Vec::new(), number_prefix: Vec::new() };

        for (index, raw_line) in content.lines().enumerate() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            self.parse_line(path, line, &mut state, loading)
                .map_err(|msg| DictionaryError(format!("{}:{}: {}", path.display(), index + 1, msg)))?;
        }

        loading.pop();
        Ok(())
    }

    fn parse_line(&mut self, path: &Path, line: &str, state: &mut FileState, loading: &mut Vec<PathBuf>) -> Result<(), String> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts[0] {
            "ATTRIBUTE" => {
                if parts.len() < 4 {
                    return Err(format!("ATTRIBUTE needs a name, number and type: {}", line));
                }
                let mut number = state.tlv_stack.last().cloned().unwrap_or_else(|| state.number_prefix.clone());
                number.extend(parse_number_path(parts[2])?);
                for option in parts[4..].iter().flat_map(|part| part.split(',')) {
                    // An attribute marked for encryption must not go out in the clear for a typo.
                    if option.starts_with("encrypt") && !matches!(option, "encrypt=1" | "encrypt=2" | "encrypt=3") {
                        return Err(format!("unknown encryption '{}' in: {}", option, line));
                    }
                }
                let options = (parts.len() > 4).then(|| parts[4..].join(" "));
                self.add_attribute(parts[1], parts[3], state.vendor, number, options);
            }
            "VALUE" => {
                if parts.len() < 4 {
                    return Err(format!("VALUE needs an attribute, name and number: {}", line));
                }
                // Values for undefined attributes are skipped, not treated as errors.
                if self.names.contains_key(parts[1]) {
                    let value = parse_u64(parts[3])?;
                    self.values.entry(parts[1].to_string()).or_default().insert(value, parts[2].to_string());
                }
            }
            "VENDOR" => {
                if parts.len() < 3 {
                    return Err(format!("VENDOR needs a name and number: {}", line));
                }
                let id = parse_u32(parts[2])?;
                if parts.len() > 4 {
                    return Err(format!("too many fields for VENDOR: {}", line));
                }
                self.vendors.insert(parts[1].to_string(), id);
                if let Some(format) = parts.get(3) {
                    self.vendor_formats.insert(id, parse_vendor_format(format)?);
                }
            }
            "BEGIN-VENDOR" => {
                let name = parts.get(1).ok_or("BEGIN-VENDOR needs a vendor name")?;
                if parts.len() > 3 {
                    return Err(format!("too many fields for BEGIN-VENDOR: {}", line));
                }
                state.vendor = self.vendor_id(name).ok_or_else(|| format!("unknown vendor '{}'", name))?;
                if let Some(option) = parts.get(2) {
                    let parent = option.strip_prefix("format=").ok_or_else(|| format!("unsupported option '{}'", option))?;
                    let attribute = self.get_by_name(parent).filter(|attribute| attribute.data_type == "evs");
                    state.number_prefix = attribute.ok_or_else(|| format!("'{}' is not an attribute of type evs", parent))?.number.clone();
                }
            }
            "END-VENDOR" => {
                state.vendor = 0;
                state.number_prefix.clear();
            }
            "BEGIN-TLV" => {
                let name = parts.get(1).ok_or("BEGIN-TLV needs an attribute name")?;
                let parent = self.get_by_name(name).ok_or_else(|| format!("unknown TLV attribute '{}'", name))?;
                state.tlv_stack.push(parent.number.clone());
            }
            "END-TLV" => {
                state.tlv_stack.pop().ok_or("END-TLV without matching BEGIN-TLV")?;
            }
            "$INCLUDE" | "include" => {
                let target = parts.get(1).ok_or("include needs a file name")?;
                let include_path = match Path::new(target) {
                    p if p.is_absolute() => p.to_path_buf(),
                    p => path.parent().unwrap_or(Path::new("")).join(p),
                };
                let canonical = include_path.canonicalize()
                    .map_err(|e| format!("cannot include '{}': {}", include_path.display(), e))?;
                if loading.contains(&canonical) {
                    return Err(format!("include cycle: '{}' is already being loaded", include_path.display()));
                }
                let content = fs::read_to_string(&include_path)
                    .map_err(|e| format!("cannot include '{}': {}", include_path.display(), e))?;
                self.load_content(&include_path, &content, loading).map_err(|e| e.to_string())?;
            }
            other => return Err(format!("unsupported directive '{}'", other)),
        }
        Ok(())
    }
}

/// Parses a decimal or `0x` hexadecimal unsigned number.
fn parse_u64(text: &str) -> Result<u64, String> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse::<u64>(),
    };
    parsed.map_err(|_| format!("invalid number '{}'", text))
}

fn parse_u32(text: &str) -> Result<u32, String> {
    u32::try_from(parse_u64(text)?).map_err(|_| format!("number out of range '{}'", text))
}

/// Parses `format=<type size>,<length size>[,c]`.
fn parse_vendor_format(option: &str) -> Result<VendorFormat, String> {
    let invalid = || format!("invalid vendor format '{}'", option);
    let text = option.strip_prefix("format=").ok_or_else(invalid)?;
    let parts: Vec<&str> = text.split(',').collect();
    let (sizes, continuation) = match parts.as_slice() {
        [type_size, length_size] => ((*type_size, *length_size), false),
        [type_size, length_size, "c"] => ((*type_size, *length_size), true),
        _ => return Err(invalid()),
    };
    let format = match sizes {
        ("1", "0") => VendorFormat { type_size: 1, length_size: 0, continuation },
        ("1", "1") => VendorFormat { type_size: 1, length_size: 1, continuation },
        ("1", "2") => VendorFormat { type_size: 1, length_size: 2, continuation },
        ("2", "0") => VendorFormat { type_size: 2, length_size: 0, continuation },
        ("2", "1") => VendorFormat { type_size: 2, length_size: 1, continuation },
        ("2", "2") => VendorFormat { type_size: 2, length_size: 2, continuation },
        ("4", "0") => VendorFormat { type_size: 4, length_size: 0, continuation },
        ("4", "1") => VendorFormat { type_size: 4, length_size: 1, continuation },
        ("4", "2") => VendorFormat { type_size: 4, length_size: 2, continuation },
        _ => return Err(invalid()),
    };
    if continuation && sizes != ("1", "1") {
        return Err(invalid());
    }
    Ok(format)
}

/// Parses an attribute number such as `1`, `0x1A` or `59.1` into its number path.
fn parse_number_path(text: &str) -> Result<Vec<u32>, String> {
    text.split('.').map(parse_u32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_dict(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        path
    }

    fn load_lines(lines: &[&str]) -> Dictionary {
        let dir = tempdir().unwrap();
        let path = write_dict(dir.path(), "dictionary", lines);
        let mut dict = Dictionary::new();
        dict.load_from_file(&path).unwrap();
        dict
    }

    #[test]
    fn test_vendor_block() {
        let dict = load_lines(&[
            "ATTRIBUTE User-Name 1 string",
            "VENDOR 3GPP 10415",
            "BEGIN-VENDOR 3GPP",
            "ATTRIBUTE 3GPP-IMSI 1 string",
            "END-VENDOR 3GPP",
        ]);

        assert_eq!(dict.get_name(0, &[1]), Some("User-Name"));
        assert_eq!(dict.get_name(10415, &[1]), Some("3GPP-IMSI"));
        assert_eq!(dict.vendor_id("3GPP"), Some(10415));
        assert_eq!(dict.get_by_name("3GPP-IMSI").unwrap().vendor, 10415);
        assert_eq!(dict.get_by_name("User-Name").unwrap().vendor, 0);
    }

    #[test]
    fn test_vendor_format() {
        let dict = load_lines(&["VENDOR WiMAX 24757 format=1,1,c", "VENDOR Cisco 9", "VENDOR USR 429 format=4,0", "VENDOR Starent 8164 format=2,2"]);
        assert_eq!(dict.vendor_format(24757), VendorFormat { type_size: 1, length_size: 1, continuation: true });
        assert_eq!(dict.vendor_format(9), VendorFormat::default());
        assert_eq!(dict.vendor_format(429), VendorFormat { type_size: 4, length_size: 0, continuation: false });
        assert_eq!(dict.vendor_format(8164), VendorFormat { type_size: 2, length_size: 2, continuation: false });
        assert_eq!(dict.vendor_format(12345), VendorFormat::default(), "one nobody defined");
    }

    #[test]
    fn test_vendor_format_errors() {
        for line in ["VENDOR Bad 1 format=3,1", "VENDOR Bad 1 format=1,3", "VENDOR Bad 1 format=1", "VENDOR Bad 1 format=2,1,c", "VENDOR Bad 1 format=1,1,x", "VENDOR Bad 1 format=a,b", "VENDOR Bad 1 what=1,1", "VENDOR Bad 1 format=1,1,c,c", "VENDOR Bad 1 format=1,1 junk"] {
            let error = Dictionary::new().load_from_str("/nonexistent/dictionary", line).map(|_| ()).unwrap_err().to_string();
            assert!(error.contains("/nonexistent/dictionary:1"), "{line}: {error}");
        }
    }

    #[test]
    fn test_block_under_evs() {
        let dict = load_lines(&[
            "ATTRIBUTE Extended-Attribute-5 245 long-extended",
            "ATTRIBUTE Extended-Vendor-Specific-5 245.26 evs",
            "VENDOR FreeRADIUS 11344",
            "BEGIN-VENDOR FreeRADIUS",
            "ATTRIBUTE Ordinary 1 string",
            "END-VENDOR FreeRADIUS",
            "BEGIN-VENDOR FreeRADIUS format=Extended-Vendor-Specific-5",
            "ATTRIBUTE Example 1 octets",
            "ATTRIBUTE Example-Tlv 2 tlv",
            "BEGIN-TLV Example-Tlv",
            "ATTRIBUTE Example-Inner 1 byte",
            "END-TLV Example-Tlv",
            "END-VENDOR FreeRADIUS",
            "ATTRIBUTE After 9 string",
        ]);
        let example = dict.get_by_name("Example").unwrap();
        assert_eq!((example.vendor, example.number.as_slice()), (11344, &[245, 26, 1][..]));
        assert_eq!(dict.get_by_name("Example-Inner").unwrap().number, [245, 26, 2, 1]);
        assert_eq!(dict.get_by_name("Ordinary").unwrap().number, [1], "only in that block");
        assert_eq!(dict.get_by_name("After").unwrap().number, [9]);
    }

    #[test]
    fn test_block_under_something_else() {
        for line in ["BEGIN-VENDOR Plain format=User-Name", "BEGIN-VENDOR Plain format=Nobody", "BEGIN-VENDOR Plain what=User-Name", "BEGIN-VENDOR Plain format=Evs junk"] {
            let text = format!("ATTRIBUTE User-Name 1 string\nATTRIBUTE Evs 241.26 evs\nVENDOR Plain 9\n{line}\n");
            let error = Dictionary::new().load_from_str("/nonexistent/dictionary", &text).map(|_| ()).unwrap_err().to_string();
            assert!(error.contains("/nonexistent/dictionary:4"), "{line}: {error}");
        }
    }

    #[test]
    fn test_standard_attribute() {
        let dict = load_lines(&["ATTRIBUTE User-Name 1 string"]);
        let attr = dict.get_by_name("User-Name").unwrap();
        assert_eq!(dict.get_name(0, &[1]), Some("User-Name"));
        assert_eq!(attr.data_type, "string");
        assert_eq!(attr.options, None);
    }

    #[test]
    fn test_named_values() {
        let dict = load_lines(&[
            "ATTRIBUTE Service-Type 6 integer",
            "VALUE Service-Type Login-User 1",
            "VALUE Service-Type Framed-User 0x2  # trailing comment",
        ]);
        assert_eq!(dict.value_name("Service-Type", 1), Some("Login-User"));
        assert_eq!(dict.value_name("Service-Type", 2), Some("Framed-User"));
    }

    #[test]
    fn test_value_for_undefined_attribute_is_skipped() {
        let dict = load_lines(&["VALUE No-Such-Attr Foo 1"]);
        assert_eq!(dict.value_name("No-Such-Attr", 1), None);
    }

    #[test]
    fn test_hexadecimal_attribute_number() {
        let dict = load_lines(&["ATTRIBUTE Hex-Attr 0x1A integer"]);
        assert_eq!(dict.get_name(0, &[26]), Some("Hex-Attr"));
    }

    #[test]
    fn test_unknown_encrypt_option_is_an_error() {
        for options in ["encrypt=9", "encrypt=", "encrypt", "encrypt=22", "has_tag,encrypt=0", "has_tag encrypt=x", "encrypted=2"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("dictionary");
            fs::write(&path, format!("# first\nATTRIBUTE X 200 string {options}\n")).unwrap();
            let error = Dictionary::new().load_from_file(&path).unwrap_err().to_string();
            assert!(error.contains("dictionary") && error.contains(":2") && error.contains("encrypt"), "{options}: {error}");
        }
        for options in ["encrypt=1", "encrypt=2", "encrypt=3", "has_tag encrypt=2", "has_tag,encrypt=2", "array", "has_tag"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("dictionary");
            fs::write(&path, format!("ATTRIBUTE X 200 string {options}\n")).unwrap();
            Dictionary::new().load_from_file(&path).unwrap_or_else(|e| panic!("{options}: {e}"));
        }
    }

    #[test]
    fn test_attribute_options_preserved() {
        let dict = load_lines(&[
            "ATTRIBUTE User-Password 2 string encrypt=1",
            "ATTRIBUTE Tunnel-Type 64 integer has_tag",
        ]);
        assert_eq!(dict.get_by_name("User-Password").unwrap().options.as_deref(), Some("encrypt=1"));
        assert_eq!(dict.get_by_name("Tunnel-Type").unwrap().options.as_deref(), Some("has_tag"));
    }

    #[test]
    fn test_include_relative_to_including_file() {
        let dir = tempdir().unwrap();
        write_dict(dir.path(), "dictionary.sub", &["ATTRIBUTE Sub-Attr 7 string"]);
        let main = write_dict(dir.path(), "main", &["$INCLUDE dictionary.sub", "include dictionary.sub"]);
        assert_ne!(std::env::current_dir().unwrap(), dir.path());

        let mut dict = Dictionary::new();
        dict.load_from_file(&main).unwrap();
        assert_eq!(dict.get_name(0, &[7]), Some("Sub-Attr"));
    }

    fn share_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../share")
    }

    fn load_error(lines: &[&str]) -> String {
        let dir = tempdir().unwrap();
        let path = write_dict(dir.path(), "dictionary", lines);
        let mut dict = Dictionary::new();
        dict.load_from_file(&path).unwrap_err().to_string()
    }

    #[test]
    fn test_tlv_block() {
        let dict = load_lines(&[
            "ATTRIBUTE Parent-Attr 5 tlv",
            "BEGIN-TLV Parent-Attr",
            "ATTRIBUTE Child-Attr 2 byte",
            "ATTRIBUTE Nested-Parent 3 tlv",
            "BEGIN-TLV Nested-Parent",
            "ATTRIBUTE Grandchild-Attr 1 byte",
            "END-TLV Nested-Parent",
            "END-TLV Parent-Attr",
            "ATTRIBUTE After-Tlv 6 string",
        ]);
        assert_eq!(dict.get_name(0, &[5, 2]), Some("Child-Attr"));
        assert_eq!(dict.get_name(0, &[5, 3, 1]), Some("Grandchild-Attr"));
        assert_eq!(dict.get_name(0, &[6]), Some("After-Tlv"));
    }

    #[test]
    fn test_dotted_attribute_number() {
        let dict = load_lines(&["ATTRIBUTE Remote-IP-Address-Value 59.1 ipaddr"]);
        assert_eq!(dict.get_name(0, &[59, 1]), Some("Remote-IP-Address-Value"));
    }

    #[test]
    fn test_share_wimax_wichorus_tlv() {
        let mut dict = Dictionary::new();
        dict.load_from_file(share_dir().join("dictionary.wimax.wichorus")).unwrap();
        let hour = dict.get_by_name("WiMAX-Hour").unwrap();
        let parent = dict.get_by_name("WiMAX-Time-Of-Day-Time").unwrap();
        assert_eq!(parent.number, vec![20]);
        assert_eq!(hour.number, vec![20, 1]);
        assert_eq!(hour.vendor, parent.vendor);
    }

    #[test]
    fn test_unsupported_directive() {
        let err = load_error(&["ATTRIBUTE User-Name 1 string", "FROBNICATE x y"]);
        assert!(err.contains("dictionary:2:"), "unexpected error: {err}");
        assert!(err.contains("FROBNICATE"), "unexpected error: {err}");
    }

    #[test]
    fn test_unknown_vendor_name() {
        let err = load_error(&["BEGIN-VENDOR Nobody"]);
        assert!(err.contains("dictionary:1:"), "unexpected error: {err}");
        assert!(err.contains("Nobody"), "unexpected error: {err}");
    }

    #[test]
    fn test_missing_included_file() {
        let err = load_error(&["ATTRIBUTE User-Name 1 string", "$INCLUDE dictionary.missing"]);
        assert!(err.contains("dictionary:2:"), "unexpected error: {err}");
        assert!(err.contains("dictionary.missing"), "unexpected error: {err}");
    }

    #[test]
    fn test_include_cycle() {
        let dir = tempdir().unwrap();
        let a = write_dict(dir.path(), "dictionary.a", &["$INCLUDE dictionary.b"]);
        write_dict(dir.path(), "dictionary.b", &["ATTRIBUTE B-Attr 1 string", "$INCLUDE dictionary.a"]);

        let mut dict = Dictionary::new();
        let err = dict.load_from_file(&a).unwrap_err().to_string();
        assert!(err.contains("include cycle"), "unexpected error: {err}");
        assert!(err.contains("dictionary.b:2:"), "unexpected error: {err}");
        assert!(err.contains("dictionary.a"), "unexpected error: {err}");
    }

    #[test]
    fn test_load_bundled_share_dictionary() {
        let mut dict = Dictionary::new();
        dict.load_from_file(share_dir().join("dictionary")).unwrap();

        let user_name = dict.get_by_name("User-Name").unwrap();
        assert_eq!((user_name.vendor, user_name.number.clone()), (0, vec![1]));
        assert_eq!(dict.vendor_id("3GPP"), Some(10415));
        let imsi = dict.get_by_name("3GPP-IMSI").unwrap();
        assert_eq!((imsi.vendor, imsi.number.clone()), (10415, vec![1]));
        assert_eq!(dict.value_name("Service-Type", 1), Some("Login-User"));
    }
}
