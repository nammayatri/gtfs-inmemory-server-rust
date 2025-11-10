#!/bin/bash

# Development setup script for GTFS In Memory Service
# This script sets up pre-commit hooks and other development tools

set -e

echo "🚀 Setting up development environment..."

# Check if pre-commit is installed
if ! command -v pre-commit &> /dev/null; then
    echo "📦 Installing pre-commit..."

    # Try different installation methods
    if command -v pip3 &> /dev/null; then
        pip3 install pre-commit
    elif command -v pip &> /dev/null; then
        pip install pre-commit
    elif command -v brew &> /dev/null; then
        brew install pre-commit
    else
        echo "❌ Could not install pre-commit automatically."
        echo "Please install it manually:"
        echo "  - pip install pre-commit"
        echo "  - brew install pre-commit"
        echo "  - conda install -c conda-forge pre-commit"
        exit 1
    fi
else
    echo "✅ pre-commit is already installed"
fi

# Install pre-commit hooks
echo "🔧 Installing pre-commit hooks..."
pre-commit install

# Run pre-commit on all files to ensure everything is set up correctly
echo "🔍 Running pre-commit on all files..."
pre-commit run --all-files

echo "✅ Development environment setup complete!"
echo ""
echo "Pre-commit hooks are now active and will run on every commit."
echo "The hooks include:"
echo "  - Code formatting (rustfmt)"
echo "  - Compilation checks (cargo check)"
echo "  - Secret detection (gitleaks)"
echo "  - File quality checks"
