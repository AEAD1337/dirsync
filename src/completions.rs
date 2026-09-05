/// Print a shell completion script for the given shell and exit.
pub fn print(shell: &str) {
    match shell {
        "bash" => print!("{BASH}"),
        "zsh" => print!("{ZSH}"),
        "fish" => print!("{FISH}"),
        "powershell" => print!("{POWERSHELL}"),
        other => {
            eprintln!("Unknown shell {other:?}. Supported: bash, zsh, fish, powershell");
            std::process::exit(2);
        }
    }
}

// ── Bash ─────────────────────────────────────────────────────────────────────

const BASH: &str = r#"# bash completion for dirsync
# Source this file or drop it in /etc/bash_completion.d/
_dirsync() {
    local cur="${COMP_WORDS[COMP_CWORD]}"
    local prev="${COMP_WORDS[COMP_CWORD-1]}"

    # After 'completions', complete shell names
    local w
    for w in "${COMP_WORDS[@]}"; do
        if [[ "$w" == "completions" ]]; then
            COMPREPLY=($(compgen -W "bash zsh fish powershell" -- "$cur"))
            return
        fi
    done

    case "$prev" in
        --config)
            COMPREPLY=($(compgen -f -- "$cur"))
            return ;;
        --port|-e|--exclude)
            COMPREPLY=()
            return ;;
    esac

    if [[ "$cur" == -* ]]; then
        local opts="-n --dry-run -e --exclude --gui --port --config --yolo -V --version -h --help"
        COMPREPLY=($(compgen -W "$opts" -- "$cur"))
        return
    fi

    # SRC / DST positional: directories; also offer the completions subcommand
    COMPREPLY=($(compgen -W "completions" -- "$cur"))
    COMPREPLY+=($(compgen -d -- "$cur"))
}
complete -F _dirsync dirsync
"#;

// ── Zsh ──────────────────────────────────────────────────────────────────────

const ZSH: &str = r#"#compdef dirsync

_dirsync() {
    local state

    _arguments \
        '(-n --dry-run)'{-n,--dry-run}'[Analyse only, make no changes]' \
        '*'{-e,--exclude}'[Exclude files/dirs matching glob]:pattern' \
        '--gui[Launch the web GUI]' \
        '--port[Override GUI server port]:port' \
        '--config[Path to config file]:config file:_files' \
        '--yolo[Disable system-critical path checks]' \
        '(-V --version)'{-V,--version}'[Print version and exit]' \
        '(-h --help)'{-h,--help}'[Print help and exit]' \
        '1:src or subcommand:->first' \
        '2::dst directory:_directories' && return 0

    case $state in
        first)
            _alternative \
                'subcmds:subcommand:((completions\:"Print shell completion script"))' \
                'dirs:source directory:_directories'
            ;;
    esac
}

_dirsync "$@"
"#;

// ── Fish ─────────────────────────────────────────────────────────────────────

const FISH: &str = r#"# fish completion for dirsync
complete -c dirsync -f

# Subcommand: completions
complete -c dirsync -n '__fish_use_subcommand' -a completions -d 'Print shell completion script'
complete -c dirsync -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish powershell' -f -d 'Shell'

# Options (suppress when inside a subcommand)
complete -c dirsync -n '__fish_use_subcommand' -s n -l dry-run -d 'Analyse only, make no changes'
complete -c dirsync -n '__fish_use_subcommand' -s e -l exclude -r -d 'Exclude files/dirs matching glob (repeatable)'
complete -c dirsync -n '__fish_use_subcommand' -l gui -d 'Launch the web GUI'
complete -c dirsync -n '__fish_use_subcommand' -l port -r -d 'Override GUI server port'
complete -c dirsync -n '__fish_use_subcommand' -l config -r -a '(__fish_complete_path)' -d 'Path to config file'
complete -c dirsync -n '__fish_use_subcommand' -l yolo -d 'Disable system-critical path checks'
complete -c dirsync -n '__fish_use_subcommand' -s V -l version -d 'Print version and exit'
complete -c dirsync -n '__fish_use_subcommand' -s h -l help -d 'Print help and exit'

# SRC / DST directory completions
complete -c dirsync -n '__fish_use_subcommand' -a '(__fish_complete_directories)' -d 'Directory'
"#;

// ── PowerShell ────────────────────────────────────────────────────────────────

const POWERSHELL: &str = r#"# PowerShell completion for dirsync
# Add to your $PROFILE: . /path/to/dirsync.ps1
Register-ArgumentCompleter -Native -CommandName dirsync -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $tokens = $commandAst.CommandElements | Select-Object -Skip 1

    # Detect 'completions' subcommand
    $sub = $tokens | Where-Object { -not "$_".StartsWith('-') } | Select-Object -First 1 -ExpandProperty Value
    if ($sub -eq 'completions') {
        @('bash', 'zsh', 'fish', 'powershell') |
            Where-Object { $_ -like "$wordToComplete*" } |
            ForEach-Object {
                [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', "Shell: $_")
            }
        return
    }

    if ($wordToComplete.StartsWith('-')) {
        @(
            @('--dry-run',  '--dry-run',  'Analyse only, make no changes'),
            @('-n',         '-n',         'Analyse only, make no changes'),
            @('--exclude',  '--exclude',  'Exclude files/dirs matching glob'),
            @('-e',         '-e',         'Exclude files/dirs matching glob'),
            @('--gui',      '--gui',      'Launch the web GUI'),
            @('--port',     '--port',     'Override GUI server port'),
            @('--config',   '--config',   'Path to config file'),
            @('--yolo',     '--yolo',     'Disable system-critical path checks'),
            @('--version',  '--version',  'Print version and exit'),
            @('-V',         '-V',         'Print version and exit'),
            @('--help',     '--help',     'Print help and exit'),
            @('-h',         '-h',         'Print help and exit')
        ) | Where-Object { $_[0] -like "$wordToComplete*" } |
            ForEach-Object {
                [System.Management.Automation.CompletionResult]::new($_[1], $_[1], 'ParameterValue', $_[2])
            }
        return
    }

    # Offer 'completions' subcommand when no positional args yet
    if (-not ($tokens | Where-Object { -not "$_".StartsWith('-') })) {
        if ('completions' -like "$wordToComplete*") {
            [System.Management.Automation.CompletionResult]::new('completions', 'completions', 'ParameterValue', 'Print shell completion script')
        }
    }

    # Directory completions for SRC / DST
    $base = if ($wordToComplete) { $wordToComplete } else { '.' }
    Get-ChildItem -Directory -Path "${base}*" -ErrorAction SilentlyContinue |
        ForEach-Object {
            [System.Management.Automation.CompletionResult]::new($_.FullName, $_.Name, 'ProviderItem', 'Directory')
        }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every supported shell must produce a script rather than the error arm.
    /// The unknown-shell arm exits the process, so it is covered from the
    /// outside in tests/cli_binary.rs instead.
    #[test]
    fn print_emits_a_script_for_every_supported_shell() {
        for shell in ["bash", "zsh", "fish", "powershell"] {
            print(shell);
        }
    }

    #[test]
    fn every_script_completes_the_completions_subcommand() {
        for script in [BASH, ZSH, FISH, POWERSHELL] {
            assert!(script.contains("dirsync"));
            assert!(script.contains("completions"));
        }
    }

    #[test]
    fn every_script_offers_the_documented_flags() {
        // fish spells long options without the leading dashes (`-l dry-run`),
        // so the shared assertion uses the bare option name.
        for script in [BASH, ZSH, FISH, POWERSHELL] {
            for flag in ["dry-run", "exclude", "gui", "port", "yolo"] {
                assert!(script.contains(flag), "{flag} missing from a script");
            }
        }
    }

    #[test]
    fn shell_name_completion_lists_every_supported_shell() {
        // The shells that complete the argument of `completions` must offer
        // exactly the set `print` accepts.
        for script in [BASH, FISH, POWERSHELL] {
            for shell in ["bash", "zsh", "fish", "powershell"] {
                assert!(script.contains(shell), "{shell} missing from a script");
            }
        }
    }
}
