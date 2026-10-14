//! The command line.

use std::ffi::OsString;
use std::io::Write;

use crate::inner;

/// The exit status of a command line that does not parse (`EX_USAGE`).
pub const EXIT_USAGE: u8 = 64;

const USAGE: &str = "usage: kablo <command>

commands:
  version   print the version and the protocol versions this build speaks
  help      print this text
";

/// The line `kablo version` prints: `key=value` tokens, to which keys may be
/// added but from which none is removed.
pub fn version_line() -> String {
    format!(
        "version={} wire_min={} wire_max={} inner_min={} inner_max={}",
        env!("CARGO_PKG_VERSION"),
        kablo_proto::MIN_VERSION,
        kablo_proto::MAX_VERSION,
        inner::MIN_VERSION,
        inner::MAX_VERSION,
    )
}

/// Runs a command line (without the program name) and returns the exit status.
pub fn run<I>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    let words: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
    let all_text = words.len() == args.len();
    match (all_text, words.as_slice()) {
        (true, ["version"]) => {
            let _ = writeln!(out, "{}", version_line());
            0
        }
        (true, ["help" | "-h" | "--help"]) => {
            let _ = write!(out, "{USAGE}");
            0
        }
        _ => {
            let _ = write!(err, "{USAGE}");
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_words(words: &[&str]) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(words.iter().map(OsString::from), &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn version_is_one_line_of_tokens() {
        let (code, out, err) = run_words(&["version"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out.lines().count(), 1);
        assert_eq!(out, format!("{}\n", version_line()));
        for token in out.split_whitespace() {
            assert!(
                token
                    .split_once('=')
                    .is_some_and(|(k, v)| !k.is_empty() && !v.is_empty()),
                "{token}"
            );
        }
    }

    #[test]
    fn anything_else_is_a_usage_error() {
        for words in [
            &[][..],
            &["bogus"],
            &["version", "extra"],
            &["--version"],
            &[""],
        ] {
            let (code, out, err) = run_words(words);
            assert_eq!(code, EXIT_USAGE, "{words:?}");
            assert_eq!(out, "");
            assert!(err.starts_with("usage: kablo"), "{words:?}");
        }
        let mut err = Vec::new();
        assert_eq!(
            run([OsString::from("ver\u{0}sion")], &mut Vec::new(), &mut err),
            EXIT_USAGE
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert_eq!(
                run([OsString::from_vec(vec![0xff])], &mut Vec::new(), &mut err),
                EXIT_USAGE
            );
        }
    }

    #[test]
    fn help_goes_to_stdout_and_succeeds() {
        for word in ["help", "-h", "--help"] {
            let (code, out, err) = run_words(&[word]);
            assert_eq!((code, err.as_str()), (0, ""));
            assert!(out.starts_with("usage: kablo"));
        }
    }
}
