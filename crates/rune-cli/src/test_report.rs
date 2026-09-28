//! Per-test results from libtest's output, and reports for CI (JUnit XML).
//!
//! libtest prints one line per test (`test path::name ... ok`) and, for
//! failures, the captured output under `---- path::name stdout ----`. That is
//! stable across Rust versions (it is what humans read), unlike its JSON
//! format, which still needs `-Z unstable-options`.

use std::fmt::Write as _;

/// How one test ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Passed,
    Failed,
    Ignored,
}

/// One test of a test binary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Case {
    pub name: String,
    pub status: Status,
    /// What the test printed, for a failure.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output: String,
    /// Failed at first and passed when run again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub flaky: bool,
}

/// The tests in one run of a libtest binary.
pub fn parse_cases(output: &str) -> Vec<Case> {
    let mut cases: Vec<Case> = Vec::new();
    for line in output.lines() {
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name, result)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let status = match result.trim() {
            "ok" => Status::Passed,
            "FAILED" => Status::Failed,
            r if r.starts_with("ignored") => Status::Ignored,
            // `bench:` lines and anything else libtest may add.
            _ => continue,
        };
        cases.push(Case {
            name: name.trim().to_string(),
            status,
            output: String::new(),
            flaky: false,
        });
    }
    // Captured output of failures: `---- name stdout ----` up to the next
    // such header or the `failures:` summary list.
    let mut current: Option<(String, String)> = None;
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in output.lines() {
        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|l| l.strip_suffix(" stdout ----"))
        {
            if let Some(done) = current.take() {
                sections.push(done);
            }
            current = Some((name.trim().to_string(), String::new()));
        } else if line.trim() == "failures:" || line.starts_with("test result:") {
            if let Some(done) = current.take() {
                sections.push(done);
            }
        } else if let Some((_, text)) = current.as_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    if let Some(done) = current.take() {
        sections.push(done);
    }
    for (name, text) in sections {
        if let Some(c) = cases.iter_mut().find(|c| c.name == name) {
            c.output = text.trim_end().to_string();
        }
    }
    cases
}

/// The results of one test binary (or doc-test run), for reports.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Suite {
    pub name: String,
    pub seconds: f64,
    /// The binary itself failed without a test failing (a crash, a
    /// `process::exit`): recorded as an error of the suite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub cases: Vec<Case>,
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Characters XML 1.0 cannot hold at all (ANSI escapes, NUL…).
            c if (c as u32) < 0x20 && !matches!(c, '\n' | '\r' | '\t') => {}
            c => out.push(c),
        }
    }
    out
}

/// A JUnit XML document (the shape Jenkins, GitLab, GitHub reporters and
/// most CI systems read): one `<testsuite>` per binary, `<testcase>` per test
/// with its module path as `classname`.
pub fn junit(suites: &[Suite]) -> String {
    let count = |s: &Suite, st: Status| s.cases.iter().filter(|c| c.status == st).count();
    let total_tests: usize = suites.iter().map(|s| s.cases.len()).sum();
    let total_failures: usize = suites.iter().map(|s| count(s, Status::Failed)).sum();
    let total_errors = suites.iter().filter(|s| s.error.is_some()).count();
    let total_time: f64 = suites.iter().map(|s| s.seconds).sum();
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        x,
        "<testsuites name=\"rune test\" tests=\"{total_tests}\" failures=\"{total_failures}\" \
         errors=\"{total_errors}\" time=\"{total_time:.3}\">"
    );
    for s in suites {
        let _ = writeln!(
            x,
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">",
            xml_escape(&s.name),
            s.cases.len(),
            count(s, Status::Failed),
            usize::from(s.error.is_some()),
            count(s, Status::Ignored),
            s.seconds
        );
        for c in &s.cases {
            let (class, name) = match c.name.rsplit_once("::") {
                Some((path, leaf)) => (format!("{}::{path}", s.name), leaf.to_string()),
                None => (s.name.clone(), c.name.clone()),
            };
            let _ = write!(
                x,
                "    <testcase classname=\"{}\" name=\"{}\"",
                xml_escape(&class),
                xml_escape(&name)
            );
            match c.status {
                Status::Passed if !c.flaky => x.push_str("/>\n"),
                Status::Passed => {
                    let _ = writeln!(
                        x,
                        ">\n      <system-out>flaky: failed, then passed when run again\n{}</system-out>\n    </testcase>",
                        xml_escape(&c.output)
                    );
                }
                Status::Ignored => x.push_str(">\n      <skipped/>\n    </testcase>\n"),
                Status::Failed => {
                    let first = c
                        .output
                        .lines()
                        .find(|l| l.contains("panicked"))
                        .unwrap_or("test failed");
                    let _ = writeln!(
                        x,
                        ">\n      <failure message=\"{}\">{}</failure>\n    </testcase>",
                        xml_escape(first.trim()),
                        xml_escape(&c.output)
                    );
                }
            }
        }
        if let Some(e) = &s.error {
            let _ = writeln!(x, "    <system-err>{}</system-err>", xml_escape(e));
        }
        x.push_str("  </testsuite>\n");
    }
    x.push_str("</testsuites>\n");
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &str = "\nrunning 4 tests\ntest tests::adds ... ok\ntest tests::flaky ... FAILED\n\
test tests::slow ... ignored, needs a network\ntest other ... FAILED\n\nfailures:\n\n\
---- tests::flaky stdout ----\n\nthread 'tests::flaky' panicked at src/lib.rs:9:5:\nassertion failed\n\
---- other stdout ----\nhello\nthread 'other' panicked at src/lib.rs:12:5:\nboom\n\n\
failures:\n    other\n    tests::flaky\n\ntest result: FAILED. 1 passed; 2 failed; 1 ignored; 0 measured; 0 filtered out\n";

    #[test]
    fn every_test_line_and_failure_output_is_read() {
        let cases = parse_cases(OUTPUT);
        let summary: Vec<(&str, Status)> =
            cases.iter().map(|c| (c.name.as_str(), c.status)).collect();
        assert_eq!(
            summary,
            [
                ("tests::adds", Status::Passed),
                ("tests::flaky", Status::Failed),
                ("tests::slow", Status::Ignored),
                ("other", Status::Failed),
            ]
        );
        assert!(
            cases[1].output.contains("assertion failed"),
            "{:?}",
            cases[1]
        );
        assert!(cases[3].output.starts_with("hello"), "{:?}", cases[3]);
        assert!(!cases[3].output.contains("tests::flaky"));
    }

    #[test]
    fn doc_test_names_with_spaces_parse() {
        let cases = parse_cases("test src/lib.rs - add (line 3) ... ok\n");
        assert_eq!(cases[0].name, "src/lib.rs - add (line 3)");
    }

    #[test]
    fn junit_counts_and_escapes() {
        let suite = Suite {
            name: "demo (lib)".into(),
            seconds: 0.5,
            error: None,
            cases: parse_cases(OUTPUT),
        };
        let xml = junit(&[suite]);
        assert!(
            xml.contains("tests=\"4\" failures=\"2\" errors=\"0\" skipped=\"1\""),
            "{xml}"
        );
        assert!(
            xml.contains("classname=\"demo (lib)::tests\" name=\"adds\"/>"),
            "{xml}"
        );
        assert!(
            xml.contains(
                "<failure message=\"thread &apos;tests::flaky&apos; panicked at src/lib.rs:9:5:\">"
            ),
            "{xml}"
        );
        assert!(xml.contains("<skipped/>"));
    }
}
