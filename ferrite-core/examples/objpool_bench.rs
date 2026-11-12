//! Compare the hot read path of ObjPool with stable boxed widgets and a
//! contiguous Vec. Run with:
//! `cargo run --release -p ferrite-core --example objpool_bench`
//!
//! This isolates handle-to-widget access; it deliberately does not claim to
//! benchmark the renderer, pool mutation, or allocator fragmentation.

use std::hint::black_box;
use std::time::{Duration, Instant};

use ferrite_core::pool::{ObjPool, T_WIDGET};
use ferrite_core::types::{Offset, Size};
use ferrite_core::widget::{Widget, WidgetId};

const LOOKUPS: usize = 1_000_000;
const ROUNDS: usize = 7;

fn elapsed_per_lookup(elapsed: Duration, lookups: usize) -> f64 {
    elapsed.as_nanos() as f64 / lookups as f64
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn run(label: &str, n: usize, mut read: impl FnMut(usize) -> i16) -> f64 {
    let mut samples = Vec::with_capacity(ROUNDS);
    let mut sum = 0i64;
    for _ in 0..ROUNDS {
        let start = Instant::now();
        for i in 0..LOOKUPS {
            // A deterministic, non-sequential access pattern over the widgets.
            let index = (i.wrapping_mul(65_537).wrapping_add(17)) % n;
            sum = sum.wrapping_add(black_box(read(index)) as i64);
        }
        samples.push(elapsed_per_lookup(start.elapsed(), LOOKUPS));
    }
    black_box(sum);
    let ns = median(samples);
    println!("  {label:<20} {ns:>8.2} ns/lookup");
    ns
}

fn main() {
    println!("ObjPool hot read path: median of {ROUNDS} × {LOOKUPS} indexed reads");
    println!("Times are host CPU nanoseconds; setup and allocation are excluded.\n");

    for n in [96usize, 512, 2048] {
        let mut pool = ObjPool::new();
        let mut ids = Vec::<WidgetId>::with_capacity(n);
        for i in 0..n {
            let id = pool.alloc_widget().expect("ObjPool capacity");
            let w = pool.get_mut(id);
            w.location = Offset {
                x: i as i16,
                y: (i as i16).wrapping_mul(3),
            };
            w.size = Size { w: 24, h: 36 };
            ids.push(id);
        }
        // A shallow tree with Monitor-like branching; construction is not timed.
        for i in 1..n {
            pool.add_child(ids[(i - 1) / 8], ids[i]);
        }
        let boxed: Vec<Box<Widget>> = ids.iter().map(|&id| Box::new(*pool.get(id))).collect();
        let contiguous: Vec<Widget> = ids.iter().map(|&id| *pool.get(id)).collect();
        println!(
            "{n} widgets ({} B/widget payload):",
            std::mem::size_of::<Widget>()
        );
        println!(
            "  pool arena={} B, pool heap={} B; boxed minimum={} B ({} B slots + {} B payload; allocator overhead excluded)",
            pool.bytes_of(T_WIDGET),
            pool.heap_bytes(),
            n * (std::mem::size_of::<usize>() + std::mem::size_of::<Widget>()),
            n * std::mem::size_of::<usize>(),
            n * std::mem::size_of::<Widget>(),
        );

        let pool_ns = run("ObjPool get(id)", n, |i| pool.get(ids[i]).location.x);
        let live_ns = run("is_live + get", n, |i| {
            let id = ids[i];
            if pool.is_live(id) {
                pool.get(id).location.x
            } else {
                0
            }
        });
        let boxed_ns = run("Vec<Box<Widget>>", n, |i| boxed[i].location.x);
        let dense_ns = run("Vec<Widget>", n, |i| contiguous[i].location.x);
        println!(
            "  relative to boxed: ObjPool {:.2}×, is_live+get {:.2}×; relative to dense: ObjPool {:.2}×\n",
            pool_ns / boxed_ns,
            live_ns / boxed_ns,
            pool_ns / dense_ns,
        );
    }
}
