# A3S Box - Justfile

default:
    @just --list

# AI-powered commit message
cz:
    @bash .scripts/generate-commit-message.sh

# ============================================================================
# Build
# ============================================================================

# Build all
build:
    cd src && cargo build --workspace
    just sign-shim debug

# Build release
release:
    cd src && cargo build --workspace --release
    just sign-shim release

# Sign the shim binary with Hypervisor.framework entitlement (macOS)
[macos]
sign-shim profile="debug":
    @codesign --entitlements src/shim/entitlements.plist --force -s - src/target/{{profile}}/a3s-box-shim
    @echo "✓ Signed a3s-box-shim with Hypervisor entitlement"

[linux]
sign-shim profile="debug":
    @echo "✓ No signing needed on Linux"

# Build guest binaries (cross-compile for Linux aarch64 musl)
build-guest profile="release":
    cd src && cargo build -p a3s-box-guest-init --target aarch64-unknown-linux-musl --{{profile}}
    @if [ "{{profile}}" = "release" ]; then \
        aarch64-linux-musl-strip src/target/aarch64-unknown-linux-musl/release/a3s-box-guest-init; \
        aarch64-linux-musl-strip src/target/aarch64-unknown-linux-musl/release/a3s-box-nsexec; \
    fi
    @echo "Guest binaries built at src/target/aarch64-unknown-linux-musl/{{profile}}/"
    @ls -lh src/target/aarch64-unknown-linux-musl/{{profile}}/a3s-box-guest-init src/target/aarch64-unknown-linux-musl/{{profile}}/a3s-box-nsexec 2>/dev/null || true

# ============================================================================
# Test (unified command with progress display)
# ============================================================================

# Run all tests with progress display and module breakdown
test:
    #!/usr/bin/env bash
    set -e

    # Ensure libkrun is findable on macOS (installed via homebrew)
    if [ -d "/opt/homebrew/lib" ]; then
        export LIBRARY_PATH="/opt/homebrew/lib:${LIBRARY_PATH:-}"
    fi

    # Colors
    BOLD='\033[1m'
    GREEN='\033[0;32m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    YELLOW='\033[0;33m'
    RED='\033[0;31m'
    DIM='\033[2m'
    RESET='\033[0m'

    # Counters
    TOTAL_PASSED=0
    TOTAL_FAILED=0
    TOTAL_IGNORED=0
    CRATES_TESTED=0
    CRATES_FAILED=0

    print_header() {
        echo ""
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
        echo -e "${BOLD}  $1${RESET}"
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    }

    # Extract module test counts from cargo test output
    extract_module_counts() {
        local output="$1"
        # Parse "test module::submodule::test_name ... ok" lines
        # Group by top-level module and count
        echo "$output" | grep -E "^test .+::.+ \.\.\. ok$" | \
            sed 's/^test \([^:]*\)::.*/\1/' | \
            sort | uniq -c | sort -rn | \
            while read count module; do
                printf "      ${DIM}%-20s %3d tests${RESET}\n" "$module" "$count"
            done
    }

    run_tests() {
        local crate=$1
        local display_name=$2
        local extra_args=${3:-""}

        CRATES_TESTED=$((CRATES_TESTED + 1))
        echo -ne "${CYAN}▶${RESET} ${BOLD}$display_name${RESET} "

        # Run tests and capture output
        if OUTPUT=$(cd src && cargo test -p "$crate" --lib $extra_args 2>&1); then
            TEST_EXIT=0
        else
            TEST_EXIT=1
        fi

        # Extract test results
        RESULT_LINE=$(echo "$OUTPUT" | grep -E "^test result:" | tail -1)
        if [ -n "$RESULT_LINE" ]; then
            PASSED=$(echo "$RESULT_LINE" | grep -oE '[0-9]+ passed' | grep -oE '[0-9]+' || echo "0")
            FAILED=$(echo "$RESULT_LINE" | grep -oE '[0-9]+ failed' | grep -oE '[0-9]+' || echo "0")
            IGNORED=$(echo "$RESULT_LINE" | grep -oE '[0-9]+ ignored' | grep -oE '[0-9]+' || echo "0")

            TOTAL_PASSED=$((TOTAL_PASSED + PASSED))
            TOTAL_FAILED=$((TOTAL_FAILED + FAILED))
            TOTAL_IGNORED=$((TOTAL_IGNORED + IGNORED))

            if [ "$FAILED" -gt 0 ]; then
                echo -e "${RED}✗${RESET} ${DIM}$PASSED passed, $FAILED failed${RESET}"
                CRATES_FAILED=$((CRATES_FAILED + 1))
                echo "$OUTPUT" | grep -E "^test .* FAILED$" | sed 's/^/    /'
            else
                echo -e "${GREEN}✓${RESET} ${DIM}$PASSED passed${RESET}"
                # Show module breakdown for crates with many tests
                if [ "$PASSED" -gt 10 ]; then
                    extract_module_counts "$OUTPUT"
                fi
            fi
        else
            # No tests found or compilation error
            if echo "$OUTPUT" | grep -q "error\[E"; then
                echo -e "${RED}✗${RESET} ${DIM}compile error${RESET}"
                CRATES_FAILED=$((CRATES_FAILED + 1))
                echo "$OUTPUT" | grep -E "^error" | head -3 | sed 's/^/    /'
            elif [ "$TEST_EXIT" -ne 0 ]; then
                echo -e "${RED}✗${RESET} ${DIM}failed${RESET}"
                CRATES_FAILED=$((CRATES_FAILED + 1))
            else
                echo -e "${YELLOW}○${RESET} ${DIM}no tests${RESET}"
            fi
        fi
    }

    print_header "🧪 A3S Box Test Suite"
    echo ""

    # Test each crate
    run_tests "a3s-box-core"       "core"
    run_tests "a3s-box-runtime"    "runtime"
    run_tests "a3s-box-cli"        "cli"
    run_tests "a3s-box-cri"        "cri"
    run_tests "a3s-box-guest-init" "guest-init"
    run_tests "a3s-box-shim"       "shim"

    # Summary
    echo ""
    echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"

    if [ "$CRATES_FAILED" -gt 0 ]; then
        echo -e "  ${RED}${BOLD}✗ FAILED${RESET}  ${GREEN}$TOTAL_PASSED passed${RESET}  ${RED}$TOTAL_FAILED failed${RESET}  ${YELLOW}$TOTAL_IGNORED ignored${RESET}"
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
        exit 1
    else
        echo -e "  ${GREEN}${BOLD}✓ PASSED${RESET}  ${GREEN}$TOTAL_PASSED passed${RESET}  ${YELLOW}$TOTAL_IGNORED ignored${RESET}  ${DIM}($CRATES_TESTED crates)${RESET}"
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    fi
    echo ""

# Run tests without progress (raw cargo output)
test-raw:
    cd src && cargo test -p a3s-box-core -p a3s-box-runtime -p a3s-box-cli -p a3s-box-cri -p a3s-box-guest-init -p a3s-box-shim --lib

# Run tests with verbose output
test-v:
    cd src && cargo test -p a3s-box-core -p a3s-box-runtime -p a3s-box-cli -p a3s-box-cri -p a3s-box-guest-init -p a3s-box-shim --lib -- --nocapture

# ============================================================================
# Test Subsets
# ============================================================================

# Test a3s-box-core
test-core:
    cd src && cargo test -p a3s-box-core --lib

# Test skill system (in runtime)
test-skills:
    cd src && cargo test -p a3s-box-runtime --lib -- skill

# Test a3s-box-runtime (check only, requires libkrun for actual tests)
test-runtime:
    cd src && A3S_DEPS_STUB=1 cargo check -p a3s-box-runtime -p a3s-box-shim
    cd src && A3S_DEPS_STUB=1 cargo clippy -p a3s-box-runtime -p a3s-box-shim -- -D warnings
    @echo "✓ Runtime compilation passed (actual tests require libkrun)"

# Run VM integration tests (requires built binary + HVF/KVM)
# Usage: just test-vm              # run all integration tests
#        just test-vm <test_name>  # run a specific test
test-vm *ARGS:
    #!/usr/bin/env bash
    set -e
    cd src

    # Locate libkrun/libkrunfw dynamic libraries from cargo build output
    LIBKRUN_LIB=$(ls -td target/debug/build/libkrun-sys-*/out/libkrun/lib 2>/dev/null | head -1)
    LIBKRUNFW_LIB=$(ls -td target/debug/build/libkrun-sys-*/out/libkrunfw/lib 2>/dev/null | head -1)

    if [ -z "$LIBKRUN_LIB" ] || [ -z "$LIBKRUNFW_LIB" ]; then
        echo "❌ libkrun not found. Run 'just build' first."
        exit 1
    fi

    export DYLD_LIBRARY_PATH="${LIBKRUN_LIB}:${LIBKRUNFW_LIB}"
    export LD_LIBRARY_PATH="${LIBKRUN_LIB}:${LIBKRUNFW_LIB}"

    # Verify binary works
    if ! target/debug/a3s-box version >/dev/null 2>&1; then
        echo "❌ a3s-box binary not working. Run 'just build' first."
        exit 1
    fi

    echo "🚀 Running VM integration tests..."
    echo "   DYLD_LIBRARY_PATH=${LIBKRUN_LIB}:${LIBKRUNFW_LIB}"
    echo ""

    ARGS="{{ARGS}}"
    if [ -n "$ARGS" ]; then
        cargo test -p a3s-box-cli --test nginx_integration -- --ignored --nocapture --test-threads=1 "$ARGS"
    else
        cargo test -p a3s-box-cli --test nginx_integration -- --ignored --nocapture --test-threads=1
    fi

# Run TEE integration tests (requires built binary + HVF/KVM)
# Usage: just test-tee                          # run all TEE tests
#        just test-tee test_tee_seal_unseal_lifecycle  # run a specific test
test-tee *ARGS:
    #!/usr/bin/env bash
    set -e
    cd src

    # Locate libkrun/libkrunfw dynamic libraries from cargo build output
    LIBKRUN_LIB=$(ls -td target/debug/build/libkrun-sys-*/out/libkrun/lib 2>/dev/null | head -1)
    LIBKRUNFW_LIB=$(ls -td target/debug/build/libkrun-sys-*/out/libkrunfw/lib 2>/dev/null | head -1)

    if [ -z "$LIBKRUN_LIB" ] || [ -z "$LIBKRUNFW_LIB" ]; then
        echo "❌ libkrun not found. Run 'just build' first."
        exit 1
    fi

    export DYLD_LIBRARY_PATH="${LIBKRUN_LIB}:${LIBKRUNFW_LIB}"
    export LD_LIBRARY_PATH="${LIBKRUN_LIB}:${LIBKRUNFW_LIB}"

    # Verify binary works
    if ! target/debug/a3s-box version >/dev/null 2>&1; then
        echo "❌ a3s-box binary not working. Run 'just build' first."
        exit 1
    fi

    echo "🔒 Running TEE integration tests..."
    echo "   DYLD_LIBRARY_PATH=${LIBKRUN_LIB}:${LIBKRUNFW_LIB}"
    echo ""

    ARGS="{{ARGS}}"
    if [ -n "$ARGS" ]; then
        cargo test -p a3s-box-cli --test tee_integration -- --ignored --nocapture --test-threads=1 "$ARGS"
    else
        cargo test -p a3s-box-cli --test tee_integration -- --ignored --nocapture --test-threads=1
    fi

# ============================================================================
# Coverage (requires: cargo install cargo-llvm-cov, brew install lcov)
# ============================================================================

# Test with coverage - shows real-time test progress + module coverage
test-cov:
    #!/usr/bin/env bash
    set -e

    # Colors
    BOLD='\033[1m'
    GREEN='\033[0;32m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    YELLOW='\033[0;33m'
    RED='\033[0;31m'
    DIM='\033[2m'
    RESET='\033[0m'

    # Clear line and move cursor
    CLEAR_LINE='\033[2K'
    MOVE_UP='\033[1A'

    # Shared temp directory for grand totals
    GRAND_TMP="/tmp/test_cov_grand_$$"
    mkdir -p "$GRAND_TMP"
    echo "0" > "$GRAND_TMP/grand_tests"
    echo "0" > "$GRAND_TMP/grand_lines"
    echo "0" > "$GRAND_TMP/grand_covered"

    print_header() {
        echo ""
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
        echo -e "${BOLD}  $1${RESET}"
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    }

    run_cov_realtime() {
        local crate=$1
        local display_name=$2

        echo -e "${CYAN}▶${RESET} ${BOLD}$display_name${RESET}"
        echo ""

        # Temp files for tracking
        local tmp_dir="/tmp/test_cov_$$_${display_name}"
        mkdir -p "$tmp_dir"

        # Initialize module counters file
        touch "$tmp_dir/module_counts"

        # Run tests with coverage, parse output in real-time
        cd src

        # Use process substitution to read output line by line
        {
            cargo llvm-cov --lib -p "$crate" 2>&1
        } | {
            current_module=""
            module_passed=0
            total_passed=0
            total_failed=0
            declare -A module_counts 2>/dev/null || true  # May fail on bash 3

            while IFS= read -r line; do
                # Check if it's a test result line
                if [[ "$line" =~ ^test\ ([a-z_]+)::.*\.\.\.\ (ok|FAILED)$ ]]; then
                    module="${BASH_REMATCH[1]}"
                    result="${BASH_REMATCH[2]}"

                    # Update counts
                    if [ "$result" = "ok" ]; then
                        total_passed=$((total_passed + 1))
                        # Track per-module (write to file for bash 3 compat)
                        count=$(grep "^${module} " "$tmp_dir/module_counts" 2>/dev/null | awk '{print $2}' || echo "0")
                        count=$((count + 1))
                        grep -v "^${module} " "$tmp_dir/module_counts" > "$tmp_dir/module_counts.tmp" 2>/dev/null || true
                        echo "$module $count" >> "$tmp_dir/module_counts.tmp"
                        mv "$tmp_dir/module_counts.tmp" "$tmp_dir/module_counts"
                    else
                        total_failed=$((total_failed + 1))
                    fi

                    # Show progress (overwrite line)
                    echo -ne "\r${CLEAR_LINE}      ${DIM}Running:${RESET} ${module}::... ${GREEN}${total_passed}${RESET} passed"
                    [ "$total_failed" -gt 0 ] && echo -ne " ${RED}${total_failed}${RESET} failed"

                # Check for compilation message
                elif [[ "$line" =~ ^[[:space:]]*Compiling ]]; then
                    echo -ne "\r${CLEAR_LINE}      ${DIM}Compiling...${RESET}"

                # Check for running tests message
                elif [[ "$line" =~ ^[[:space:]]*Running ]]; then
                    echo -ne "\r${CLEAR_LINE}      ${DIM}Running tests...${RESET}"

                # Check for coverage report lines (save for later)
                elif [[ "$line" =~ ^[a-z_]+.*\.rs[[:space:]] ]]; then
                    echo "$line" >> "$tmp_dir/coverage_lines"

                # Check for TOTAL line
                elif [[ "$line" =~ ^TOTAL ]]; then
                    echo "$line" >> "$tmp_dir/total_line"
                fi
            done

            # Save final counts
            echo "$total_passed" > "$tmp_dir/total_passed"
            echo "$total_failed" > "$tmp_dir/total_failed"
        }

        cd ..

        # Clear progress line
        echo -ne "\r${CLEAR_LINE}"

        # Read results
        total_passed=$(cat "$tmp_dir/total_passed" 2>/dev/null || echo "0")
        total_failed=$(cat "$tmp_dir/total_failed" 2>/dev/null || echo "0")

        # Show final test result
        if [ "$total_failed" -gt 0 ]; then
            echo -e "      ${RED}✗${RESET} ${total_passed} passed, ${RED}${total_failed} failed${RESET}"
        else
            echo -e "      ${GREEN}✓${RESET} ${total_passed} tests passed"
        fi
        echo ""

        # Parse coverage data and aggregate by module
        if [ -f "$tmp_dir/coverage_lines" ]; then
            awk '
            {
                file=$1; lines=$8; missed=$9
                n = split(file, parts, "/")
                if (n > 1) {
                    module = parts[1]
                } else {
                    gsub(/\.rs$/, "", file)
                    module = file
                }
                total_lines[module] += lines
                total_missed[module] += missed
            }
            END {
                for (m in total_lines) {
                    if (total_lines[m] > 0) {
                        covered = total_lines[m] - total_missed[m]
                        pct = (covered / total_lines[m]) * 100
                        printf "%s %.1f %d\n", m, pct, total_lines[m]
                    }
                }
            }' "$tmp_dir/coverage_lines" | sort -t' ' -k2 -rn > "$tmp_dir/cov_agg"

            # Display coverage results with test counts
            echo -e "      ${BOLD}Module               Tests   Coverage${RESET}"
            echo -e "      ${DIM}──────────────────────────────────────────────${RESET}"

            while read module pct lines; do
                # Find test count for this module
                tests=$(grep "^${module} " "$tmp_dir/module_counts" 2>/dev/null | awk '{print $2}' || echo "0")
                [ -z "$tests" ] && tests=0

                # Color the percentage
                num=${pct%.*}
                if [ "$num" -ge 90 ]; then
                    cov_color="${GREEN}${pct}%${RESET}"
                elif [ "$num" -ge 70 ]; then
                    cov_color="${YELLOW}${pct}%${RESET}"
                else
                    cov_color="${RED}${pct}%${RESET}"
                fi
                echo -e "      $(printf '%-18s' "$module") $(printf '%4d' "$tests")   ${cov_color} ${DIM}($lines lines)${RESET}"
            done < "$tmp_dir/cov_agg"

            # Print total and accumulate grand totals
            if [ -f "$tmp_dir/total_line" ]; then
                total_cov=$(cat "$tmp_dir/total_line" | awk '{print $4}' | tr -d '%')
                total_lines=$(cat "$tmp_dir/total_line" | awk '{print $8}')
                total_missed=$(cat "$tmp_dir/total_line" | awk '{print $9}')
                total_covered=$((total_lines - total_missed))
                echo -e "      ${DIM}──────────────────────────────────────────────${RESET}"

                num=${total_cov%.*}
                if [ "$num" -ge 90 ]; then
                    cov_color="${GREEN}${BOLD}${total_cov}%${RESET}"
                elif [ "$num" -ge 70 ]; then
                    cov_color="${YELLOW}${BOLD}${total_cov}%${RESET}"
                else
                    cov_color="${RED}${BOLD}${total_cov}%${RESET}"
                fi
                echo -e "      ${BOLD}$(printf '%-18s' "TOTAL") $(printf '%4d' "$total_passed")${RESET}   ${cov_color} ${DIM}($total_lines lines)${RESET}"

                # Save to grand totals
                echo "$display_name $total_passed $total_lines $total_covered" >> "$GRAND_TMP/crate_stats"
            fi
        fi

        # Cleanup crate tmp
        rm -rf "$tmp_dir"
        echo ""
    }

    print_header "🧪 A3S Box Test Suite with Coverage"
    echo ""

    run_cov_realtime "a3s-box-core" "core"
    run_cov_realtime "a3s-box-runtime" "runtime"
    run_cov_realtime "a3s-box-cli" "cli"
    run_cov_realtime "a3s-box-cri" "cri"
    run_cov_realtime "a3s-box-guest-init" "guest-init"
    run_cov_realtime "a3s-box-shim" "shim"

    # Print grand total summary
    echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo -e "${BOLD}  📊 Overall Summary${RESET}"
    echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo ""

    if [ -f "$GRAND_TMP/crate_stats" ]; then
        echo -e "      ${BOLD}Crate                Tests    Lines   Coverage${RESET}"
        echo -e "      ${DIM}────────────────────────────────────────────────────${RESET}"

        grand_tests=0
        grand_lines=0
        grand_covered=0

        while read crate tests lines covered; do
            grand_tests=$((grand_tests + tests))
            grand_lines=$((grand_lines + lines))
            grand_covered=$((grand_covered + covered))

            if [ "$lines" -gt 0 ]; then
                pct=$(awk "BEGIN {printf \"%.2f\", ($covered / $lines) * 100}")
            else
                pct="0.00"
            fi

            num=${pct%.*}
            if [ "$num" -ge 90 ]; then
                cov_color="${GREEN}${pct}%${RESET}"
            elif [ "$num" -ge 70 ]; then
                cov_color="${YELLOW}${pct}%${RESET}"
            else
                cov_color="${RED}${pct}%${RESET}"
            fi

            echo -e "      $(printf '%-18s' "$crate") $(printf '%5d' "$tests")   $(printf '%6d' "$lines")   ${cov_color}"
        done < "$GRAND_TMP/crate_stats"

        echo -e "      ${DIM}────────────────────────────────────────────────────${RESET}"

        # Calculate grand total percentage
        if [ "$grand_lines" -gt 0 ]; then
            grand_pct=$(awk "BEGIN {printf \"%.2f\", ($grand_covered / $grand_lines) * 100}")
        else
            grand_pct="0.00"
        fi

        num=${grand_pct%.*}
        if [ "$num" -ge 90 ]; then
            grand_cov_color="${GREEN}${BOLD}${grand_pct}%${RESET}"
        elif [ "$num" -ge 70 ]; then
            grand_cov_color="${YELLOW}${BOLD}${grand_pct}%${RESET}"
        else
            grand_cov_color="${RED}${BOLD}${grand_pct}%${RESET}"
        fi

        echo -e "      ${BOLD}$(printf '%-18s' "GRAND TOTAL") $(printf '%5d' "$grand_tests")   $(printf '%6d' "$grand_lines")${RESET}   ${grand_cov_color}"
    fi

    echo ""
    echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo ""

    # Cleanup grand tmp
    rm -rf "$GRAND_TMP"

# Coverage with pretty terminal output
cov:
    #!/usr/bin/env bash
    set -e
    COV_FILE="/tmp/a3s-box-coverage.lcov"
    echo "┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓"
    echo "┃                    🧪 Running Tests with Coverage                     ┃"
    echo "┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛"
    cd src && cargo llvm-cov --lib -p a3s-box-core -p a3s-box-runtime -p a3s-box-cli -p a3s-box-cri -p a3s-box-sdk -p a3s-box-guest-init -p a3s-box-shim \
        --lcov --output-path "$COV_FILE" 2>&1 | grep -E "^test result"
    echo ""
    echo "┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓"
    echo "┃                         📊 Coverage Report                            ┃"
    echo "┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛"
    lcov --summary "$COV_FILE" 2>&1
    rm -f "$COV_FILE"

# Coverage for specific module
cov-module MOD:
    cd src && cargo llvm-cov --lib -p a3s-box-core -- {{MOD}}::

# Coverage with HTML report (opens in browser)
cov-html:
    cd src && cargo llvm-cov --lib -p a3s-box-core -p a3s-box-runtime -p a3s-box-cli -p a3s-box-cri -p a3s-box-sdk -p a3s-box-guest-init -p a3s-box-shim --html --open

# Coverage with detailed file-by-file table
cov-table:
    cd src && cargo llvm-cov --lib -p a3s-box-core -p a3s-box-runtime -p a3s-box-cli -p a3s-box-cri -p a3s-box-sdk -p a3s-box-guest-init -p a3s-box-shim

# Coverage for CI (generates lcov.info)
cov-ci:
    cd src && cargo llvm-cov --lib -p a3s-box-core -p a3s-box-runtime -p a3s-box-cli -p a3s-box-cri -p a3s-box-sdk -p a3s-box-guest-init -p a3s-box-shim --lcov --output-path lcov.info

# ============================================================================
# Code Quality
# ============================================================================

# Format code
fmt:
    cd src && cargo fmt --all

# Lint (clippy)
lint:
    cd src && cargo clippy --all-targets --all-features -- -D warnings

# CI checks (fmt + lint + test)
ci:
    cd src && cargo fmt --all -- --check
    cd src && cargo clippy --all-targets --all-features -- -D warnings
    cd src && cargo test --all

# ============================================================================
# Crate Commands
# ============================================================================

core *ARGS:
    just -f src/core/justfile {{ARGS}}

runtime *ARGS:
    just -f src/runtime/justfile {{ARGS}}

# ============================================================================
# Utilities
# ============================================================================

# Watch and rebuild
watch:
    cd src && cargo watch -x build

# Generate docs
doc:
    cd src && cargo doc --no-deps --open

# Clean artifacts
clean:
    cd src && cargo clean

# ============================================================================
# Docker / OCI Image
# ============================================================================

# Build Docker image for agent
docker-build tag="a3s-box-agent:latest":
    docker build -t {{tag}} .

# Build Docker image with specific platform
docker-build-linux tag="a3s-box-agent:latest":
    docker build --platform linux/amd64 -t {{tag}} .

# Push Docker image to registry
docker-push tag="a3s-box-agent:latest" registry="ghcr.io/a3s-lab":
    docker tag {{tag}} {{registry}}/{{tag}}
    docker push {{registry}}/{{tag}}

# Export Docker image as OCI tarball
docker-export tag="a3s-box-agent:latest" output="a3s-box-agent.tar":
    docker save {{tag}} -o {{output}}

# Build and export OCI image
oci-build tag="a3s-box-agent:latest" output="a3s-box-agent.tar":
    just docker-build-linux {{tag}}
    just docker-export {{tag}} {{output}}

# ============================================================================
# Publish
# ============================================================================

# Publish all crates to crates.io (in dependency order)
publish:
    #!/usr/bin/env bash
    set -e

    # Colors
    BOLD='\033[1m'
    GREEN='\033[0;32m'
    BLUE='\033[0;34m'
    YELLOW='\033[0;33m'
    RED='\033[0;31m'
    DIM='\033[2m'
    RESET='\033[0m'

    print_header() {
        echo ""
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
        echo -e "${BOLD}  $1${RESET}"
        echo -e "${BOLD}${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    }

    print_step() {
        echo -e "${BLUE}▶${RESET} ${BOLD}$1${RESET}"
    }

    print_success() {
        echo -e "${GREEN}✓${RESET} $1"
    }

    print_error() {
        echo -e "${RED}✗${RESET} $1"
        exit 1
    }

    publish_crate() {
        local crate=$1
        local crate_path=$2

        print_header "📦 Publishing ${crate}"
        echo ""

        # Show version
        VERSION=$(grep '^version' "src/${crate_path}/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
        echo -e "  ${DIM}Version:${RESET} ${BOLD}${VERSION}${RESET}"
        echo ""

        # Dry run first
        print_step "Verifying ${crate}..."
        if (cd src && cargo publish -p "$crate" --dry-run); then
            print_success "Package verification OK"
        else
            print_error "Package verification failed for ${crate}."
        fi

        # Publish
        print_step "Publishing ${crate}..."
        if (cd src && cargo publish -p "$crate"); then
            print_success "Published ${crate} v${VERSION}"
        else
            print_error "Publish failed for ${crate}."
        fi

        # Wait for crates.io to index (important for dependencies)
        echo -e "  ${DIM}Waiting for crates.io to index...${RESET}"
        sleep 30
    }

    print_header "📦 Publishing A3S Box Crates to crates.io"
    echo ""
    echo -e "  ${DIM}Crates will be published in dependency order:${RESET}"
    echo -e "    1. a3s-box-core"
    echo -e "    2. a3s-box-runtime"
    echo ""

    # Pre-publish checks
    print_step "Running pre-publish checks..."

    # Format check
    print_step "Checking formatting..."
    if (cd src && cargo fmt --all -- --check); then
        print_success "Formatting OK"
    else
        print_error "Formatting check failed. Run 'just fmt' first."
    fi

    # Lint
    print_step "Running clippy..."
    if (cd src && cargo clippy --all-targets --all-features -- -D warnings); then
        print_success "Clippy OK"
    else
        print_error "Clippy check failed. Fix warnings first."
    fi

    # Test
    print_step "Running tests..."
    if (cd src && cargo test -p a3s-box-core -p a3s-box-runtime -p a3s-box-cli -p a3s-box-cri -p a3s-box-sdk -p a3s-box-guest-init -p a3s-box-shim --lib); then
        print_success "Tests OK"
    else
        print_error "Tests failed."
    fi

    # Publish crates in order
    publish_crate "a3s-box-core" "core"
    publish_crate "a3s-box-runtime" "runtime"

    print_header "✓ All crates published successfully"
    echo ""

# Publish dry-run (verify all crates without publishing)
publish-dry:
    #!/usr/bin/env bash
    set -e

    echo ""
    echo "┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓"
    echo "┃                  📦 Publish Dry Run (A3S Box)                          ┃"
    echo "┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛"
    echo ""

    echo "=== a3s-box-core ==="
    CORE_VERSION=$(grep '^version' src/core/Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
    echo "Version: ${CORE_VERSION}"
    cd src && cargo publish -p a3s-box-core --dry-run
    cd ..
    echo ""

    echo "=== a3s-box-runtime ==="
    RUNTIME_VERSION=$(grep '^version' src/runtime/Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
    echo "Version: ${RUNTIME_VERSION}"
    cd src && cargo publish -p a3s-box-runtime --dry-run
    cd ..
    echo ""

    echo "✓ Dry run successful. Ready to publish with 'just publish'"
    echo ""

# Publish a single crate
publish-crate CRATE:
    #!/usr/bin/env bash
    set -e
    echo ""
    echo "Publishing {{CRATE}}..."
    cd src && cargo publish -p {{CRATE}}
    echo ""
    echo "✓ Published {{CRATE}}"

# Show versions of all crates
version:
    #!/usr/bin/env bash
    echo ""
    echo "A3S Box Crate Versions:"
    echo "  a3s-box-core:    $(grep '^version' src/core/Cargo.toml | head -1 | sed 's/.*\"\(.*\)\".*/\1/')"
    echo "  a3s-box-runtime: $(grep '^version' src/runtime/Cargo.toml | head -1 | sed 's/.*\"\(.*\)\".*/\1/')"
    echo ""

