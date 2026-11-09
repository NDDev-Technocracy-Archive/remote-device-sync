# Build-free source checks; native compilation and tests run on build devices.
set positional-arguments

default: check

check: format workflows dependencies whitespace

format:
    just --fmt --check
    cargo fmt --all -- --check

workflows:
    actionlint
    zizmor --offline --no-progress .github/workflows .github/dependabot.yml

dependencies:
    cargo deny --locked check --hide-inclusion-graph
    cargo shear --locked --offline --deny-warnings

whitespace:
    git diff --check

# Run only on a build device or runner. Native feature sets are explicit.
test features="":
    cargo clippy --locked --workspace --all-targets --features "$1" -- -D warnings
    cargo nextest run --locked --workspace --all-targets --features "$1"
    cargo test --locked --workspace --doc --features "$1"
