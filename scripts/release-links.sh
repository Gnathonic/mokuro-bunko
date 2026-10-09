#!/bin/sh
# Check the download links in a GitHub release's notes (the "Which file do I want?"
# table that `xtask release-notes` writes): every `releases/download/<tag>/<file>` link
# must name a file of that release, and fetching it must answer 200 or 302.
#
#   scripts/release-links.sh v0.7.0-beta.3            # a draft: its files, through the API
#   scripts/release-links.sh v0.7.0-beta.3 --public   # published: the links as people click them
#
# Needs `gh` (GH_TOKEN) and curl. A draft's files are not on the public download URL
# yet, so before publishing each one is fetched through the API (with the token);
# after publishing, the links themselves are fetched without one.
set -eu
TAG=${1:?usage: release-links.sh <tag> [--public]}
MODE=${2:-}
REPO=${GITHUB_REPOSITORY:-Gnathonic/mokuro-bunko}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

gh release view "$TAG" -R "$REPO" --json body --jq .body >"$tmp/body"
gh api "repos/$REPO/releases/tags/$TAG" --jq '.assets[] | [.name, .state, .url] | @tsv' >"$tmp/assets" 2>/dev/null ||
	gh api "repos/$REPO/releases?per_page=30" --jq ".[] | select(.tag_name==\"$TAG\") | .assets[] | [.name, .state, .url] | @tsv" >"$tmp/assets"
prefix="https://github.com/$REPO/releases/download/$TAG/"
grep -o "${prefix}[^) ]*" "$tmp/body" | sort -u >"$tmp/links" || true
n=$(wc -l <"$tmp/links" | tr -d ' ')
[ "$n" -gt 0 ] || {
	echo "no download links in the notes of $TAG" >&2
	exit 1
}
bad=0
while read -r link; do
	file=${link#"$prefix"}
	row=$(awk -F '\t' -v f="$file" '$1 == f' "$tmp/assets")
	if [ -z "$row" ]; then
		echo "FAIL $file: linked in the notes, not a file of $TAG"
		bad=1
		continue
	fi
	state=$(printf '%s' "$row" | cut -f2)
	api=$(printf '%s' "$row" | cut -f3)
	if [ "$MODE" = --public ]; then
		code=$(curl -s -o /dev/null -I -w '%{http_code}' "$link")
	else
		code=$(curl -s -o /dev/null -I -w '%{http_code}' -H "Authorization: Bearer ${GH_TOKEN:-$(gh auth token)}" \
			-H 'Accept: application/octet-stream' "$api")
	fi
	case "$code" in
	200 | 302) echo "OK   $code $file ($state)" ;;
	*)
		echo "FAIL $code $file ($state)"
		bad=1
		;;
	esac
done <"$tmp/links"
exit "$bad"
