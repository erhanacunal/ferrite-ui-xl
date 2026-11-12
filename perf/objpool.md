# ObjPool performance measurements

## Isolated lookup benchmark

Reproduce on the host with:

```powershell
cargo run --release --target x86_64-pc-windows-msvc -p ferrite-core --features host --example objpool_bench
```

The benchmark runs seven rounds of one million indexed reads for 96, 512, and 2,048 widgets. It compares `ObjPool::get(id)`, `is_live(id) + get(id)`, `Vec<Box<Widget>>`, and contiguous `Vec<Widget>`. Construction and allocation are outside the timed region. These are cache-resident x86-64 results, not Exynos cycle counts.

Across two runs, `ObjPool::get` took 1.9–2.0 ns/read for 96 and 512 widgets, and 2.13–2.14 ns/read for 2,048. `Vec<Box<Widget>>` took 1.80–1.85 ns/read; contiguous `Vec<Widget>` took 1.79–1.88 ns/read. Thus this hot lookup was about 5–17% slower in the pool. Adding an explicit `is_live` check raised the cost to 2.8–3.12 ns/read (about 54–70% over boxed lookup).

The pool stores a 32-byte `Widget` in a 40-byte arena block (8-byte header included). At the default capacity, `ObjPool::new()` reserves about 2,179,072 bytes total for the arena, tables, and side state, regardless of whether 96 or 2,048 widgets are live. The boxed baseline's 40 bytes/widget includes only its 8-byte pointer slot and 32-byte object; allocator metadata and fragmentation are excluded. It is not a whole-pool memory comparison.

## Exynos Monitor stress

Five 2-second device profiler windows were collected with CPU2 render enabled and Monitor stress active. They averaged 2,034 passes/window (about 1,017/s). Existing profiler phases 17/18 measured scene capture and scene handoff:

| Measurement | Per pass | Per 2-second window |
| --- | ---: | ---: |
| Scene capture | 3.8 µs | 7.7 ms |
| Scene handoff | 1.8 µs | 3.6 ms |
| Capture + handoff | 5.6 µs | 11.3 ms |
| Render phase (CPU0) | 6.2 µs | 12.6 ms |
| G2D/GPU work | 106 µs | 215 ms |

Capture plus handoff was about 0.57% of the 985 µs average pass interval. These phases include scene copying, resource/message synchronization, and render-core submission; they are not pure `ObjPool` lookup time, and not all of them would disappear in a pointer-based design.

The compositor's cumulative status at uptime 1,225 s reported 6,135,337 µs in its tree collection over 52,717 compositor frames, about 116 µs/frame. That collection includes geometry, cache, and primitive-building work alongside `ObjPool` reads, so treat it as an inclusive tree-walk cost rather than pool-only time.

## Interpretation

The pool's isolated read path is modestly slower than direct boxed-pointer access on this host; the measured render pipeline does not support expecting a large whole-app speedup from removing `ObjPool`. The design also provides stable compact IDs, stale-handle behavior, reuse, and contiguous arena storage. A pointer conversion would need equivalent lifetime/handle checks and should be benchmarked as a complete renderer/runtime change before considering it.

Reproducible benchmark source: [`ferrite-core/examples/objpool_bench.rs`](../ferrite-core/examples/objpool_bench.rs).
