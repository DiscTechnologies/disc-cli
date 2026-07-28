.PHONY: test test-cov

test:
	rustup run stable cargo test --all-features

test-cov:
	rustup run stable cargo llvm-cov --all-features \
		--fail-under-lines 90 \
		--fail-under-regions 90 \
		--fail-under-functions 90
