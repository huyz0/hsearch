//! Command-line options of the `hsearch-node` binary.
use std::path::PathBuf;

/// Parsed flags.
#[derive(Debug, Eq, PartialEq)]
pub struct Options {
    /// Address to listen on; loopback unless `insecure` is set.
    pub listen: String,
    /// Disposable scratch directory for engines.
    pub data_dir: PathBuf,
    /// Allow a non-loopback address despite there being no authentication.
    pub insecure: bool,
}

/// Parse the arguments after the program name.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        listen: "127.0.0.1:9200".into(),
        data_dir: PathBuf::from("hsearch-data"),
        insecure: false,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => options.listen = args.next().ok_or("--listen needs an address")?,
            "--data-dir" => options.data_dir = args.next().ok_or("--data-dir needs a path")?.into(),
            "--insecure-no-auth-listen" => options.insecure = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, String> {
        parse_args(args.iter().map(|a| (*a).to_string()))
    }

    #[test]
    fn defaults_are_loopback_and_a_local_directory() {
        let options = parse(&[]).unwrap();
        assert_eq!(options.listen, "127.0.0.1:9200");
        assert_eq!(options.data_dir, PathBuf::from("hsearch-data"));
        assert!(!options.insecure);
    }

    #[test]
    fn flags_override_and_unknown_ones_are_refused() {
        let options = parse(&[
            "--listen",
            "0.0.0.0:1",
            "--data-dir",
            "/tmp/x",
            "--insecure-no-auth-listen",
        ])
        .unwrap();
        assert_eq!(options.listen, "0.0.0.0:1");
        assert_eq!(options.data_dir, PathBuf::from("/tmp/x"));
        assert!(options.insecure);
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--listen"]).is_err());
        assert!(parse(&["--data-dir"]).is_err());
    }
}
