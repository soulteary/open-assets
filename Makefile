.PHONY: test build validate
test:
	python -m unittest discover -s tests -v
build:
	docker compose build publisher
validate:
	docker compose config --quiet
