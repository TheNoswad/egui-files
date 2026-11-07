#!/bin/sh
# Shows how what we wait on upstream is doing (scripts/upstream.watch): the
# issues and pull requests, newer versions of the crates, whether our forks'
# branches need rebasing onto a newer release, new reports of
# problems we have that nobody had reported, and security advisories for
# what's in Cargo.lock. Marks with * what changed
# since the last run (remembered in target/upstream-seen).
#
#   scripts/upstream.sh
#
# Needs gh, signed in (for GitHub's API limits), git and curl. Only reads.
set -eu
cd "$(dirname "$0")/.."

seen=target/upstream-seen
mkdir -p target
touch "$seen"
next=$seen.next
: >"$next"
agent="egui-files-upstream-check"

command -v gh >/dev/null || {
    echo "upstream.sh: needs gh (GitHub's CLI)" >&2
    exit 1
}

# What a key was last time, and remembering what it is now.
was() {
    awk -v k="$1" '$1 == k { $1 = ""; sub(/^ /, ""); print; exit }' "$seen"
}
remember() {
    echo "$1 $2" >>"$next"
}

# " *" if a key's value changed since last time, else "  ".
mark() {
    old=$(was "$1")
    remember "$1" "$2"
    if [ "$old" = "$2" ]; then printf '  '; else printf ' *'; fi
}

watched() {
    grep -v '^#' scripts/upstream.watch | awk -v kind="$1" '$1 == kind'
}

why_of() {
    printf '%s\n' "$1" | sed 's/^[^|]*| *//'
}

echo "Issues and pull requests"
for kind in issue pr; do
    watched "$kind" | while IFS= read -r line; do
        ref=$(printf '%s\n' "$line" | awk '{ print $2 }')
        repo=${ref%#*}
        n=${ref#*#}
        if [ "$kind" = pr ]; then
            state=$(gh api "repos/$repo/pulls/$n" --jq \
                'if .merged_at then "merged" else .state end + " " + .updated_at[:10] + " " + .title') ||
                state="unknown (couldn't ask GitHub)"
        else
            state=$(gh api "repos/$repo/issues/$n" --jq \
                '.state + (if .state_reason == "completed" then " (done)" elif .state_reason then " (" + .state_reason + ")" else "" end) + " " + .updated_at[:10] + " " + .title') ||
                state="unknown (couldn't ask GitHub)"
        fi
        m=$(mark "$ref" "$(printf '%s\n' "$state" | awk '{ print $1, $2 }')")
        printf '%s %-5s %s %s\n' "$m" "$kind" "$ref" "$state"
        printf '         %s\n' "$(why_of "$line")"
    done
done

echo
echo "Crates (Cargo.lock → crates.io)"
watched crate | while IFS= read -r line; do
    name=$(printf '%s\n' "$line" | awk '{ print $2 }')
    ours=$(awk -v n="$name" '
        $0 == "[[package]]" { p = "" }
        $1 == "name" && $3 == "\"" n "\"" { p = n }
        $1 == "version" && p != "" { gsub(/"/, "", $3); printf "%s%s", sep, $3; sep = ", "; p = "" }
    ' Cargo.lock)
    latest=$(curl -fsSL -A "$agent" "https://crates.io/api/v1/crates/$name" |
        sed -n 's/.*"max_stable_version":"\([^"]*\)".*/\1/p') || latest=
    [ -n "$latest" ] || latest="unknown"
    case ", $ours," in
        *", $latest,"*) news="up to date" ;;
        *) news="$latest is out" ;;
    esac
    m=$(mark "crate:$name" "$latest")
    printf '%s %-20s %-10s %s\n' "$m" "$name" "${ours:-not in Cargo.lock}" "$news"
    printf '         %s\n' "$(why_of "$line")"
done

echo
echo "Our forks (Cargo.toml → the branch, and releases to rebase onto)"
# The file of a crate's versions in crates.io's index.
index_path() {
    n=$(printf '%s' "$1" | tr 'A-Z' 'a-z')
    case ${#n} in
        1) echo "1/$n" ;;
        2) echo "2/$n" ;;
        3) echo "3/$(printf '%s' "$n" | cut -c1)/$n" ;;
        *) echo "$(printf '%s' "$n" | cut -c1-2)/$(printf '%s' "$n" | cut -c3-4)/$n" ;;
    esac
}
watched fork | while IFS= read -r line; do
    repo=$(printf '%s\n' "$line" | awk '{ print $2 }')
    branch=$(printf '%s\n' "$line" | awk '{ print $3 }')
    crate=$(printf '%s\n' "$line" | awk '{ print $4 }')
    base=${branch##*/}
    url="https://github.com/$repo"
    revs=$(sed -n 's|.*git = "'"$url"'", rev = "\([0-9a-f]*\)".*|\1|p' Cargo.toml | sort -u)
    head=$(git ls-remote "$url" "refs/heads/$branch" 2>/dev/null | awk '{ print $1 }') || head=
    versions=$(curl -fsSL -A "$agent" "https://index.crates.io/$(index_path "$crate")" |
        sed -n '/"yanked":false/s/.*"vers":"\([^"]*\)".*/\1/p' | grep -v -- - | sort -V) || versions=
    newest=$(printf '%s\n' "$versions" | tail -n 1)
    # Releases that are drop-in for ours: same minor for 0.x, else same major.
    case $base in
        0.*) series=${base%.*} ;;
        *) series=${base%%.*} ;;
    esac
    in_series=$(printf '%s\n' "$versions" | grep "^$(printf '%s' "$series" | sed 's/\./\\./g')\." | tail -n 1)
    if [ -z "$revs" ]; then
        news="not in Cargo.toml's [patch.crates-io]"
    elif [ "$(printf '%s\n' "$revs" | wc -l)" -ne 1 ]; then
        news="Cargo.toml uses more than one commit of it"
    elif [ -z "$head" ]; then
        news="branch not found (couldn't reach GitHub?)"
    elif [ "$head" != "$revs" ]; then
        news="branch is at $(printf '%.10s' "$head"), Cargo.toml at $(printf '%.10s' "$revs"): update the rev"
    elif [ -z "$newest" ]; then
        news="couldn't ask crates.io for $crate's releases"
    elif [ -n "$in_series" ] && [ "$in_series" != "$base" ]; then
        news="$crate $in_series is out: rebase the branch onto it"
    elif [ "$newest" != "$base" ]; then
        news="$crate $newest is out: rebase onto it when the app moves to it"
    else
        news="on $crate's newest release"
    fi
    m=$(mark "fork:$repo" "$head $newest")
    printf '%s %-30s %-20s %s\n' "$m" "$repo" "$branch" "$news"
    printf '         %s\n' "$(why_of "$line")"
done

echo
echo "Not reported upstream (new matches only; may be unrelated)"
watched search | while IFS= read -r line; do
    repo=$(printf '%s\n' "$line" | awk '{ print $2 }')
    terms=$(printf '%s\n' "$line" | sed 's/^search *[^ ]* *//; s/ *|.*//')
    key="search:$repo:$(printf '%s' "$terms" | tr ' ' '+')"
    query=$(printf 'repo:%s %s' "$repo" "$terms")
    found=$(gh api -X GET search/issues -f q="$query" -f per_page=20 --jq \
        '.items[] | "\(.number)\t\(.state)\t\(.title)"') || {
        printf '   %s "%s": couldn'"'"'t search\n' "$repo" "$terms"
        continue
    }
    old=$(was "$key")
    numbers=$(printf '%s\n' "$found" | awk -F '\t' 'NF { print $1 }' | sort -n | tr '\n' ',')
    remember "$key" "$numbers"
    new=$(printf '%s\n' "$found" | while IFS="$(printf '\t')" read -r num st title; do
        [ -n "$num" ] || continue
        case ",$old" in *",$num,"*) ;; *) printf '           #%s %s %s\n' "$num" "$st" "$title" ;; esac
    done)
    count=$(printf '%s' "$numbers" | tr ',' '\n' | grep -c . || true)
    matches="$count matches"
    [ "$count" -ne 1 ] || matches="1 match"
    if [ -n "$new" ]; then
        printf ' * %s "%s": %s, new:\n%s\n' "$repo" "$terms" "$matches" "$new"
    else
        printf '   %s "%s": %s, none new\n' "$repo" "$terms" "$matches"
    fi
    printf '         %s\n' "$(why_of "$line")"
done

echo
echo "Security advisories (RustSec, deny.toml)"
if cargo deny --version >/dev/null 2>&1; then
    if report=$(cargo deny -L error check advisories --hide-inclusion-graph 2>&1); then
        echo "   none"
    else
        printf '%s\n' "$report" | sed 's/^/ * /'
    fi
else
    echo "   cargo-deny isn't installed (cargo install cargo-deny --locked)"
fi

mv "$next" "$seen"
