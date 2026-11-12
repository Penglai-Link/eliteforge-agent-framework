.DEFAULT_GOAL := help
NODE ?= node
VERSION ?=
SKIP_TESTS ?= false
export CAP_VERSION = $(VERSION)
export CAP_SKIP_TESTS = $(SKIP_TESTS)

.PHONY: help doctor install clean format lint test build dev deploy
help doctor install clean format lint test build dev deploy:
	$(NODE) scripts/project.mjs $@
