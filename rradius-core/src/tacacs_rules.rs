//! TACACS+ authorization groups: rules written one per line, such as
//! `permit service=shell cmd\* {priv-lvl=15}`, that decide an Authorization REQUEST
//! without a script.

use regex::bytes::{Regex, RegexBuilder};

/// The most AV pairs one reply holds, and the longest one: each travels with a 1-octet count
/// or length (RFC 8907 section 6.2).
const MAX_REPLY_PAIRS: usize = 255;
const MAX_PAIR_LEN: usize = 255;

#[derive(Debug, Clone)]
enum Action {
    /// PASS_ADD with these AV pairs.
    Permit(Vec<Vec<u8>>),
    Deny,
}

#[derive(Debug, Clone)]
struct Rule {
    /// Each one has to match a whole AV pair of the request.
    patterns: Vec<Regex>,
    action: Action,
}

impl Rule {
    fn holds(&self, args: &[&[u8]]) -> bool {
        self.patterns.iter().all(|pattern| args.iter().any(|arg| pattern.is_match(arg)))
    }
}

/// The rules of one group, in the order they were written.
#[derive(Debug, Clone, Default)]
pub struct AuthorizationRules {
    rules: Vec<Rule>,
    /// As written, which is what two groups are compared by.
    source: Vec<String>,
}

impl PartialEq for AuthorizationRules {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl AuthorizationRules {
    /// Compiles `rules`; an error names the rule by its position, counted from 1.
    pub fn parse(source: &[String]) -> Result<Self, String> {
        let rules = source
            .iter()
            .enumerate()
            .map(|(index, rule)| parse_rule(rule).map_err(|e| format!("rule {}: {}", index + 1, e)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { rules, source: source.to_vec() })
    }

    /// The AV pairs of the first rule that holds for `args` when it permits; `None` when it
    /// denies or no rule holds.
    pub fn decide(&self, args: &[&[u8]]) -> Option<&[Vec<u8>]> {
        match &self.rules.iter().find(|rule| rule.holds(args))?.action {
            Action::Permit(reply) => Some(reply),
            Action::Deny => None,
        }
    }
}

fn parse_rule(rule: &str) -> Result<Rule, String> {
    let rule = rule.trim_matches(|c: char| c.is_ascii_whitespace());
    // The reply starts at the first token beginning with `{`; a `{` inside a pattern, as in
    // `a{2}`, stays part of it.
    let mut reply_at = None;
    let mut tokens = Vec::new();
    let mut rest = rule;
    while let Some(start) = rest.find(|c: char| !c.is_ascii_whitespace()) {
        let offset = rule.len() - rest.len() + start;
        let token_rest = &rule[offset..];
        if token_rest.starts_with('{') {
            reply_at = Some(offset);
            break;
        }
        let end = token_rest.find(|c: char| c.is_ascii_whitespace()).unwrap_or(token_rest.len());
        tokens.push(&token_rest[..end]);
        rest = &token_rest[end..];
    }
    let Some((&action, patterns)) = tokens.split_first() else {
        return Err("is empty; it has to start with permit or deny".to_string());
    };
    let reply = match reply_at {
        Some(at) => Some(parse_reply(&rule[at..])?),
        None => None,
    };
    let action = match (action, reply) {
        ("permit", reply) => Action::Permit(reply.unwrap_or_default()),
        ("deny", None) => Action::Deny,
        ("deny", Some(_)) => return Err("deny cannot have a reply: FAIL carries no AV pairs".to_string()),
        (other, _) => return Err(format!("starts with '{}'; it has to start with permit or deny", other)),
    };
    let patterns = patterns
        .iter()
        .map(|pattern| {
            // Bytes, not characters: an AV pair need not be UTF-8, and `.` should still match it.
            RegexBuilder::new(&format!("^(?:{})$", pattern))
                .unicode(false)
                .dot_matches_new_line(true)
                .build()
                .map_err(|e| format!("pattern '{}' is not a valid regular expression: {}", pattern, e))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Rule { patterns, action })
}

/// Splits the text after a reply's `{` into its AV pairs at whitespace outside double quotes,
/// up to the first `}` outside them, and returns the pairs and what follows that `}`. The
/// quotes stay in the pairs.
fn split_reply(inner: &str) -> Result<(Vec<&str>, &str), String> {
    let mut pairs = Vec::new();
    let mut start = None;
    let mut quoted = false;
    for (at, c) in inner.char_indices() {
        match c {
            '"' => {
                quoted = !quoted;
                start.get_or_insert(at);
            }
            _ if quoted => {}
            '}' => {
                pairs.extend(start.map(|start| &inner[start..at]));
                return Ok((pairs, &inner[at + 1..]));
            }
            c if c.is_ascii_whitespace() => pairs.extend(start.take().map(|start| &inner[start..at])),
            _ => {
                start.get_or_insert(at);
            }
        }
    }
    Err(if quoted { "the reply's double quotes are not paired".to_string() } else { "the reply has no closing }".to_string() })
}

/// `{name=value name*value ...}`, which has to end the rule. Double quotes keep whitespace
/// and `}` inside a value, and are sent as written.
fn parse_reply(text: &str) -> Result<Vec<Vec<u8>>, String> {
    let Some(inner) = text.strip_prefix('{') else {
        return Err("the reply has to start with {".to_string());
    };
    let (pairs, after) = split_reply(inner)?;
    if !after.is_empty() {
        return Err(format!("text after the reply's closing }}: '{}'", after));
    }
    let pairs: Vec<Vec<u8>> = pairs
        .into_iter()
        .map(|pair| {
            match pair.find(['=', '*']) {
                None => Err(format!("reply AV pair '{}' has no = or *", pair)),
                Some(0) => Err(format!("reply AV pair '{}' has no name", pair)),
                Some(_) if pair.len() > MAX_PAIR_LEN => Err(format!("reply AV pair '{}' is longer than {} octets", pair, MAX_PAIR_LEN)),
                Some(_) => Ok(pair.as_bytes().to_vec()),
            }
        })
        .collect::<Result<_, _>>()?;
    if pairs.len() > MAX_REPLY_PAIRS {
        return Err(format!("the reply has {} AV pairs, more than the {} one reply holds", pairs.len(), MAX_REPLY_PAIRS));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(rules: &[&str]) -> Result<AuthorizationRules, String> {
        AuthorizationRules::parse(&rules.iter().map(|rule| rule.to_string()).collect::<Vec<_>>())
    }

    fn decide(group: &AuthorizationRules, args: &[&str]) -> Option<Vec<String>> {
        let args: Vec<&[u8]> = args.iter().map(|arg| arg.as_bytes()).collect();
        group.decide(&args).map(|reply| reply.iter().map(|pair| String::from_utf8_lossy(pair).into_owned()).collect())
    }

    #[test]
    fn test_rule_decisions() -> Result<(), String> {
        let shell = r"permit service=shell cmd\* {priv-lvl=15}";
        let show = ["deny cmd=show cmd-arg=running-config", "permit service=shell cmd=show"];
        // Rules, the request's AV pairs, and the reply when permitted.
        type Case<'a> = (Vec<&'a str>, Vec<&'a str>, Option<Vec<&'a str>>);
        let cases: Vec<Case> = vec![
            (vec![shell], vec!["service=shell", "cmd*"], Some(vec!["priv-lvl=15"])),
            (vec![shell], vec!["service=shell", "cmd=show", "cmd-arg=version"], None),
            (vec!["permit service=shell cmd=show"], vec!["service=shell", "cmd=showx"], None),
            (show.to_vec(), vec!["service=shell", "cmd=show", "cmd-arg=running-config"], None),
            (show.to_vec(), vec!["service=shell", "cmd=show", "cmd-arg=version"], Some(vec![])),
            (vec!["permit {priv-lvl=1 timeout=30}"], vec!["service=ppp", "protocol=ip"], Some(vec!["priv-lvl=1", "timeout=30"])),
            (vec![], vec!["service=shell", "cmd*"], None),
            (vec!["permit service=netscreen {vsys=root privilege=root}"], vec!["service=netscreen"], Some(vec!["vsys=root", "privilege=root"])),
            (vec!["permit service=netscreen {vsys=root privilege=root}"], vec!["service=shell"], None),
        ];
        for (rules, args, expected) in cases {
            let expected = expected.map(|reply| reply.iter().map(|pair| pair.to_string()).collect::<Vec<_>>());
            assert_eq!(decide(&group(&rules)?, &args), expected, "{rules:?} {args:?}");
        }
        Ok(())
    }

    #[test]
    fn test_rejected_rules() -> Result<(), String> {
        let cases = [
            ("allow service=shell", "permit or deny"),
            ("permit cmd=(show", "regular expression"),
            ("permit service=shell {priv-lvl=15", "closing }"),
            ("permit service=shell {priv-lvl=15} x", "after the reply"),
            ("deny service=shell {priv-lvl=1}", "deny cannot have a reply"),
            ("permit {priv-lvl}", "no = or *"),
            ("permit {=15}", "no name"),
            ("", "is empty"),
        ];
        for (rule, message) in cases {
            let Err(e) = group(&["permit", rule]) else {
                return Err(format!("'{rule}' was accepted"));
            };
            assert!(e.starts_with("rule 2: "), "{e}");
            assert!(e.contains(message), "'{rule}': {e}");
        }
        Ok(())
    }

    #[test]
    fn test_reply_limits() {
        let long = format!("permit {{a={}}}", "x".repeat(MAX_PAIR_LEN - 2));
        assert!(group(&[&long]).is_ok());
        let longer = format!("permit {{a={}}}", "x".repeat(MAX_PAIR_LEN - 1));
        assert!(group(&[&longer]).is_err_and(|e| e.contains("longer than 255")));
        let many = format!("permit {{{}}}", vec!["a=1"; MAX_REPLY_PAIRS + 1].join(" "));
        assert!(group(&[&many]).is_err_and(|e| e.contains("256 AV pairs")));
    }

    #[test]
    fn test_brace_inside_a_pattern() -> Result<(), String> {
        let group = group(&["permit cmd-arg=a{2} {priv-lvl=2}"])?;
        assert_eq!(decide(&group, &["cmd-arg=aa"]), Some(vec!["priv-lvl=2".to_string()]));
        assert_eq!(decide(&group, &["cmd-arg=a"]), None);
        Ok(())
    }

    #[test]
    fn test_one_arg_can_match_several_patterns() -> Result<(), String> {
        let group = group(&["permit cmd=.* .*=show"])?;
        assert_eq!(decide(&group, &["cmd=show"]), Some(vec![]));
        Ok(())
    }

    #[test]
    fn test_args_that_are_not_utf8() -> Result<(), String> {
        let arg: &[u8] = b"cmd-arg=\xff\xff";
        assert!(group(&[r"permit cmd-arg=\xff+"])?.decide(&[arg]).is_some());
        assert!(group(&[r"permit cmd-arg=\xff+"])?.decide(&[b"cmd-arg=x".as_slice()]).is_none());
        assert!(group(&["permit cmd-arg=.*"])?.decide(&[arg]).is_some(), ". matches any octet");
        Ok(())
    }

    #[test]
    fn test_quoted_reply_values() -> Result<(), String> {
        let cases: [(&str, &[&str]); 6] = [
            (r#"permit {shell:roles="network-admin vdc-admin"}"#, &[r#"shell:roles="network-admin vdc-admin""#]),
            (r#"permit {a="x y" b=2}"#, &[r#"a="x y""#, "b=2"]),
            (r#"permit {a="x}y"}"#, &[r#"a="x}y""#]),
            (r#"permit {a="x"}"#, &[r#"a="x""#]),
            ("permit {priv-lvl=15}", &["priv-lvl=15"]),
            (r#"permit {"a b"=1}"#, &[r#""a b"=1"#]),
        ];
        for (rule, expected) in cases {
            let expected: Vec<String> = expected.iter().map(|pair| pair.to_string()).collect();
            assert_eq!(decide(&group(&[rule])?, &["service=shell"]), Some(expected), "{rule}");
        }
        for (rule, message) in [(r#"permit {a="x y}"#, "not paired"), (r#"permit {a="x y" b}"#, "'b' has no = or *"), (r#"permit {a="x" } x"#, "after the reply")] {
            let Err(e) = group(&[rule]) else {
                return Err(format!("'{rule}' was accepted"));
            };
            assert!(e.starts_with("rule 1: ") && e.contains(message), "'{rule}': {e}");
        }
        // The quotes are sent, so they count toward the 255 octets.
        let quoted = |len: usize| format!("permit {{a=\"{}\"}}", "x".repeat(len));
        assert!(group(&[&quoted(MAX_PAIR_LEN - 4)]).is_ok());
        assert!(group(&[&quoted(MAX_PAIR_LEN - 3)]).is_err_and(|e| e.contains("longer than 255")));
        // Patterns keep double quotes as plain characters.
        let plain = group(&[r#"permit a="b""#])?;
        assert_eq!(decide(&plain, &[r#"a="b""#]), Some(vec![]));
        assert_eq!(decide(&plain, &["a=b"]), None);
        Ok(())
    }

    #[test]
    fn test_optional_reply_pair_kept() -> Result<(), String> {
        let group = group(&["permit {timeout*30}"])?;
        assert_eq!(decide(&group, &["service=shell"]), Some(vec!["timeout*30".to_string()]));
        Ok(())
    }
}
