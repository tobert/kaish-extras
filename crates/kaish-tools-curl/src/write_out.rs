//! `-w` / `--write-out`: the subset of curl's format string this build
//! supports, parsed before any request is made.
//!
//! Only the variables an agent types are here. An unknown `%{name}` is a
//! refusal rather than curl's warning-and-empty, because an empty value in a
//! script that branches on it reads as an answer.

use std::time::Duration;

use crate::error::CurlError;
use crate::model::Response;

/// The variables this build supports, in the spelling the refusal prints.
pub const SUPPORTED: &[&str] = &[
    "http_code",
    "response_code",
    "time_total",
    "size_download",
    "url_effective",
    "content_type",
    "redirect_url",
    "num_redirects",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Var {
    HttpCode,
    TimeTotal,
    SizeDownload,
    UrlEffective,
    ContentType,
    RedirectUrl,
    NumRedirects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Var(Var),
}

/// A parsed `-w` format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOut(Vec<Part>);

fn unsupported(what: &str) -> CurlError {
    CurlError::Usage(format!(
        "'-w' cannot use {what} (exit 2), and no request was sent. Supported variables: {}. \
         Supported escapes: \\n, \\r, \\t, and %% for a percent sign. \
         For anything else, use '--json' and read the fields with jq.",
        SUPPORTED
            .iter()
            .map(|n| format!("%{{{n}}}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Parse a `-w` format. The error is the refusal an agent reads.
pub fn parse(format: &str) -> Result<WriteOut, CurlError> {
    if format.starts_with('@') {
        return Err(CurlError::Usage(
            "'-w @file' is not supported (exit 2), and no request was sent. Pass the format \
             string itself: -w \"$(cat <path>)\""
                .into(),
        ));
    }
    let mut parts: Vec<Part> = Vec::new();
    let mut text = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some('n') => {
                    chars.next();
                    text.push('\n');
                }
                Some('r') => {
                    chars.next();
                    text.push('\r');
                }
                Some('t') => {
                    chars.next();
                    text.push('\t');
                }
                // curl prints any other backslash pair as typed, both
                // characters: `\\n` is a backslash, a backslash, and an `n`,
                // and `\%{http_code}` is not a variable. A backslash that
                // ends the text prints as itself.
                Some(_) => {
                    text.push('\\');
                    text.extend(chars.next());
                }
                None => text.push('\\'),
            },
            '%' => match chars.peek() {
                Some('%') => {
                    chars.next();
                    text.push('%');
                }
                Some('{') => {
                    chars.next();
                    let mut name = String::new();
                    let mut closed = false;
                    for n in chars.by_ref() {
                        if n == '}' {
                            closed = true;
                            break;
                        }
                        name.push(n);
                    }
                    if !closed {
                        return Err(unsupported(&format!("the unterminated '%{{{name}'")));
                    }
                    let var = match name.as_str() {
                        "http_code" | "response_code" => Var::HttpCode,
                        "time_total" => Var::TimeTotal,
                        "size_download" => Var::SizeDownload,
                        "url_effective" => Var::UrlEffective,
                        "content_type" => Var::ContentType,
                        "redirect_url" => Var::RedirectUrl,
                        "num_redirects" => Var::NumRedirects,
                        other => return Err(unsupported(&format!("'%{{{other}}}'"))),
                    };
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(Part::Var(var));
                }
                // `%header{name}` and the like: a word then a brace.
                Some(w) if w.is_ascii_alphabetic() => {
                    let word: String = chars
                        .clone()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if chars.clone().nth(word.chars().count()) == Some('{') {
                        return Err(unsupported(&format!("'%{word}{{…}}'")));
                    }
                    text.push('%');
                }
                _ => text.push('%'),
            },
            other => text.push(other),
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(WriteOut(parts))
}

impl WriteOut {
    /// Fill the variables from a finished response. `elapsed` is the whole
    /// request, redirects included.
    pub fn render(&self, response: &Response, elapsed: Duration) -> String {
        let mut out = String::new();
        for part in &self.0 {
            match part {
                Part::Text(t) => out.push_str(t),
                // Three digits, so a request with no response is `000`, as in curl.
                Part::Var(Var::HttpCode) => out.push_str(&format!("{:03}", response.status)),
                Part::Var(Var::TimeTotal) => out.push_str(&format!("{:.6}", elapsed.as_secs_f64())),
                Part::Var(Var::SizeDownload) => out.push_str(&response.body.len().to_string()),
                Part::Var(Var::UrlEffective) => out.push_str(&response.url),
                Part::Var(Var::ContentType) => out.push_str(
                    response
                        .headers
                        .get("content-type")
                        .and_then(|v| v.first())
                        .map_or("", String::as_str),
                ),
                Part::Var(Var::RedirectUrl) => out.push_str(&redirect_url(response)),
                Part::Var(Var::NumRedirects) => out.push_str(&response.redirects.to_string()),
            }
        }
        out
    }
}

/// The absolute `Location` of a 3xx response that was handed back, empty
/// otherwise — what curl reports without `-L`.
fn redirect_url(response: &Response) -> String {
    if !(300..400).contains(&response.status) {
        return String::new();
    }
    let Some(location) = response.headers.get("location").and_then(|v| v.first()) else {
        return String::new();
    };
    match url::Url::parse(&response.url).and_then(|base| base.join(location.trim())) {
        Ok(next) => next.to_string(),
        Err(_) => location.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in headers {
            map.entry((*k).to_string()).or_default().push((*v).to_string());
        }
        Response {
            url: "http://h.test/a/b".into(),
            status,
            reason: String::new(),
            headers: map,
            body: body.as_bytes().to_vec(),
            redirects: 2,
            history: Vec::new(),
        }
    }

    #[test]
    fn renders_every_supported_variable() {
        let w = parse("%{http_code} %{response_code} %{size_download} %{url_effective} %{num_redirects} %{time_total}").unwrap();
        let out = w.render(&response(200, &[], "hello"), Duration::from_micros(1500));
        assert_eq!(out, "200 200 5 http://h.test/a/b 2 0.001500");
    }

    #[test]
    fn redirect_url_is_absolute_and_empty_for_a_non_redirect() {
        let w = parse("[%{redirect_url}]").unwrap();
        let r = response(302, &[("location", "../c")], "");
        assert_eq!(w.render(&r, Duration::ZERO), "[http://h.test/c]");
        assert_eq!(w.render(&response(200, &[("location", "/x")], ""), Duration::ZERO), "[]");
    }

    #[test]
    fn escapes_follow_curl() {
        let w = parse("a\\tb\\\\c\\n100%%\\q%z").unwrap();
        assert_eq!(w.render(&response(200, &[], ""), Duration::ZERO), "a\tb\\\\c\n100%\\q%z");
    }

    #[test]
    fn unknown_forms_refuse_and_name_the_supported_set() {
        for bad in ["%{speed_download}", "%{http_code", "%header{x}", "@fmt.txt"] {
            let err = parse(bad).unwrap_err().to_string();
            assert!(err.contains("%{http_code}") || err.contains("-w @file"), "{bad}: {err}");
            assert_eq!(parse(bad).unwrap_err().exit_code(), 2, "{bad}");
        }
    }
}
