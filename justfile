# Build-free source checks; native compilation and tests run on build devices.
set positional-arguments

# Lists the available project commands.
default:
    @just --list

# Fetches locked sources and advisory data using the prepared toolchain.
setup:
    cargo fetch --locked
    cargo deny --locked fetch all

# Runs build-free source checks without network access.
check: format workflows dependencies whitespace

# Applies the project's formatters.
fix:
    just --fmt
    cargo fmt --all

[private]
format:
    just --fmt --check
    cargo fmt --all -- --check

[private]
workflows:
    actionlint
    zizmor --offline --no-progress .github/workflows

[private]
dependencies:
    cargo deny --frozen check --hide-inclusion-graph
    cargo shear --locked --offline --deny-warnings

[private]
whitespace:
    git diff --check

# Run only on a build device or runner. Native feature sets are explicit.
test features="":
    cargo clippy --locked --workspace --all-targets --features "$1" -- -D warnings
    cargo nextest run --locked --workspace --all-targets --features "$1"
    cargo test --locked --workspace --doc --features "$1"

# Builds release binaries with an explicit native feature selection when needed.
build features="":
    cargo build --locked --release --workspace --features "$1"

# Runs source, test and release checks for the same selected feature set.
verify features="":
    just check
    just test "$1"
    just build "$1"
