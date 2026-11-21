//! Assertions on the generated completion scripts: path arguments must survive
//! spaces in directory names, and `--config` must complete files.

use dirsync::completions::script;

#[test]
fn every_supported_shell_has_a_script_and_others_do_not() {
    for shell in ["bash", "zsh", "fish", "powershell"] {
        assert!(script(shell).is_some(), "{shell} has no script");
    }
    assert!(script("tcsh").is_none());
}

#[test]
fn bash_splits_path_candidates_on_newlines_only() {
    let bash = script("bash").unwrap();
    // Every path compgen runs with IFS set to a newline, so a directory
    // called "My Files" stays one candidate instead of two.
    assert!(
        bash.contains("local IFS=$'\\n'"),
        "IFS not narrowed:\n{bash}"
    );
    for line in bash.lines() {
        let line = line.trim();
        if line.contains("compgen -d") || line.contains("compgen -f") {
            let before = &bash[..bash.find(line).unwrap()];
            let func_start = before.rfind("_dirsync()").unwrap();
            assert!(
                before[func_start..].contains("local IFS=$'\\n'"),
                "path compgen without a newline IFS: {line}"
            );
        }
    }
}

/// Runs the bash script for real where a bash is available: `_dirsync` is
/// called with a hand-built COMP_WORDS and prints one candidate per line.
/// Returns `None` (and the caller skips) when no usable bash exists.
fn bash_candidates(dir: &std::path::Path, words: &[&str]) -> Option<Vec<String>> {
    let script_path = dir.join("dirsync.bash");
    std::fs::write(&script_path, script("bash").unwrap()).unwrap();
    let quoted: Vec<String> = words
        .iter()
        .map(|w| format!("'{}'", w.replace('\'', r"'\''")))
        .collect();
    let driver = format!(
        "source ./dirsync.bash\nCOMP_WORDS=({})\nCOMP_CWORD={}\n_dirsync\nprintf '%s\\n' \"${{COMPREPLY[@]}}\"\n",
        quoted.join(" "),
        words.len() - 1
    );
    // The driver goes in a file, not `bash -c`: on Windows `Command` finds
    // System32's WSL launcher before PATH, and it expands `$` references in
    // its command line before the inner bash runs, emptying COMPREPLY.
    std::fs::write(dir.join("driver.sh"), driver).unwrap();
    let out = std::process::Command::new("bash")
        .arg("driver.sh")
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "skipping: bash failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort();
    Some(lines)
}

#[test]
fn bash_keeps_a_directory_with_spaces_as_one_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("My Files")).unwrap();
    std::fs::create_dir(tmp.path().join("Mine")).unwrap();
    let Some(got) = bash_candidates(tmp.path(), &["dirsync", "M"]) else {
        return;
    };
    assert_eq!(got, ["Mine", "My Files"]);
}

#[test]
fn bash_word_lists_still_split_into_separate_candidates() {
    // compgen -W splits its word list on IFS: narrowing IFS before it would
    // turn "bash zsh fish powershell" into a single candidate.
    let tmp = tempfile::tempdir().unwrap();
    let Some(got) = bash_candidates(tmp.path(), &["dirsync", "completions", ""]) else {
        return;
    };
    assert_eq!(got, ["bash", "fish", "powershell", "zsh"]);
    let Some(got) = bash_candidates(tmp.path(), &["dirsync", "--d"]) else {
        return;
    };
    assert_eq!(got, ["--dry-run"]);
}

#[test]
fn bash_registers_with_filename_handling() {
    let bash = script("bash").unwrap();
    assert!(bash.contains("complete -o filenames -F _dirsync dirsync"));
}

#[test]
fn powershell_quotes_completion_text_with_spaces() {
    let ps = script("powershell").unwrap();
    // The raw FullName must not be handed to CompletionResult unquoted.
    assert!(
        !ps.contains("CompletionResult]::new($_.FullName"),
        "FullName inserted unquoted:\n{ps}"
    );
    assert!(ps.contains("$quote"), "no quoting helper:\n{ps}");
}

#[test]
fn powershell_completes_files_after_config() {
    let ps = script("powershell").unwrap();
    assert!(ps.contains("'--config'"), "no --config branch:\n{ps}");
    // The --config branch lists files as well as directories, so it must
    // call Get-ChildItem without -Directory somewhere.
    let unfiltered = ps
        .lines()
        .filter(|l| l.contains("Get-ChildItem"))
        .any(|l| !l.contains("-Directory"));
    assert!(unfiltered, "only directories are ever completed:\n{ps}");
}

#[test]
fn zsh_and_fish_complete_files_after_config() {
    assert!(
        script("zsh")
            .unwrap()
            .contains("'--config[Path to config file]:config file:_files'")
    );
    assert!(
        script("fish")
            .unwrap()
            .contains("-l config -r -a '(__fish_complete_path)'")
    );
}

/// Runs the PowerShell script's `$quote` helper for real, where a PowerShell
/// is available, and parses each completion text the way the shell would.
/// Returns the paths whose text does not come back as exactly one argument
/// with the path's own value, or `None` (and the caller skips) without one.
fn powershell_misquoted(paths: &[&str]) -> Option<Vec<String>> {
    let ps = script("powershell").unwrap();
    let start = ps.find("    $quote = {").expect("no $quote helper");
    let len = ps[start..].find("\n    }\n").expect("unterminated $quote") + "\n    }\n".len();
    let helper = &ps[start..start + len];

    let dir = tempfile::TempDir::new().unwrap();
    let json: Vec<String> = paths
        .iter()
        .map(|p| serde_json::to_string(p).unwrap())
        .collect();
    std::fs::write(
        dir.path().join("paths.json"),
        format!("[{}]", json.join(",")),
    )
    .unwrap();
    // Parse `x <text>`: one command element after `x`, a constant whose
    // value is the path, or the completion would run or split something.
    let driver = format!(
        "{helper}\n\
         $paths = Get-Content -Raw -Encoding UTF8 paths.json | ConvertFrom-Json\n\
         foreach ($p in $paths) {{\n\
             $text = & $quote $p\n\
             $tokens = $null; $errors = $null\n\
             $ast = [System.Management.Automation.Language.Parser]::ParseInput(\"x $text\", [ref]$tokens, [ref]$errors)\n\
             $els = @($ast.EndBlock.Statements[0].PipelineElements[0].CommandElements)\n\
             $ok = $errors.Count -eq 0 -and $els.Count -eq 2 -and\n\
                 $els[1] -is [System.Management.Automation.Language.StringConstantExpressionAst] -and\n\
                 $els[1].Value -ceq $p\n\
             if (-not $ok) {{ [Console]::Out.WriteLine(\"BAD $p\") }}\n\
         }}\n\
         [Console]::Out.WriteLine('DONE')\n"
    );
    // UTF-8 with a BOM, so Windows PowerShell 5.1 reads the script as UTF-8.
    std::fs::write(dir.path().join("driver.ps1"), format!("\u{feff}{driver}")).unwrap();
    let out = ["pwsh", "powershell"].iter().find_map(|exe| {
        std::process::Command::new(exe)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                "driver.ps1",
            ])
            .current_dir(dir.path())
            .output()
            .ok()
    })?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains("DONE"),
        "driver failed: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(
        stdout
            .lines()
            .filter_map(|l| l.strip_prefix("BAD ").map(str::to_owned))
            .collect(),
    )
}

#[test]
fn powershell_completion_text_is_always_one_literal_argument() {
    // PowerShell treats U+2018 to U+201B as single quotes and U+201C to
    // U+201E as double quotes, and a backtick escapes the next character:
    // a planted directory name must not close the quoting and run code
    // when the user accepts the completion.
    let paths = [
        "plain",
        "/home/user/My Files",
        r"C:\Users\it's",
        "C:\\x\u{2019}; Set-Variable pwned 1; \u{2019}",
        "/tmp/a b\u{2018}$(Set-Variable pwned 1)\u{2018}",
        "/tmp/it\u{2019}s",
        "/tmp/q\u{201A}x\u{201B}y",
        "/tmp/d\u{201C}x\u{201D}y\u{201E}",
        "/tmp/back`tick",
        "/tmp/dollar$HOME",
        "/tmp/#hash",
    ];
    let Some(bad) = powershell_misquoted(&paths) else {
        eprintln!("skipped: no PowerShell available");
        return;
    };
    assert!(bad.is_empty(), "misquoted: {bad:?}");
}
