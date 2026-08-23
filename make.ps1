param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("all", "build", "bundle", "clean", "sweep", "release", "run", "test", "update", "size", "loc")]
    [string]$Action
)

$ErrorActionPreference = "Stop"

function Run-Command {
    param(
        [string]$Command
    )

    Write-Host ">> $Command" -ForegroundColor Cyan
    Invoke-Expression $Command

    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: ${Command}"
    }
}

function Run-Frontend {
    param([string[]]$Commands)
    Push-Location frontend
    try { foreach ($cmd in $Commands) { Run-Command $cmd } }
    finally { Pop-Location }
}

function Run-CargoChecks {
    param([string]$BuildArgs = "")
    $build = if ($BuildArgs) { "cargo build $BuildArgs --features gui" } else { "cargo build --features gui" }
    Run-Command "cargo fmt"
    Run-Command "cargo test --all-features --quiet"
    Run-Command "cargo clippy --all-targets --all-features -- -D warnings"
    Run-Command $build
}

# Per-extension comment syntax: an optional line marker plus block delimiter pairs.
$LocSyntax = @{
    ".rs"     = @{ Line = "//"; Blocks = @(, @("/*", "*/")) }
    ".ts"     = @{ Line = "//"; Blocks = @(, @("/*", "*/")) }
    ".js"     = @{ Line = "//"; Blocks = @(, @("/*", "*/")) }
    ".mjs"    = @{ Line = "//"; Blocks = @(, @("/*", "*/")) }
    ".svelte" = @{ Line = "//"; Blocks = @(@("/*", "*/"), @("<!--", "-->")) }
    ".html"   = @{ Line = "";   Blocks = @(, @("<!--", "-->")) }
    ".css"    = @{ Line = "";   Blocks = @(, @("/*", "*/")) }
    ".toml"   = @{ Line = "#";  Blocks = @() }
    ".yml"    = @{ Line = "#";  Blocks = @() }
    ".yaml"   = @{ Line = "#";  Blocks = @() }
    ".sh"     = @{ Line = "#";  Blocks = @() }
    ".ps1"    = @{ Line = "#";  Blocks = @(, @("<#", "#>")) }
    ".json"   = @{ Line = "";   Blocks = @() }
}

# Counts code/comment/blank lines the way cloc does: a line is a comment only when
# it contains nothing but a comment; trailing comments after code count as code.
function Measure-Loc {
    param([string[]]$Paths)

    $code = 0; $comment = 0; $blank = 0
    foreach ($path in $Paths) {
        $syntax = $LocSyntax[[System.IO.Path]::GetExtension($path).ToLowerInvariant()]
        if (-not $syntax) { $syntax = @{ Line = ""; Blocks = @() } }
        $closer = ""

        foreach ($raw in [System.IO.File]::ReadAllLines($path)) {
            $line = $raw.Trim()

            if ($closer) {
                $comment++
                if ($line.Contains($closer)) { $closer = "" }
                continue
            }
            if (-not $line) { $blank++; continue }

            $isComment = $false
            if ($syntax.Line -and $line.StartsWith($syntax.Line)) {
                $isComment = $true
            }
            else {
                foreach ($block in $syntax.Blocks) {
                    if (-not $line.StartsWith($block[0])) { continue }
                    $isComment = $true
                    if ($line.IndexOf($block[1], $block[0].Length) -lt 0) { $closer = $block[1] }
                    break
                }
            }

            if ($isComment) { $comment++ } else { $code++ }
        }
    }

    [pscustomobject]@{ Files = $Paths.Count; Code = $code; Comment = $comment; Blank = $blank }
}

Clear-Host
switch ($Action) {
    "update" {
        Run-Command "rustup update"
        Run-Command "cargo update"
        # npm outdated exits 1 when packages are behind: that's informational,
        # not a failure, so run it outside Run-Command.
        Run-Frontend "npm update --no-fund"
        Push-Location frontend
        try { npm outdated } catch {}
        Pop-Location
    }

    "build" {
        Run-Frontend "npm install --no-fund", "npm run build", "npm run check"
        Run-CargoChecks
    }

    "run" {
        Run-Frontend "npm install --no-fund --no-audit", "npm run build"
        Run-Command "cargo run --features gui -- --gui"
    }

    "all" {
        Run-Frontend "npm clean-install --no-fund", "npm run build", "npm run check"
        Run-Command "rustup update"
        Run-Command "cargo update"
        Run-CargoChecks
        Run-Command "cargo run --features gui -- --gui"
    }

    "release" {
        Run-Frontend "npm install --no-fund", "npm run build", "npm run check"
        Run-Command "rustup update"
        Run-Command "cargo update"
        Run-CargoChecks "--release"
    }

    "test" {
        Run-Frontend "npm run check"
        Run-Command "cargo fmt -- --check"
        Run-Command "cargo test --all-features"
        Run-Command "cargo clippy --all-targets --all-features -- -D warnings"
    }

    "clean" {
        Run-Command "cargo clean"
    }

    "sweep" {
        Run-Command "cargo sweep --installed" # cargo install cargo-sweep
    }

    "size" {
        Run-Command "cargo bloat --release --features gui --crates" # cargo install cargo-bloat
    }

    "bundle" {
        $out = Join-Path $PSScriptRoot "bundle.txt"
        $files = Get-ChildItem -Path $PSScriptRoot -Recurse -Include *.rs, *.ts, *.svelte `
            | Where-Object { $_.FullName -notmatch '\\(target|node_modules)\\' } `
            | Where-Object { $_.Name -ne "licenses_generated.ts" } `
            | Sort-Object FullName
        $sb = [System.Text.StringBuilder]::new()
        foreach ($f in $files) {
            $rel = $f.FullName.Substring($PSScriptRoot.Length + 1).Replace('\', '/')
            $null = $sb.AppendLine("=== $rel ===")
            $null = $sb.AppendLine((Get-Content $f.FullName -Raw -Encoding UTF8))
        }
        [System.IO.File]::WriteAllText($out, $sb.ToString(), [System.Text.UTF8Encoding]::new($false))
        Write-Host "Bundled $($files.Count) files -> bundle.txt" -ForegroundColor Cyan
    }

    "loc" {
        $root = $PSScriptRoot
        $groups = [ordered]@{
            "Rust (App)"        = [System.Collections.Generic.List[string]]::new()
            "Rust (Tests)"      = [System.Collections.Generic.List[string]]::new()
            "Svelte Components" = [System.Collections.Generic.List[string]]::new()
            "TypeScript"        = [System.Collections.Generic.List[string]]::new()
            "Build & Config"    = [System.Collections.Generic.List[string]]::new()
        }

        $candidates = Get-ChildItem -Path $root -Recurse -File `
            | Where-Object { $_.FullName -notmatch '\\(target|node_modules|dist|\.git|\.claude)\\' }

        foreach ($file in $candidates) {
            $rel = $file.FullName.Substring($root.Length + 1).Replace('\', '/')
            $group = switch -Regex ($rel) {
                '^src/sync/tests\.rs$'                      { "Rust (Tests)"; break }
                '^src/.+\.rs$'                              { "Rust (App)"; break }
                '^tests/.+\.rs$'                            { "Rust (Tests)"; break }
                '^frontend/src/.+\.svelte$'                 { "Svelte Components"; break }
                '^frontend/src/lib/licenses_generated\.ts$' { $null; break }
                '^frontend/src/.+\.ts$'                     { "TypeScript"; break }
                '^(build\.rs|make\.ps1|make\.sh|Cargo\.toml)$' { "Build & Config"; break }
                '^\.cargo/config\.toml$'                   { "Build & Config"; break }
                '^\.github/workflows/[^/]+\.ya?ml$'        { "Build & Config"; break }
                '^scripts/[^/]+\.(js|mjs|ts)$'              { "Build & Config"; break }
                '^frontend/(vite\.config\.ts|tsconfig[^/]*\.json|package\.json|index\.html)$' { "Build & Config"; break }
                default                                     { $null }
            }
            if ($group) { $groups[$group].Add($file.FullName) }
        }

        $fmt = "{0,-20}{1,8}{2,9}{3,11}{4,8}{5,9}"
        $rule = "-" * 65
        Write-Host ($fmt -f "Group", "Files", "Code", "Comment", "Blank", "Total") -ForegroundColor Cyan
        Write-Host $rule

        $files = 0; $code = 0; $comment = 0; $blank = 0
        foreach ($name in $groups.Keys) {
            if ($groups[$name].Count -eq 0) { continue }
            $stat = Measure-Loc $groups[$name]
            Write-Host ($fmt -f $name, $stat.Files, $stat.Code, $stat.Comment, $stat.Blank, `
                    ($stat.Code + $stat.Comment + $stat.Blank))
            $files += $stat.Files; $code += $stat.Code; $comment += $stat.Comment; $blank += $stat.Blank
        }

        Write-Host $rule
        Write-Host ($fmt -f "Total", $files, $code, $comment, $blank, ($code + $comment + $blank)) -ForegroundColor Cyan
    }
}
