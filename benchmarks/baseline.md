# Measurements

Every stage records its numbers here. The point is the **delta**, not the
absolute value: stage 1 is the engine floor, and everything after it is
measured against that.

## Stage 1 — bar only (2026-10-12, measured)

| | |
|---|---|
| RSS | 124 M |
| **PSS** | **79 M** |
| **Private dirty** | **34 M** |
| Threads | 30 |
| fds | 62, stable over 60 s |
| Children | 0 |
| Idle CPU (30 s) | 0.00% |
| Binary | 24.7 MB |
| Surface | `xywh: 0 43 1920 32`, namespace `lume-bar` |

### Read these numbers carefully

My prediction was 55–80 MB RSS against Ryoku's 140 MB, on the theory that
Qt/QML/JS engine overhead is most of Ryoku's cost and we do not pay it. That was
wrong. Measured on the same machine, both idle:

| | RSS | PSS | Private dirty | Threads | Surfaces |
|---|---|---|---|---|---|
| **lume** (1 bar) | 124 M | 79 M | 34 M | 30 | 1 |
| **ryoku** (quickshell) | 131 M | 50 M | 36 M | 15 | 10 |

- RSS is marginally better, not half.
- **PSS is worse.** PSS divides shared pages fairly and is the honest figure for
  what a process costs the system.
- **Private dirty is a tie**, and that is the fairest measure of application
  memory.

GPUI's floor and Qt's floor are about the same. Where lume will win is features:
one bar against ten surfaces and ~190 kLoC of QML. Do not plan against a memory
budget we have not earned — measure each stage.

### Where the RSS goes

`/proc/<pid>/smaps`, sorted:

| Mapping | RSS | Note |
|---|---|---|
| `libnvidia-gpucomp.so` | 27.9 M | NVIDIA GPU compiler, **loaded but unused** |
| `[heap]` | 20.9 M | the actual Rust heap |
| `lume` | 18.7 M | our code pages |
| `libLLVM.so.23.1` | 15.8 M | naga compiling WGSL at startup |
| `libvulkan_intel.so` | 10.4 M | the driver we use |
| `libnvidia-glcore.so` | 9.2 M | unused |
| `libnvidia-glvkspirv.so` | 2.9 M | unused |

~40 MB is NVIDIA libraries for a GPU we never render on. ~15.8 MB is LLVM.
Neither is inherent to GPUI; both are potential future wins, and neither is worth
chasing until five surfaces exist.

RSS over two minutes went 127.2 M → 123.4 M. The allocator settling, not a leak.

## Stage 2 — compositor added (2026-10-12)

| | Stage 1 | Stage 2 | Delta |
|---|---|---|---|
| RSS | 124 M | 115 M | -9 M (allocator variance) |
| PSS | 79 M | 92 M | +13 M |
| **Private dirty** | **34 M** | **48 M** | **+14 M** |
| Threads | 30 | 32 | +2 |
| fds | 62 | 70 | +8 |
| Children | 0 | 0 | — |
| Idle CPU (60 s) | 0.15% | **0.15%** | unchanged |

The +14 M private dirty is compositor state, the IPC socket, and two threads.

**Correction:** stage 1's idle CPU was recorded as 0.00% from a 30 s sample.
CPU ticks have 10 ms granularity and a cheap poll can be missed entirely in a
30 s window. Over 60 s it is **0.15%** — about 12 `hyprctl` spawns per minute
from the poll loop. That is the number to compare against from here.

## Stage 3 — five-surface registry (2026-10-12)

| | Stage 2 | Stage 3 |
|---|---|---|
| Private dirty | 48 M | **40 M** |
| PSS | 92 M | 104 M |
| RSS | 115 M | 128 M |
| Threads | 32 | 33 |
| fds | 70 | 118 |
| Children | 0 | 0 |
| Idle CPU (60 s) | 0.15% | **0.10%** |

Private dirty went *down* with three more surfaces. That is allocator variance,
not a saving — do not read a trend into one sample.

fds nearly doubled: each layer surface brings a `wl_surface` and its buffers.

### Inset proof

```
$ hyprctl monitors -j | jq '.[0].reserved'
[0, 43, 0, 0]
```

Left, right and bottom are zero. The hidden dock and both sidebars reserve
nothing, which is the whole point of the zone-0 + `Some(&[])` hide mechanism.

Note the 43 is Ryoku's frame-edge, which was running at the same time. Two
shells reserving the same edge is not negotiable — one wins. Do not run both
when judging layout.

## Method

```sh
lume &
sleep 10
pid=$(pgrep -x lume)
hz=$(getconf CLK_TCK)

# RSS alone overcounts shared pages. Quote all three.
awk '/^Rss:|^Pss:|^Private_Dirty:/{printf "%s %s kB\n", $1, $2}' /proc/$pid/smaps_rollup
echo "Threads  $(awk '/Threads/{print $2}' /proc/$pid/status)"
echo "fds      $(ls /proc/$pid/fd | wc -l)"
echo "Children $(pgrep -P $pid | wc -l)"

# Sample for at least 60 s. CPU ticks have 10 ms granularity and a short
# window can report 0.00% for a process that is actually working.
a=$(awk '{print $14+$15}' /proc/$pid/stat); sleep 60
b=$(awk '{print $14+$15}' /proc/$pid/stat)
awk -v t=$((b-a)) -v h=$hz 'BEGIN{printf "Idle CPU %.3f%%\n",(t/h)/60*100}'
```

## Leak checks

```sh
before=$(ls /proc/$pid/fd | wc -l); sleep 300
after=$(ls /proc/$pid/fd | wc -l); echo "$before -> $after"   # must be equal

for i in $(seq 1 10); do printf '%s ' "$(awk '/VmRSS/{print $2}' /proc/$pid/status)"; sleep 180; done; echo
```

A sawtooth is a cache filling to a cap and resetting — fine. A monotonic climb
is a leak.

## Build notes

`/tmp` here is a 7.7 GB tmpfs and a cold gpui build needs about 6 GB. Always:

```sh
export CARGO_TARGET_DIR=~/.cache/lume-target
```

Cold build: ~6.5 min. Incremental: seconds.

## Reference

Ryoku's shell measured the same way: 140 MB RSS, 15 threads, 73 fds, 0.00% idle
CPU, 10 mapped layer surfaces, stable after 1 d 6 h uptime.