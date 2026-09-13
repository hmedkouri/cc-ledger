BIN_DIR ?= $(HOME)/.claude/bin
# cc-usage is typed by hand, so it needs to be on PATH. statusline never is -
# Claude Code invokes it by the absolute path stored in settings.json.
LINK_DIR ?= $(HOME)/.local/bin
SETTINGS ?= $(HOME)/.claude/settings.json
UNAME := $(shell uname -s)

.PHONY: all build test lint install install-bins statusline-diff statusline-apply backfill clean

all: lint test

build:
	cargo build --release

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

## Install binaries, then show what would change in settings.json.
## Nothing touches settings.json unless you run `make statusline-apply`.
install: install-bins statusline-diff
	@echo
	@echo "Binaries installed. To point Claude Code at the new status line:"
	@echo "    make statusline-apply"

install-bins: build
	@mkdir -p "$(BIN_DIR)"
	@install -m 0755 target/release/statusline "$(BIN_DIR)/statusline"
	@install -m 0755 target/release/cc-usage  "$(BIN_DIR)/cc-usage"
	@echo "Installed statusline and cc-usage to $(BIN_DIR)"
	@mkdir -p "$(LINK_DIR)"
	@ln -sf "$(BIN_DIR)/cc-usage" "$(LINK_DIR)/cc-usage"
	@echo "Linked cc-usage into $(LINK_DIR)"
	@case ":$$PATH:" in \
		*":$(LINK_DIR):"*) ;; \
		*) echo "WARNING: $(LINK_DIR) is not on your PATH - add it, or run cc-usage as $(BIN_DIR)/cc-usage" ;; \
	esac
ifeq ($(UNAME),Darwin)
	@echo "Signing for macOS Gatekeeper"
	@codesign --force --sign - "$(BIN_DIR)/statusline" 2>/dev/null || \
		echo "codesign unavailable; skipping (binaries still run)"
	@codesign --force --sign - "$(BIN_DIR)/cc-usage" 2>/dev/null || true
endif

## Print the exact diff that statusline-apply would write. Read-only.
statusline-diff:
	@command -v jq >/dev/null || { echo "jq is required to edit settings.json safely"; exit 1; }
	@test -f "$(SETTINGS)" || { echo "No $(SETTINGS); nothing to diff"; exit 0; }
	@jq '.statusLine = {"type":"command","command":"$(BIN_DIR)/statusline"}' \
		"$(SETTINGS)" > "$(SETTINGS).proposed"
	@echo "--- proposed change to $(SETTINGS) ---"
	@diff -u "$(SETTINGS)" "$(SETTINGS).proposed" || true
	@rm -f "$(SETTINGS).proposed"

## Apply it. Every other key in settings.json is preserved; a backup is kept.
statusline-apply:
	@command -v jq >/dev/null || { echo "jq is required"; exit 1; }
	@cp "$(SETTINGS)" "$(SETTINGS).bak.$$(date +%Y%m%d%H%M%S)"
	@jq '.statusLine = {"type":"command","command":"$(BIN_DIR)/statusline"}' \
		"$(SETTINGS)" > "$(SETTINGS).tmp"
	@jq -e . "$(SETTINGS).tmp" >/dev/null || { echo "refusing to write invalid JSON"; exit 1; }
	@mv "$(SETTINGS).tmp" "$(SETTINGS)"
	@echo "statusLine now points at $(BIN_DIR)/statusline (backup kept)"

## One-off: read every transcript currently on disk into the ledger.
backfill: install-bins
	@"$(BIN_DIR)/cc-usage" backfill

clean:
	cargo clean
