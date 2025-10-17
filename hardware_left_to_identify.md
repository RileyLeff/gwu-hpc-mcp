# Pegasus Hardware Metadata: Outstanding Items

The current MCP configuration captures the best public information available, but a few details still need confirmation:

- **`small-gpu` / `debug-gpu` partitions**: confirm per-GPU memory size (documentation lists Tesla V100 but omits whether nodes are 16 GB or 32 GB cards).
- **`med-gpu` partition**: identify the exact hardware backing this queue (likely newer accelerator nodes—clarify GPU model, count, and memory).
- **`highCore` partition**: verify the CPU model / core count that differentiates these nodes from the standard 6148 compute nodes.
- **`viz` partition**: document the GPU model and memory available on the visualization node(s).
- **Grace Hopper nodes**: confirm final production specs for the `superChip` queue (current notes reflect the published hardware sheet but should be validated with HPC staff).
- **Node-local storage sizes**: double-check actual `/tmp` or additional local NVMe capacity per partition in case of deviations from the published "800 GB SSD" baseline.

Update the configuration once these items are confirmed so `hpc_describe` can show the fully accurate hardware profile.
