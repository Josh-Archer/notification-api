#!/usr/bin/env bash
set -e

# Always run from repo root
cd "$(git rev-parse --show-toplevel)"

git fetch origin --tags --force

# Get latest tag name and version (strip 'v')
LATEST_TAG_NAME=$(git tag --sort=-v:refname | grep '^v' | head -n1)
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
if [ "$LATEST_TAG_VERSION" = "$CURRENT_VERSION" ] && [ "$LATEST_TAG_COMMIT" = "$CURRENT_COMMIT" ]; then
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
rm Cargo.toml.bak

echo "Bumped version: $CURRENT_VERSION -> $NEW_VERSION"

git add Cargo.toml
# Amend last commit to include version bump
GIT_EDITOR=true git commit --amend --no-edit

# Check if tag already exists on remote
if git ls-remote --tags origin | grep -q "refs/tags/v$NEW_VERSION"; then
  echo "Tag v$NEW_VERSION already exists on remote. Skipping tag creation and push."
  exit 0
fi

# Check if tag exists locally before creating
if git tag | grep -q "^v$NEW_VERSION$"; then
  echo "Tag v$NEW_VERSION already exists locally. Skipping tag creation."
else
  git tag v$NEW_VERSION
fi

git push origin "refs/tags/v$NEW_VERSION:refs/tags/v$NEW_VERSION"
