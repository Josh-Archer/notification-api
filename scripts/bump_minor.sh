#!/usr/bin/env bash
set -e

# Get current version from Cargo.toml
CURRENT_VERSION=$(grep '^version =' Cargo.toml | head -n1 | sed 's/version = "//;s/"//')

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

