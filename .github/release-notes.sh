#!/usr/bin/env bash
# Release notes for a version tag, from the commit messages.
#
#   .github/release-notes.sh v0.1.4   > notes.md
#
# - Patch release (vX.Y.Z, Z > 0): just the changes since the previous tag.
# - Minor / major release (vX.Y.0): everything since the previous X.Y.0
#   release, newest version first, so it reads as the full update.
#
# Needs the full history and tags (actions/checkout with fetch-depth: 0).
set -euo pipefail

tag="$1"
mapfile -t tags < <(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' --sort=v:refname)

index=-1
for i in "${!tags[@]}"; do
    [[ "${tags[$i]}" == "$tag" ]] && index=$i
done
if (( index < 0 )); then
    echo "Unknown tag $tag" >&2
    exit 1
fi

# One commit: its subject as a heading (minus "; release X.Y.Z") and its
# body without the Co-Authored-By trailer.
print_commit() {
    local subject body
    subject=$(git log -1 --format='%s' "$1" | sed -E 's/;? *[Rr]elease [0-9]+\.[0-9]+\.[0-9]+$//')
    body=$(git log -1 --format='%b' "$1" | grep -v -i '^co-authored-by:' | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}')
    echo "### $subject"
    [[ -n "$body" ]] && { echo; echo "$body"; }
    echo
}

# Commits in (from, to], oldest first. Empty `from` = from the beginning.
print_range() {
    local range="$2"
    [[ -n "$1" ]] && range="$1..$2"
    git log --reverse --format='%H' "$range" | while read -r commit; do
        print_commit "$commit"
    done
}

version="${tag#v}"
patch="${version##*.}"
previous=""
(( index > 0 )) && previous="${tags[$((index - 1))]}"

if [[ "$patch" != "0" ]]; then
    print_range "$previous" "$tag"
    [[ -n "$previous" ]] && echo "**Changes since $previous:** https://github.com/${GITHUB_REPOSITORY:-birdguy-cs/bman}/compare/$previous...$tag"
    exit 0
fi

# Minor/major: roll up every release since the previous X.Y.0.
base=""
for (( i = index - 1; i >= 0; i-- )); do
    if [[ "${tags[$i]##*.}" == "0" ]]; then
        base="${tags[$i]}"
        break
    fi
done

if [[ -z "$base" ]]; then
    echo "First release: everything so far."
    echo
    print_range "" "$tag"
    exit 0
fi

echo "Everything since $base, newest first."
echo
for (( i = index; i > 0; i-- )); do
    [[ "${tags[$i]}" == "$base" ]] && break
    from="${tags[$((i - 1))]}"
    echo "## ${tags[$i]}"
    echo
    print_range "$from" "${tags[$i]}"
    [[ "$from" == "$base" ]] && break
done
echo "**Changes since $base:** https://github.com/${GITHUB_REPOSITORY:-birdguy-cs/bman}/compare/$base...$tag"
