# abtop — build & install helpers
#
# Override the abtop installed on this system with a from-source build of this
# fork. abtop's official installers (shell/powershell) and `cargo install abtop`
# all place the binary in CARGO_HOME (~/.cargo/bin) — see dist-workspace.toml —
# so `cargo install --path . --force` reliably replaces whatever is on PATH.
#
# Quick start:
#   make install          # build + override the installed abtop
#   make which            # confirm which abtop is on PATH and its version
#   make restore-release  # undo the override, reinstall the published version
#
# Windows: Make isn't native there. Run the documented override directly:
#   cargo install --path . --force

CARGO ?= cargo
BIN := abtop

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

.PHONY: build
build: ## Build the release binary (target/release/abtop)
	$(CARGO) build --release

.PHONY: install
install: ## Override the installed abtop on PATH with this source build
	$(CARGO) install --path . --force
	@echo "Installed. Run 'make which' to confirm the swap."

.PHONY: reinstall
reinstall: build install ## Rebuild, then override (build + install)

.PHONY: uninstall
uninstall: ## Remove the cargo-installed abtop (~/.cargo/bin/abtop)
	$(CARGO) uninstall $(BIN)

.PHONY: restore-release
restore-release: ## Undo the override: reinstall the published abtop from crates.io
	$(CARGO) install $(BIN) --force

.PHONY: which
which: ## Show which abtop is on PATH and its version
	@command -v $(BIN) && $(BIN) --version

.PHONY: fmt
fmt: ## Check formatting (cargo fmt --check)
	$(CARGO) fmt --check

.PHONY: clippy
clippy: ## Lint with clippy (--all-targets)
	$(CARGO) clippy --all-targets

.PHONY: test
test: ## Run the full test suite
	$(CARGO) test

.PHONY: check
check: fmt clippy test ## Run fmt + clippy + test (CI parity)
