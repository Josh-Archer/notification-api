#!/usr/bin/env bash
set -e

# Always run from repo root
cd "$(git rev-parse --show-toplevel)"

# Guard against re-entrancy when this script pushes with --no-verify
if [ "${NOTIFICATION_API_SKIP_BUMP:-}" = "1" ]; then
  exit 0
fi

BRANCH=$(git rev-parse --abbrev-ref HEAD)

# Only bump on main so feature-branch / PR pushes never rewrite or pad history
if [ "$BRANCH" != "main" ]; then
  echo "Not on main (current branch: $BRANCH). Skipping version bump for PR-safe collaboration."
  exit 0
fi

git fetch origin --tags --force

# Get latest tag name and version (strip 'v')
LATEST_TAG_NAME=$(git tag --sort=-v:refname | grep '^v' | head -n1 || true)
LATEST_TAG_VERSION=$(echo "$LATEST_TAG_NAME" | sed 's/^v//')

# Get current version from Cargo.toml
CURRENT_VERSION=$(grep '^version =' Cargo.toml | head -n1 | sed 's/version = "//;s/"//')

# Get commit hash for latest tag
LATEST_TAG_COMMIT=""
if [ -n "$LATEST_TAG_NAME" ]; then
  LATEST_TAG_COMMIT=$(git rev-list -n 1 "$LATEST_TAG_NAME")
fi
CURRENT_COMMIT=$(git rev-parse HEAD)

# If latest tag matches Cargo.toml version AND points to current commit, skip bumping
if [ -n "$LATEST_TAG_VERSION" ] && [ "$LATEST_TAG_VERSION" = "$CURRENT_VERSION" ] && [ "$LATEST_TAG_COMMIT" = "$CURRENT_COMMIT" ]; then
  echo "Latest tag ($LATEST_TAG_NAME) matches Cargo.toml version ($CURRENT_VERSION) and points to current commit. Skipping version bump."
  exit 0
fi

# Bump minor version (e.g., 0.1.0 -> 0.2.0)
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_VERSION"
MINOR=$((MINOR + 1))
PATCH=0
NEW_VERSION="$MAJOR.$MINOR.$PATCH"

# Update Cargo.toml with new version
sed -i.bak "s/^version = \"$CURRENT_VERSION\"/version = \"$NEW_VERSION\"/" Cargo.toml
rm -f Cargo.toml.bak

echo "Bumped version: $CURRENT_VERSION -> $NEW_VERSION"

git add Cargo.toml

# Separate version commit (never amend — safe for shared history)
git commit -m "chore: bump version to $NEW_VERSION"

# Check if tag already exists on remote
if git ls-remote --tags origin | grep -q "refs/tags/v$NEW_VERSION"; then
  echo "Tag v$NEW_VERSION already exists on remote. Skipping tag creation."
else
  if git tag | grep -q "^v$NEW_VERSION$"; then
    echo "Tag v$NEW_VERSION already exists locally. Skipping tag creation."
  else
    git tag "v$NEW_VERSION"
  fi
fi

# pre-push has already fixed the OIDs for the original push, so a new commit
# would not be included. Push the updated tip (and tag) ourselves, then abort
# the original push so history stays linear and no amend/force is required.
export NOTIFICATION_API_SKIP_BUMP=1
git push --no-verify origin "HEAD:refs/heads/main"

if git rev-parse "v$NEW_VERSION" >/dev/null 2>&1; then
  if git ls-remote --tags origin | grep -q "refs/tags/v$NEW_VERSION"; then
    echo "Tag v$NEW_VERSION already on remote."
  else
    git push --no-verify origin "refs/tags/v$NEW_VERSION:refs/tags/v$NEW_VERSION"
  fi
fi

echo "Version $NEW_VERSION committed and pushed to main (separate commit, no amend)."
echo "Aborting the original push because it is already complete."
exit 1
