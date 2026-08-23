use std::path::PathBuf;

/// On Windows, "D:" refers to the current directory on drive D, not the root.
/// Append a separator so "D:" becomes "D:\", matching user intent.
pub(crate) fn normalize_path(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if s.len() == 2 && s.as_bytes()[1] == b':' {
            return PathBuf::from(format!("{}\\", s));
        }
    }
    p
}

pub struct Args {
    pub src: Option<PathBuf>,
    pub dst: Option<PathBuf>,
    pub dry_run: bool,
    pub exclude: Vec<String>,
    pub gui: bool,
    pub port: Option<u16>,
    pub config: Option<PathBuf>,
    pub yolo: bool,
}

const HELP: &str = "\
dirsync: One-way directory mirror sync with smart rename/move detection.

USAGE:
    dirsync [SRC] [DST] [OPTIONS]
    dirsync --gui [OPTIONS]
    dirsync completions <SHELL>

OPTIONS:
    -n, --dry-run           Analyse only, make no changes
    -e, --exclude <PATTERN> Exclude files/dirs matching glob (repeatable)
        --gui               Launch the web GUI
        --port <PORT>       Override GUI server port (1024-65535)
        --config <PATH>     Path to config file
        --yolo              Disable system-critical path checks
    -V, --version           Print version and exit
    -h, --help              Print this help and exit

SUBCOMMANDS:
    completions <SHELL>     Print shell completion script (bash, zsh, fish, powershell)

EXCLUDE PATTERNS:
    Patterns are matched against individual path components, not against the
    whole relative path. `*.tmp` and `node_modules` work; a pattern containing
    a separator (`build/temp`) can never match. Excluding a nested directory
    means naming the directory itself.
";

/// Print the same help text `-h` / `--help` produces.
pub fn print_help() {
    print!("{HELP}");
}

pub fn parse() -> Args {
    parse_from(lexopt::Parser::from_env())
}

fn parse_from(mut parser: lexopt::Parser) -> Args {
    let mut args = Args {
        src: None,
        dst: None,
        dry_run: false,
        exclude: Vec::new(),
        gui: false,
        port: None,
        config: None,
        yolo: false,
    };

    loop {
        let arg = match parser.next() {
            Ok(Some(a)) => a,
            Ok(None) => break,
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(2);
            }
        };

        use lexopt::prelude::*;
        match arg {
            Short('n') | Long("dry-run") => args.dry_run = true,
            Short('e') | Long("exclude") => {
                let val = parser.value().unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(2);
                });
                match val.into_string() {
                    Ok(s) => args.exclude.push(s),
                    Err(v) => {
                        eprintln!("Error: --exclude pattern is not valid UTF-8: {:?}", v);
                        std::process::exit(2);
                    }
                }
            }
            Long("gui") => args.gui = true,
            Long("yolo") => args.yolo = true,
            Long("port") => {
                let val = parser.value().unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(2);
                });
                let port: u16 = val.parse().unwrap_or_else(|_| {
                    eprintln!("Error: invalid port number");
                    std::process::exit(2);
                });
                if let Err(e) = crate::config::validate_port(port) {
                    eprintln!("Error: {e}");
                    std::process::exit(2);
                }
                args.port = Some(port);
            }
            Long("config") => {
                let val = parser.value().unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(2);
                });
                args.config = Some(PathBuf::from(val));
            }
            Short('V') | Long("version") => {
                let build_time_epoch: i64 = env!("BUILD_TIME").parse().unwrap_or(0);
                let build_time = chrono::DateTime::from_timestamp(build_time_epoch, 0)
                    .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                println!(
                    "dirsync {} ({} build, built {})\nLicense: {}",
                    env!("APP_VERSION"),
                    env!("BUILD_PROFILE"),
                    build_time,
                    env!("CARGO_PKG_LICENSE"),
                );
                std::process::exit(0);
            }
            Short('h') | Long("help") => {
                print!("{HELP}");
                std::process::exit(0);
            }
            Value(val) => {
                let path = normalize_path(PathBuf::from(val));
                if args.src.is_none() {
                    args.src = Some(path);
                } else if args.dst.is_none() {
                    args.dst = Some(path);
                } else {
                    eprintln!("Error: unexpected positional argument");
                    std::process::exit(2);
                }
            }
            arg => {
                eprintln!("Error: {}", arg.unexpected());
                std::process::exit(2);
            }
        }
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Args {
        parse_from(lexopt::Parser::from_args(args))
    }

    #[test]
    fn test_normalize_path_leaves_normal_path_unchanged() {
        let p = normalize_path(PathBuf::from("some/path/file.txt"));
        assert_eq!(p, PathBuf::from("some/path/file.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn test_normalize_path_appends_separator_to_bare_drive() {
        assert_eq!(normalize_path(PathBuf::from("D:")), PathBuf::from("D:\\"));
        assert_eq!(normalize_path(PathBuf::from("C:")), PathBuf::from("C:\\"));
    }

    #[test]
    #[cfg(windows)]
    fn test_normalize_path_leaves_rooted_drive_unchanged() {
        assert_eq!(normalize_path(PathBuf::from("D:\\")), PathBuf::from("D:\\"));
        assert_eq!(
            normalize_path(PathBuf::from("D:\\foo")),
            PathBuf::from("D:\\foo")
        );
    }

    #[test]
    fn test_paths_with_spaces() {
        let args = parse_args(&["/my source dir", "/my dest dir"]);
        assert_eq!(args.src, Some(PathBuf::from("/my source dir")));
        assert_eq!(args.dst, Some(PathBuf::from("/my dest dir")));
    }

    #[test]
    fn test_paths_with_special_chars() {
        let args = parse_args(&["/path/with (parens) & stuff", "/dst/[brackets]"]);
        assert_eq!(args.src, Some(PathBuf::from("/path/with (parens) & stuff")));
        assert_eq!(args.dst, Some(PathBuf::from("/dst/[brackets]")));
    }

    #[test]
    fn test_paths_with_unicode() {
        let args = parse_args(&["/données/été", "/cible/résumé"]);
        assert_eq!(args.src, Some(PathBuf::from("/données/été")));
        assert_eq!(args.dst, Some(PathBuf::from("/cible/résumé")));
    }

    #[test]
    fn test_exclude_with_spaces() {
        let args = parse_args(&["/src", "/dst", "--exclude", "my ignored dir/*"]);
        assert_eq!(args.exclude, vec!["my ignored dir/*"]);
    }

    #[test]
    fn test_flags_alongside_spaced_paths() {
        let args = parse_args(&["--dry-run", "/a path/src dir", "/a path/dst dir"]);
        assert!(args.dry_run);
        assert_eq!(args.src, Some(PathBuf::from("/a path/src dir")));
        assert_eq!(args.dst, Some(PathBuf::from("/a path/dst dir")));
    }
}
