.PHONY: test test-cov

test:
	rustup run stable cargo test --all-features

test-cov:
	rustup run stable cargo llvm-cov --all-features \
		--fail-under-lines 93 \
		--fail-under-regions 93 \
		--fail-under-functions 93
