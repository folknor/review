#!/usr/bin/env bash
# Run `review config` in every sibling project that has a .review.toml, to
# check that existing files still parse under the current resolver.
#
# Usage: notes/config_sweep.sh [review-binary]
# Defaults to the release build under ./target.
set -u
here="$(cd "$(dirname "$0")/.." && pwd)"
bin="${1:-$here/target/release/review}"
failed=0
for cfg in "$here"/../*/.review.toml; do
    dir="$(dirname "$cfg")"
    name="$(basename "$dir")"
    if out="$(cd "$dir" && "$bin" config 2>&1)"; then
        echo "ok      $name"
    else
        echo "FAILED  $name"
        echo "$out" | sed 's/^/        /'
        failed=1
    fi
done
exit "$failed"
