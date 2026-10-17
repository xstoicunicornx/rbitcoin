Fixed

- **After a restart, header replies continue a stored queue that reaches far above the tip.** The header store finds a new header's parent height by walking back at most 10,000 stored headers to one it knows. After a restart it knows only connected blocks, so with the stored queue tens of thousands of blocks above the tip every reply that continued it failed with `header parent height unknown` and was dropped while the queue drained. IBD now hands the store the parent's height from its own path.
