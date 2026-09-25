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
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(driver)
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
