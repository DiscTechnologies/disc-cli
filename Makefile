.PHONY: test test-artifacts test-cov

test:
	rustup run stable cargo test --all-features
	python3 -m unittest scripts/test_release_artifacts.py

test-artifacts:
	python3 -m unittest scripts/test_release_artifacts.py

test-cov:
	rustup run stable cargo llvm-cov --all-features \
		--fail-under-lines 93 \
		--fail-under-regions 93 \
		--fail-under-functions 93
