#!/usr/bin/env bash
# Fixture-only changelog checks; never invoke release publication.
set -euo pipefail

repo_scripts="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture_dir=$(mktemp -d)
trap 'rm -rf "$fixture_dir"' EXIT
checks=0

assert_equal() {
	local actual=$1 expected=$2 label=$3
	if [[ "$actual" != "$expected" ]]; then
		printf 'FAIL: %s\nExpected:\n%s\nActual:\n%s\n' "$label" "$expected" "$actual" >&2
		exit 1
	fi
	checks=$((checks + 1))
}

forbidden_publication() {
	printf 'unexpected publication command\n' >>"$fixture_dir/publication-calls"
	return 99
}
cargo() { forbidden_publication; }
gh() { forbidden_publication; }
curl() { forbidden_publication; }
export -f cargo gh curl forbidden_publication
export fixture_dir

original_trap=$(trap -p EXIT)
# shellcheck disable=SC1091
source "$repo_scripts/release-changelog.sh"
assert_equal "$(trap -p EXIT)" "$original_trap" "library keeps caller cleanup trap"
assert_equal "$(parse_commit_type 'feat(core): new behavior')" feat "scoped conventional commit"
assert_equal "$(parse_commit_type 'fix: repair')" fix "unscoped conventional commit"
assert_equal "$(parse_commit_type 'Unstructured subject')" other "unstructured commit"
assert_equal "$(clean_commit_message 'feat(core):  keep [skip ci]')" 'keep [skip ci]' "canonical CI marker contract"
assert_equal "$(get_github_username '123+alice@users.noreply.github.com')" alice "GitHub ID email"
assert_equal "$(get_github_username 'noreply@vtcode.com')" vtcode-release-bot "release bot identity"
assert_equal "$(get_github_username 'vinhnguyen.fixture@example.test')" vinhnx "maintainer alias"

for subject in 'chore(release): v1.2.3' 'UPDATE TODOs' 'docs(todo): owner notes' 'Build: update version number'; do
	if ! release_commit_is_excluded "$subject"; then
		printf 'FAIL: expected excluded subject: %s\n' "$subject" >&2
		exit 1
	fi
	checks=$((checks + 1))
done
for subject in 'feat: new behavior' 'fix: preserve version parsing' 'docs: user workflow'; do
	if release_commit_is_excluded "$subject"; then
		printf 'FAIL: unexpectedly excluded subject: %s\n' "$subject" >&2
		exit 1
	fi
	checks=$((checks + 1))
done

mkdir "$fixture_dir/repository"
cd "$fixture_dir/repository"
command git init --quiet
command git config core.abbrev 7
command git config core.hooksPath /dev/null
command git config commit.gpgsign false
command git config user.name 'Fixture Author'
command git config user.email 'fixture@example.test'

commit_fixture() {
	command git -c "user.email=$2" commit --quiet --allow-empty -m "$1"
}

commit_fixture 'feat(core): oldest feature' '11+oldest@users.noreply.github.com'
oldest_hash=$(command git rev-parse --short=7 HEAD)
expected=$(
	cat <<EOF
### Highlights

#### Features

- oldest feature ($oldest_hash) (@oldest)

### Contributors

@oldest
EOF
)
assert_equal "$(generate_structured_changelog HEAD)" "$expected" "single commit without trailing git-log newline"
assert_equal "$(add_username_tags "- oldest feature ($oldest_hash)" HEAD)" \
	"- oldest feature ($oldest_hash) (@oldest)" "oldest author mapping without trailing newline"

commit_fixture 'fix(io): second fix' 'vinhnguyen.fixture@example.test'
fix_hash=$(command git rev-parse --short=7 HEAD)
commit_fixture 'docs: third guide' 'noreply@vtcode.com'
docs_hash=$(command git rev-parse --short=7 HEAD)
commit_fixture 'perf: fourth optimization' '11+oldest@users.noreply.github.com'
perf_hash=$(command git rev-parse --short=7 HEAD)
commit_fixture 'docs(todo): owner bookkeeping' 'noreply@vtcode.com'
commit_fixture 'unstructured sixth change' '12+newest@users.noreply.github.com'
other_hash=$(command git rev-parse --short=7 HEAD)
commit_fixture 'feat(tui): newest feature' 'vinhnguyen.fixture@example.test'
newest_hash=$(command git rev-parse --short=7 HEAD)
original_head=$(command git rev-parse HEAD)
expected=$(
	cat <<EOF
### Highlights

#### Features

- newest feature ($newest_hash) (@vinhnx)
- oldest feature ($oldest_hash) (@oldest)

#### Bug Fixes

- second fix ($fix_hash) (@vinhnx)

#### Documentation

- third guide ($docs_hash)

### Other Changes

#### Performance

- fourth optimization ($perf_hash) (@oldest)

#### Other

- unstructured sixth change ($other_hash) (@newest)

### Contributors

@vinhnx, @newest, @oldest
EOF
)
assert_equal "$(generate_structured_changelog HEAD)" "$expected" "group order, history order, aliases and deduplication"
assert_equal "$(generate_structured_changelog HEAD..HEAD)" '*No significant changes*' "empty range"

export SCRIPT_DIR=$repo_scripts
legacy_contract=$(
	# shellcheck disable=SC1091
	source "$repo_scripts/release-lib.sh"
	printf '%s\n' "$(parse_commit_type 'fix(io): repair')" \
		"$(get_github_username '12+newest@users.noreply.github.com')" \
		"$(clean_commit_message 'fix: preserve legacy [skip ci]')" \
		"$(get_type_title feat)"
)
assert_equal "$legacy_contract" $'fix\n12+newest\npreserve legacy\nNew Features' "legacy adapter keeps distinct contracts"
legacy_notes=$(
	# shellcheck disable=SC1091
	source "$repo_scripts/release-lib.sh"
	generate_structured_changelog HEAD
)
if [[ "$legacy_notes" != *'### New Features'* || "$legacy_notes" != *"oldest feature ($oldest_hash)"* ||
	"$legacy_notes" == *'owner bookkeeping'* || "$legacy_notes" == *'### Highlights'* ]]; then
	printf 'FAIL: legacy formatter wiring or exclusion policy\n%s\n' "$legacy_notes" >&2
	exit 1
fi
checks=$((checks + 1))

# --help exits in argument parsing, before build/version/tag/upload operations.
bash "$repo_scripts/release.sh" --help >"$fixture_dir/help.txt"
if ! grep -q '^Usage: ./scripts/release.sh' "$fixture_dir/help.txt"; then
	printf 'FAIL: entrypoint help wiring\n' >&2
	exit 1
fi
checks=$((checks + 1))
assert_equal "$(command git rev-parse HEAD)" "$original_head" "formatter leaves HEAD unchanged"
assert_equal "$(command git status --porcelain)" '' "formatter leaves worktree unchanged"
assert_equal "$(command git tag -l)" '' "formatter creates no tags"
if [[ -e "$fixture_dir/publication-calls" ]]; then
	printf 'FAIL: publication was attempted\n' >&2
	exit 1
fi
checks=$((checks + 1))
printf 'PASS: %s changelog checks\n' "$checks"
