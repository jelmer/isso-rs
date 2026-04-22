# Top-level Makefile for isso.
#
# Project layout:
#   src/                    — Rust crate sources
#   templates/              — Jinja templates (admin UI)
#   static/                 — JS/CSS/img/demo tree served at /js, /css, /img, /demo
#   static/js/              — JS frontend sources; webpack-built bundles land
#                              alongside and are served by isso at /js/
#   docs/                   — Sphinx docs (+ docs/porting-reference.md)
#   apidoc/                 — apiDoc configuration
#
# Install dependencies:
#   JS frontend:    make init
#   Docs:           pip install sphinx && apt install sassc
#   Rust:           cargo (stable)

ISSO_JS_SRC := $(shell find static/js/app -type f 2>/dev/null) \
	       $(shell ls static/js/*.js 2>/dev/null | grep -vE "(min|dev)")

ISSO_JS_DST := static/js/embed.min.js static/js/embed.dev.js \
	       static/js/count.min.js static/js/count.dev.js \
	       static/js/count.dev.js.map static/js/embed.dev.js.map

DOCS_RST_SRC := $(shell find docs/ -type f -name '*.rst') \
		$(wildcard docs/_theme/*) \
		docs/index.html docs/conf.py docs/docutils.conf \
		$(shell find docs/_extensions/)

DOCS_CSS_SRC := docs/_static/css/site.scss

DOCS_CSS_DEP := $(shell find docs/_static/css/neat -type f) \
		$(shell find docs/_static/css/bourbon -type f)

DOCS_CSS_DST := docs/_static/css/site.css

DOCS_HTML_DST := docs/_build/html

APIDOC_SRC := apidoc/apidoc.json apidoc/header.md apidoc/footer.md apidoc/_apidoc.js

APIDOC_DST := apidoc/_output

APIDOC = npx --no-install apidoc

SASS = sassc

ISSO_IMAGE ?= isso:latest
ISSO_RELEASE_IMAGE ?= isso:release
ISSO_DOCKER_REGISTRY ?= ghcr.io/isso-comments
TESTBED_IMAGE ?= isso-js-testbed:latest

all: build js site

# --------------------------------------------------------------------- Rust
build:
	cargo build --release

test-rust:
	cargo test

lint-rust:
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

# --------------------------------------------------------------------- JS
init:
	npm install --omit=optional

# Note: It doesn't make sense to split up configs by output file with
# webpack, just run everything at once
static/js/embed.min.js: $(ISSO_JS_SRC)
	npm run build-prod

static/js/count.min.js: static/js/embed.min.js

static/js/embed.dev.js: $(ISSO_JS_SRC)
	npm run build-dev

static/js/count.dev.js: static/js/embed.dev.js

js: $(ISSO_JS_DST)

# --------------------------------------------------------------------- Docs
css: $(DOCS_CSS_DST)

${DOCS_CSS_DST}: $(DOCS_CSS_SRC) $(DOCS_CSS_DEP)
	$(SASS) $(DOCS_CSS_SRC) $@

${DOCS_HTML_DST}: $(DOCS_RST_SRC) $(DOCS_CSS_DST)
	sphinx-build -b dirhtml -W docs/ $@

site: $(DOCS_HTML_DST)

apidoc-init:
	npm install apidoc

# apiDoc pulls annotations from apidoc/_apidoc.js. Before the Rust port the
# majority lived in docstrings inside isso/views/comments.py and apidoc read
# both sources; after the port we consolidated everything into _apidoc.js so
# apidoc doesn't need to parse Rust.
apidoc: $(APIDOC_SRC)
	$(APIDOC) --config apidoc/apidoc.json \
		--input apidoc/ \
		--output $(APIDOC_DST) --private
	cp -rT $(APIDOC_DST) $(DOCS_HTML_DST)/docs/api/

# --------------------------------------------------------------------- Docker
docker:
	DOCKER_BUILDKIT=1 docker build -t $(ISSO_IMAGE) .

docker-release:
	DOCKER_BUILDKIT=1 docker build -t $(ISSO_IMAGE) .

docker-run:
	docker run -d --rm --name isso -p 127.0.0.1:8080:8080 \
		--mount type=bind,source=$(PWD)/contrib/isso-dev.cfg,target=/config/isso.cfg,readonly \
		$(ISSO_IMAGE)

docker-push:
	docker tag $(ISSO_IMAGE) $(ISSO_DOCKER_REGISTRY)/$(ISSO_IMAGE)
	docker push $(ISSO_DOCKER_REGISTRY)/$(ISSO_IMAGE)

docker-release-push:
	docker tag $(ISSO_RELEASE_IMAGE) $(ISSO_DOCKER_REGISTRY)/$(ISSO_RELEASE_IMAGE)
	docker push $(ISSO_DOCKER_REGISTRY)/$(ISSO_RELEASE_IMAGE)

docker-testbed:
	DOCKER_BUILDKIT=1 docker build -f docker/Dockerfile-js-testbed -t $(TESTBED_IMAGE) .

docker-testbed-push:
	docker tag $(TESTBED_IMAGE) $(ISSO_DOCKER_REGISTRY)/$(TESTBED_IMAGE)
	docker push $(ISSO_DOCKER_REGISTRY)/$(TESTBED_IMAGE)

docker-js-unit:
	docker run \
		--mount type=bind,source=$(PWD)/package.json,target=/src/package.json,readonly \
		--mount type=bind,source=$(PWD)/static/js/,target=/src/isso/js/,readonly \
		$(TESTBED_IMAGE) npm run test-unit

docker-js-integration:
	docker run \
		--mount type=bind,source=$(PWD)/package.json,target=/src/package.json,readonly \
		--mount type=bind,source=$(PWD)/static/js/,target=/src/isso/js/ \
		--env ISSO_ENDPOINT='http://isso-dev.local:8080' \
		--network container:isso-server \
		$(TESTBED_IMAGE) npm run test-integration

clean:
	rm -f $(ISSO_JS_DST)
	rm -rf $(DOCS_HTML_DST)
	rm -rf $(APIDOC_DST)
	cargo clean

.PHONY: all apidoc apidoc-init build clean docker docker-js-integration docker-js-unit docker-push docker-release docker-release-push docker-run docker-testbed docker-testbed-push init js lint-rust site test-rust
