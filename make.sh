#!/usr/bin/env bash
set -euo pipefail

ACTION="${1:-}"
if [[ -z "$ACTION" ]]; then
    echo "Usage: ./make.sh <action>"
    echo "Actions: all build clean sweep release run test update size loc coverage"
    exit 1
fi

CYAN='\033[0;36m'
NC='\033[0m'

run() {
    echo -e "${CYAN}>> $*${NC}"
    "$@"
}

run_frontend() {
    pushd frontend > /dev/null
    run "$@"
    popd > /dev/null
}

run_cargo_checks() {
    local extra="${1:-}"
    run cargo fmt
    run cargo test --all-features --quiet
    run cargo clippy --all-targets --all-features -- -D warnings
    if [[ -n "$extra" ]]; then
        run cargo build --features gui $extra
    else
        run cargo build --features gui
    fi
}

case "$ACTION" in
    update)
        run rustup update
        run cargo update
        run_frontend npm update --no-fund
        # npm outdated exits 1 when packages are behind: informational, not a failure.
        pushd frontend > /dev/null; npm outdated || true; popd > /dev/null
        ;;
    build)
        run_frontend npm install --no-fund
        run_frontend npm run build
        run_cargo_checks
        ;;
    run)
        run_frontend npm install --no-fund --no-audit
        run_frontend npm run build
        run cargo run --features gui -- --gui
        ;;
    all)
        run_frontend npm clean-install --no-fund
        run_frontend npm run build
        run rustup update
        run cargo update
        run_cargo_checks
        run cargo run --features gui -- --gui
        ;;
    release)
        run_frontend npm install --no-fund
        run_frontend npm run build
        run rustup update
        run cargo update
        run_cargo_checks "--release"
        ;;
    test)
        run_frontend npm run check
        run cargo fmt -- --check
        run cargo test --all-features
        run cargo clippy --all-targets --all-features -- -D warnings
        ;;
    clean)
        run cargo clean
        ;;
    sweep)
        run cargo sweep --installed
        ;;
    coverage)
        # CLI-only on purpose: the gui modules are axum wiring and WebSocket
        # plumbing with no test harness, so including them only dilutes the
        # number for the code the tests actually exercise.
        run cargo llvm-cov --no-default-features --summary-only # cargo install cargo-llvm-cov
        ;;

    size)
        run cargo bloat --release --features gui --crates
        ;;
    loc)
        loc_files() {
            find . -type f \
                | sed 's|^\./||' \
                | grep -Ev '^(target|node_modules|\.git|\.claude)/|/(target|node_modules|dist)/'
        }

        # Counts code/comment/blank lines the way cloc does: a line is a comment only
        # when it contains nothing but a comment; trailing comments count as code.
        loc_count() {
            local pattern="$1" exclude="${2:-}" files
            files=$(loc_files | grep -E "$pattern" || true)
            if [[ -n "$exclude" ]]; then
                files=$(echo "$files" | grep -Ev "$exclude" || true)
            fi
            if [[ -z "$files" ]]; then
                echo "0 0 0 0"
                return
            fi
            echo "$files" | tr '\n' '\0' | xargs -0 awk '
            FNR == 1 {
                files++; closer = ""
                ext = FILENAME; sub(/^.*\./, "", ext)
                lc = ""; bo = ""; bc = ""; bo2 = ""; bc2 = ""
                if (ext ~ /^(rs|ts|js|mjs)$/)      { lc = "//"; bo = "/*"; bc = "*/" }
                else if (ext == "svelte")          { lc = "//"; bo = "/*"; bc = "*/"; bo2 = "<!--"; bc2 = "-->" }
                else if (ext == "html")            { bo = "<!--"; bc = "-->" }
                else if (ext == "css")             { bo = "/*"; bc = "*/" }
                else if (ext ~ /^(toml|ya?ml|sh)$/) { lc = "#" }
                else if (ext == "ps1")             { lc = "#"; bo = "<#"; bc = "#>" }
            }
            {
                line = $0
                gsub(/^[ \t\r]+|[ \t\r]+$/, "", line)
                if (closer != "") {
                    comment++
                    if (index(line, closer) > 0) closer = ""
                    next
                }
                if (line == "") { blank++; next }
                if (lc != "" && substr(line, 1, length(lc)) == lc) { comment++; next }
                if (bo != "" && substr(line, 1, length(bo)) == bo) {
                    comment++
                    if (index(substr(line, length(bo) + 1), bc) == 0) closer = bc
                    next
                }
                if (bo2 != "" && substr(line, 1, length(bo2)) == bo2) {
                    comment++
                    if (index(substr(line, length(bo2) + 1), bc2) == 0) closer = bc2
                    next
                }
                code++
            }
            END { printf "%d %d %d %d\n", files, code, comment, blank }'
        }

        fmt='%-20s%8s%9s%11s%8s%9s'
        rule=$(printf '%.0s-' {1..65})
        sum_files=0; sum_code=0; sum_comment=0; sum_blank=0

        print_group() {
            local name="$1" stats
            stats=$(loc_count "$2" "${3:-}")
            read -r f c m b <<< "$stats"
            [[ "$f" -eq 0 ]] && return
            printf "$fmt\n" "$name" "$f" "$c" "$m" "$b" "$((c + m + b))"
            sum_files=$((sum_files + f)); sum_code=$((sum_code + c))
            sum_comment=$((sum_comment + m)); sum_blank=$((sum_blank + b))
        }

        printf "${CYAN}$fmt${NC}\n" "Group" "Files" "Code" "Comment" "Blank" "Total"
        echo "$rule"
        print_group "Rust (App)"        '^src/.+\.rs$' '^src/sync/tests\.rs$'
        print_group "Rust (Tests)"      '^(tests/.+\.rs|src/sync/tests\.rs)$'
        print_group "Svelte Components" '^frontend/src/.+\.svelte$'
        print_group "TypeScript"        '^frontend/src/.+\.ts$' 'licenses_generated\.ts$'
        print_group "Build & Config"    '^(build\.rs|make\.ps1|make\.sh|Cargo\.toml|\.cargo/config\.toml|\.github/workflows/[^/]+\.ya?ml|scripts/[^/]+\.(js|mjs|ts)|frontend/(vite\.config\.ts|tsconfig[^/]*\.json|package\.json|index\.html))$'
        echo "$rule"
        printf "${CYAN}$fmt${NC}\n" "Total" "$sum_files" "$sum_code" "$sum_comment" "$sum_blank" \
            "$((sum_code + sum_comment + sum_blank))"
        ;;
    *)
        echo "Unknown action: $ACTION"
        echo "Actions: all build clean sweep release run test update size loc"
        exit 1
        ;;
esac
