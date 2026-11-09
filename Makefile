.PHONY: test build validate
test:
	cargo test --locked
	node --test tests/test_storage_ui.js
build:
	docker compose build publisher
validate:
	docker compose config --quiet
