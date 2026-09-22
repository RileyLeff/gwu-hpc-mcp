# Pegasus metadata maintenance

The example config and config initializer share one template. Partition names,
limits, and advertised GPU types were checked against live `sinfo` and `scontrol`
on 2026-09-22. CPU/GPU partitions contain heterogeneous nodes; do not assign a
single CPU model, RAM size, or GPU count to an entire partition.

Before refreshing hardware metadata, inspect live Slurm node resources and confirm
site policy. The legacy `lustre_root` config key is retained for compatibility;
`/scratch` currently resolves under GPFS. Confirm storage retention and account
eligibility with HPC staff rather than deriving them from filesystem names.
