# isuride

Local ISUCON14 practice tree for the t5820 3+1 cluster.
Baseline is the official Go reference, scored on this homelab.

## Cluster

| Host | IP | Role |
|------|----|------|
| isucon14-1 | 192.168.0.210 | nginx + Go + MySQL (bench target) |
| isucon14-2 | 192.168.0.211 | same stack, for later split |
| isucon14-3 | 192.168.0.212 | same stack, for later split |
| isucon14-bench | 192.168.0.213 | official bench |

HTTPS `https://isuride.xiv.isucon.net` → `.210`. Language: Go.

## Baseline

- Commit: initial Go reference as deployed 2026-11-02
- Bench: `pass=true` score **2042** (60s), two CODE=26 `total_distance` lag warnings
- Command: `bash scripts/run-bench.sh`

## Layout

```
webapp/go/            Go app (running)
webapp/sql/           schema + init
webapp/public/        frontend
webapp/rust/          official Rust reference (not running)
webapp/payment_mock/  local payment mock on app VMs
nginx/                isuride.conf
systemd/              isuride-go + matcher
scripts/deploy.sh     rsync + rebuild + restart on the three app VMs
scripts/run-bench.sh  60s load from the bench VM
```

## Deploy

```bash
bash scripts/deploy.sh
bash scripts/run-bench.sh
```

Keep t5820 VM 110/111 stopped while benching.
