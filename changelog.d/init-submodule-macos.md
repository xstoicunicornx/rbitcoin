Fixed

- **`cargo test` fetches the Bitcoin Core corpus on macOS.**
  `scripts/core-functional/init-submodule.sh` locked with util-linux
  `flock`, which macOS does not ship, so the first consensus test in a
  fresh checkout failed and the rest reported a poisoned `Once`. The
  script now takes the same lock through Perl's `flock`.
