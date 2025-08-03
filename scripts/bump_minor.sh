#!/usr/bin/env bash
set -e

# Always run from repo root
cd "$(git rev-parse --show-toplevel)"

git fetch --tags

# Get latest tag version (strip 'v')
LATEST_TAG=$(git tag --sort=-v:refname | grep '^v' | head -n1 | sed 's/^v//')

# Get current version from Cargo.toml
CURRENT_VERSION=$(grep '^version =' Cargo.toml | head -n1 | sed 's/version = "//;s/"//')

# If latest tag matches Cargo.toml version, skip bumping
if [ "$LATEST_TAG" = "$CURRENT_VERSION" ]; then
  echo "Latest tag ($LATEST_TAG) matches Cargo.toml version ($CURRENT_VERSION). Skipping version bump."
  exit 0
fi

# Check if current commit is already tagged
if git tag --points-at HEAD | grep -q '^v'; then
  echo "Current commit is already tagged. Skipping version bump."
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

git tag v$NEW_VERSION
