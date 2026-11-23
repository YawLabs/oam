//! node 26's `URLPattern` natives, over denoland's `urlpattern` crate (the
//! WHATWG URLPattern parser).
//!
//! The split is the one the spec itself makes: the parser turns a pattern
//! into, per URL component, a pattern string, an ECMAScript regular
//! expression and the list of its group names; matching runs that regular
//! expression over the component of the input URL. oam does the parsing
//! here and the matching in JS (js/node_compat.js `URLPattern`) with V8's
//! own RegExp, the engine the spec's regular expressions are written for.
//!
//! Both ops take the input as JSON -- `{"string": "..."}` or `{"init":
//! {...}}`, the init already reduced to its string members by the JS side --
//! and answer JSON, or a status string the JS side turns into node's error.

use urlpattern::quirks::{self, StringOrInit, UrlPatternInit};

/// What `input` JSON names: a pattern / URL string or an init dictionary.
fn parse_input(json: &str) -> Option<StringOrInit<'static>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    if let Some(s) = value.get("string").and_then(|v| v.as_str()) {
        return Some(StringOrInit::String(s.to_string().into()));
    }
    let init: UrlPatternInit = serde_json::from_value(value.get("init")?.clone()).ok()?;
    Some(StringOrInit::Init(init))
}

/// Parse a pattern: the per-component pattern strings, regular expressions
/// and group names, and whether any group is a custom regular expression.
/// `None` when the pattern is not valid, which node reports as one error.
pub(crate) fn parse_pattern(
    input_json: &str,
    base_url: Option<&str>,
    ignore_case: bool,
) -> Option<String> {
    let input = parse_input(input_json)?;
    let init = quirks::process_construct_pattern_input(input, base_url).ok()?;
    // ECMAScript syntax: the expressions are compiled by V8 (with the
    // spec's `v` flag), not by the regex crate.
    let options = urlpattern::UrlPatternOptions {
        ignore_case,
        regex_syntax: urlpattern::RegexSyntax::EcmaScript,
    };
    let pattern = quirks::parse_pattern::<quirks::EcmaRegexp>(init, options).ok()?;
    serde_json::to_string(&pattern).ok()
}

/// How a match input resolved: its eight component values, no match at
/// all (an input that is not a URL), or an init given with a base URL.
pub(crate) enum MatchInput {
    Values(String),
    NoMatch,
    InitWithBase,
}

pub(crate) fn match_input(input_json: &str, base_url: Option<&str>) -> MatchInput {
    let Some(input) = parse_input(input_json) else {
        return MatchInput::NoMatch;
    };
    match quirks::process_match_input(input, base_url) {
        Err(_) => MatchInput::InitWithBase,
        Ok(None) => MatchInput::NoMatch,
        Ok(Some((input, _))) => match quirks::parse_match_input(input) {
            None => MatchInput::NoMatch,
            Some(values) => serde_json::to_string(&values)
                .map(MatchInput::Values)
                .unwrap_or(MatchInput::NoMatch),
        },
    }
}

/// urlPatternParse(inputJson, baseURL?, ignoreCase) -> JSON string, or null
/// for an invalid pattern.
pub(crate) fn op_url_pattern_parse(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(input) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "urlPatternParse: input required");
        return;
    };
    let base = crate::node_ops::arg_string(scope, &args, 1);
    let ignore_case = args.get(2).boolean_value(scope);
    match parse_pattern(&input, base.as_deref(), ignore_case) {
        Some(json) => {
            let s = v8::String::new(scope, &json).unwrap();
            rv.set(s.into());
        }
        None => rv.set_null(),
    }
}

/// urlPatternMatchInput(inputJson, baseURL?) -> JSON string of the component
/// values, null when the input is no URL, or the string "init-with-base".
pub(crate) fn op_url_pattern_match_input(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(input) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "urlPatternMatchInput: input required");
        return;
    };
    let base = crate::node_ops::arg_string(scope, &args, 1);
    match match_input(&input, base.as_deref()) {
        MatchInput::Values(json) => {
            let s = v8::String::new(scope, &json).unwrap();
            rv.set(s.into());
        }
        MatchInput::NoMatch => rv.set_null(),
        MatchInput::InitWithBase => {
            let s = v8::String::new(scope, "init-with-base").unwrap();
            rv.set(s.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_components_and_group_names() {
        let json = parse_pattern(r#"{"init":{"pathname":"/books/:id"}}"#, None, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["pathname"]["patternString"], "/books/:id");
        assert_eq!(v["pathname"]["groupNameList"][0], "id");
        assert_eq!(v["protocol"]["patternString"], "*");
        assert_eq!(v["hasRegexpGroups"], false);
    }

    #[test]
    fn relative_pattern_needs_a_base() {
        assert!(parse_pattern(r#"{"string":"/foo"}"#, None, false).is_none());
        assert!(
            parse_pattern(r#"{"string":"/foo"}"#, Some("https://example.com"), false).is_some()
        );
        assert!(
            parse_pattern(
                r#"{"init":{"pathname":"/a"}}"#,
                Some("https://x.com"),
                false
            )
            .is_none()
        );
    }

    #[test]
    fn match_input_resolves_urls() {
        match match_input(r#"{"string":"https://example.com/books/1?q#h"}"#, None) {
            MatchInput::Values(json) => {
                let v: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(v["hostname"], "example.com");
                assert_eq!(v["pathname"], "/books/1");
                assert_eq!(v["search"], "q");
                assert_eq!(v["hash"], "h");
            }
            _ => panic!("expected values"),
        }
        assert!(matches!(
            match_input(r#"{"string":"not a url"}"#, None),
            MatchInput::NoMatch
        ));
        assert!(matches!(
            match_input(r#"{"init":{"pathname":"/a"}}"#, Some("https://x.com")),
            MatchInput::InitWithBase
        ));
    }
}
