#!/usr/bin/env bash
# Shared commit classification for canonical and legacy release-note adapters.
# Sourcing this file only defines functions.

parse_commit_type() {
	local message="$1"
	# Extract type from conventional commit format: type(scope): message or type: message
	# Use sed to extract the type prefix
	local type
	type=$(echo "$message" | sed -E 's/^([a-z]+)(\([^)]+\))?:.*/\1/')
	if [[ "$type" == "$message" ]]; then
		echo "other"
	else
		echo "$type"
	fi
}

# Return success for release bookkeeping and owner-only tracker commit subjects.
release_commit_is_excluded() {
	local message="$1"
	local lower_msg
	lower_msg=$(echo "$message" | tr '[:upper:]' '[:lower:]')
	[[ "$lower_msg" =~ (chore\(release\):|bump version|update version|version bump|release v[0-9]+\.[0-9]+\.[0-9]+|chore.*version|chore.*release|build.*version|update.*version.*number|bump.*version.*to|update homebrew|update changelog|update.*todo|docs\(todo\)|docs\(project\).*todo|^update project$) ]]
}
