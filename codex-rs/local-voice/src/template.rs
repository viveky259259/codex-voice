//! `{name}` placeholder substitution for argv and JSON templates.

use serde_json::Value;

/// Replaces each `{name}` whose name is in `vars`, in one pass, so values
/// that themselves contain `{...}` are never expanded again. Unknown
/// placeholders are left as they are.
pub fn substitute(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let replaced = after.find('}').and_then(|close| {
            let name = &after[..close];
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value, close))
        });
        match replaced {
            Some((value, close)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Substitutes every element; each stays exactly one argv element.
pub fn substitute_argv(argv: &[String], vars: &[(&str, &str)]) -> Vec<String> {
    argv.iter().map(|arg| substitute(arg, vars)).collect()
}

/// Substitutes inside every string of a JSON value (keys untouched).
pub fn substitute_json(value: &Value, vars: &[(&str, &str)]) -> Value {
    match value {
        Value::String(text) => Value::String(substitute(text, vars)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute_json(item, vars))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), substitute_json(item, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub fn argv_uses(argv: &[String], name: &str) -> bool {
    let needle = format!("{{{name}}}");
    argv.iter().any(|arg| arg.contains(&needle))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[test]
    fn substitutes_once_and_keeps_unknown_placeholders() {
        let vars = [("text", "say {wav} $(rm -rf /)"), ("wav", "/tmp/a.wav")];
        assert_eq!(
            substitute("-o {wav} {text} {other}", &vars),
            "-o /tmp/a.wav say {wav} $(rm -rf /) {other}"
        );
        assert_eq!(substitute("{unterminated", &vars), "{unterminated");
        assert_eq!(substitute("{{text}}", &[("text", "x")]), "{x}");
    }

    #[test]
    fn json_strings_are_substituted() {
        let template = json!({"input_path": "{wav}", "n": 1, "list": ["{wav}"]});
        assert_eq!(
            substitute_json(&template, &[("wav", "/a \"b\".wav")]),
            json!({"input_path": "/a \"b\".wav", "n": 1, "list": ["/a \"b\".wav"]})
        );
    }
}
