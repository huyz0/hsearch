//! Command-line options of the `hsearch-node` binary.
use std::path::PathBuf;

use crate::{Limits, MappingLimits};

/// Parsed flags.
#[derive(Debug, Eq, PartialEq)]
pub struct Options {
    /// Address to listen on; loopback unless `insecure` is set.
    pub listen: String,
    /// Disposable scratch directory for engines.
    pub data_dir: PathBuf,
    /// Allow a non-loopback address despite there being no authentication.
    pub insecure: bool,
    /// Request limits; every default is the M02.05 value.
    pub limits: Limits,
    /// Field, depth and nested-object limits (PR-42).
    pub mapping_limits: MappingLimits,
}

/// Parse the arguments after the program name.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        listen: "127.0.0.1:9200".into(),
        data_dir: PathBuf::from("hsearch-data"),
        insecure: false,
        limits: Limits::default(),
        mapping_limits: MappingLimits::default(),
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => options.listen = args.next().ok_or("--listen needs an address")?,
            "--data-dir" => options.data_dir = args.next().ok_or("--data-dir needs a path")?.into(),
            "--insecure-no-auth-listen" => options.insecure = true,
            "--max-document-bytes" => options.limits.max_document_bytes = number(&mut args, &arg)?,
            "--max-bulk-bytes" => options.limits.max_bulk_bytes = number(&mut args, &arg)?,
            "--max-bulk-items" => options.limits.max_bulk_items = number(&mut args, &arg)?,
            "--buffer-bytes" => options.limits.buffer_bytes = number(&mut args, &arg)?,
            "--memory-budget-bytes" => {
                options.limits.memory_budget_bytes = number(&mut args, &arg)?;
            }
            "--auto-merge-mb" => options.limits.auto_merge_mb = number(&mut args, &arg)?,
            "--max-fields" => options.mapping_limits.max_fields = number(&mut args, &arg)?,
            "--max-field-depth" => options.mapping_limits.max_depth = number(&mut args, &arg)?,
            "--max-nested-objects" => {
                options.mapping_limits.max_nested_objects = number(&mut args, &arg)?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    options.limits.validate()?;
    options.mapping_limits.validate()?;
    Ok(options)
}

fn number<T: std::str::FromStr>(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<T, String> {
    args.next()
        .ok_or_else(|| format!("{flag} needs a number"))?
        .parse()
        .map_err(|_| format!("{flag} needs a whole number"))
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

    #[test]
    fn limits_are_set_by_flag_and_an_unworkable_set_is_refused() {
        let options = parse(&[
            "--max-document-bytes",
            "1048576",
            "--buffer-bytes",
            "262144",
            "--max-bulk-bytes",
            "2097152",
            "--max-bulk-items",
            "10",
            "--memory-budget-bytes",
            "4194304",
        ])
        .unwrap();
        assert_eq!(options.limits.max_document_bytes, 1 << 20);
        assert_eq!(options.limits.buffer_bytes, 256 * 1024);
        assert_eq!(options.limits.max_bulk_items, 10);
        assert!(parse(&["--max-document-bytes", "x"]).is_err());
        assert!(parse(&["--buffer-bytes"]).is_err());
        // The 200 MiB document ceiling is not raisable by configuration.
        assert!(
            parse(&[
                "--max-document-bytes",
                "209715201",
                "--max-bulk-bytes",
                "999999999"
            ])
            .is_err()
        );
        assert!(parse(&["--max-document-bytes", "100", "--max-bulk-bytes", "99"]).is_err());
    }

    #[test]
    fn mapping_limits_are_set_by_flag_and_unworkable_ones_are_refused() {
        assert_eq!(parse(&[]).unwrap().mapping_limits, MappingLimits::default());
        let options = parse(&[
            "--max-fields",
            "50",
            "--max-field-depth",
            "5",
            "--max-nested-objects",
            "7",
        ])
        .unwrap();
        assert_eq!(options.mapping_limits.max_fields, 50);
        assert_eq!(options.mapping_limits.max_depth, 5);
        assert_eq!(options.mapping_limits.max_nested_objects, 7);
        assert!(parse(&["--max-fields", "0"]).is_err());
        assert!(parse(&["--max-fields", "10001"]).is_err());
        assert!(parse(&["--max-field-depth", "0"]).is_err());
        assert!(parse(&["--max-nested-objects", "0"]).is_err());
        assert!(parse(&["--max-fields"]).is_err());
    }

    #[test]
    fn auto_merge_is_on_by_default_and_a_flag_sets_or_disables_it() {
        assert_eq!(parse(&[]).unwrap().limits.auto_merge_mb, 16);
        assert_eq!(
            parse(&["--auto-merge-mb", "64"])
                .unwrap()
                .limits
                .auto_merge_mb,
            64
        );
        assert_eq!(
            parse(&["--auto-merge-mb", "0"])
                .unwrap()
                .limits
                .auto_merge_mb,
            0
        );
        assert!(parse(&["--auto-merge-mb", "1025"]).is_err());
        assert!(parse(&["--auto-merge-mb"]).is_err());
    }
}
