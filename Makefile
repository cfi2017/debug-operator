.PHONY: test crd docker-build

IMAGE ?= ghcr.io/cfi2017/debug-operator:latest

test:
	cargo test

crd:
	cargo run --quiet --bin crdgen

docker-build:
	docker build -t $(IMAGE) .
