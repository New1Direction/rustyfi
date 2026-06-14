#!/bin/sh
# Run the rustyfi benchmark suite: every repo in bench/repos.toml through the
# release CLI, one JSON result per repo into bench/.work/results/, then aggregate.
#
#   bench/run.sh                  # full suite
#   bench/run.sh prompt-cache     # single repo by name
#   bench/run.sh --aggregate-only # rebuild RESULTS.md from existing JSON
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/bench"
WORK="$BENCH/.work"
RESULTS="$WORK/results"
ONLY="${1:-}"

# The gate: set RUSTYFI_BENCH_DEEP=1 to engage the agentic --deep doctor on
# residual errors. Pair with a strong fix model, e.g. the local Claude Code CLI
# at no extra cost:  RUSTYFI_FIX_PROVIDER=claude_cli RUSTYFI_FIX_MODEL=opus
DEEP=""
[ -n "${RUSTYFI_BENCH_DEEP:-}" ] && DEEP="--deep"

say() { printf '\033[1;33m[bench]\033[0m %s\n' "$1" >&2; }

[ "$ONLY" = "--aggregate-only" ] || {
  # Translation needs an API key unless it too runs through the Claude Code CLI.
  case "${RUSTYFI_PROVIDER:-}" in
    claude*) ;;
    *) [ -n "${RUSTYFI_LLM_API_KEY:-}" ] || { say "RUSTYFI_LLM_API_KEY not set"; exit 2; } ;;
  esac
  command -v python3 >/dev/null 2>&1 || { say "python3 is required"; exit 2; }
  python3 -c 'import tomllib' 2>/dev/null || python3 -c 'import tomli' 2>/dev/null \
    || { say "python3 needs tomllib (3.11+) or: pip install tomli"; exit 2; }
  say "building release CLI"
  cargo build --release -p rustyfi-cli --manifest-path "$ROOT/Cargo.toml" >&2
  mkdir -p "$RESULTS" "$WORK/src" "$WORK/out"

  TAB="$(printf '\t')"
  python3 - "$BENCH/repos.toml" <<'EOF' | while IFS="$TAB" read -r name source commit; do
try:
    import tomllib
except ImportError:
    import tomli as tomllib
import sys
with open(sys.argv[1], "rb") as f:
    for r in tomllib.load(f)["repo"]:
        print(f"{r['name']}\t{r['source']}\t{r['pinned_commit']}")
EOF
    [ -n "$ONLY" ] && [ "$ONLY" != "$name" ] && continue
    say "── $name ──"
    if [ "$commit" = "local" ]; then
      src="$ROOT/$source"
    else
      src="$WORK/src/$name"
      # Always re-clone remote repos. A prior run deleted .git after checkout,
      # so the guard cannot be "skip if dir exists" — the dir would exist without
      # .git and git-clone would refuse to clone into a non-empty directory.
      # Simplest correct behaviour: wipe any existing checkout and re-clone.
      # Trade-off: adds a network round-trip per run; acceptable for a benchmark
      # suite that is not meant to be run continuously.
      rm -rf "$src"
      # A clone failure aborts the whole suite (set -e): a partial RESULTS.md is worse than none.
      git clone --quiet "$source" "$src" >&2
      git -C "$src" fetch --quiet origin "$commit" 2>/dev/null || true
      git -C "$src" checkout --quiet "$commit"
      rm -rf "$src/.git"   # the analyzer must not see VCS internals
    fi
    out="$WORK/out/$name"
    rm -rf "$out"
    if "$ROOT/target/release/rustyfi" "$src" -o "$out" --fresh --json $DEEP \
        > "$RESULTS/$name.json" 2> "$RESULTS/$name.log"; then
      say "$name: clean"
    else
      code=$?
      if [ "$code" = "1" ]; then say "$name: partial (see $RESULTS/$name.json)"
      else say "$name: FAILED exit=$code (see $RESULTS/$name.log)"; echo "{\"crate_name\":\"$name\",\"pipeline_failed\":true,\"exit_code\":$code}" > "$RESULTS/$name.json"; fi
    fi
  done
}

say "aggregating → bench/RESULTS.md"
python3 "$BENCH/aggregate.py" "$BENCH/repos.toml" "$RESULTS" > "$BENCH/RESULTS.md"
say "done: bench/RESULTS.md"
