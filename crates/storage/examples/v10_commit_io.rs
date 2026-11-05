//! Count physical block reads and writes per small commit as an image fills.
//!
//! Compares the experimental v10 writer (`UnifiedV10`, default and indexed
//! formats) with the v9 `FileSystem` on the same in-memory `CrashDevice`.
//! Block counts are the meaningful output. Wall-clock time is reported for
//! orientation only: it is dominated by the `CrashDevice` durability model
//! and is not a storage latency measurement.
//!
//! ```sh
//! cargo run --release -p bfs-ng-storage --example v10_commit_io -- v10 10,100,300
//! cargo run --release -p bfs-ng-storage --example v10_commit_io -- v10-indexed 10,100,300
//! cargo run --release -p bfs-ng-storage --example v10_commit_io -- v9 10,100,300
//! ```
use bfs_ng_storage::experimental_v10::UnifiedV10;
use bfs_ng_storage::{BLOCK_SIZE, BlockDevice, CrashDevice, FileSystem, Result};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

const IMAGE_BLOCKS: usize = 16_384;
const REPS: u64 = 5;
const PAYLOAD: [u8; 1000] = [0xab; 1000];

/// Shared read/write counters, readable while a filesystem owns the device.
#[derive(Clone, Default)]
struct Counters {
    reads: Rc<Cell<u64>>,
    writes: Rc<Cell<u64>>,
}

impl Counters {
    fn snapshot(&self) -> (u64, u64) {
        (self.reads.get(), self.writes.get())
    }
}

/// Wraps a device and counts block reads and writes.
struct Counting {
    inner: CrashDevice,
    counters: Counters,
}

impl Counting {
    fn new(counters: &Counters) -> Self {
        Self {
            inner: CrashDevice::new(IMAGE_BLOCKS),
            counters: counters.clone(),
        }
    }
}

impl BlockDevice for Counting {
    fn block_count(&self) -> u64 {
        self.inner.block_count()
    }

    fn read_block(&mut self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<()> {
        self.counters.reads.set(self.counters.reads.get() + 1);
        self.inner.read_block(block, out)
    }

    fn write_block(&mut self, block: u64, data: &[u8; BLOCK_SIZE]) -> Result<()> {
        self.counters.writes.set(self.counters.writes.get() + 1);
        self.inner.write_block(block, data)
    }

    fn sync(&mut self) -> Result<()> {
        self.inner.sync()
    }
}

struct Row {
    files: usize,
    open_reads: u64,
    reads_per_op: u64,
    writes_per_op: u64,
    micros_per_op: u64,
}

fn header(title: &str) {
    println!("\n{title}");
    println!(
        "{:>6} {:>11} {:>11} {:>11} {:>11}",
        "files", "open reads", "reads/op", "writes/op", "us/op"
    );
}

fn print(row: &Row) {
    println!(
        "{:>6} {:>11} {:>11} {:>11} {:>11}",
        row.files, row.open_reads, row.reads_per_op, row.writes_per_op, row.micros_per_op
    );
}

fn path(index: usize) -> String {
    format!("/d/f{index:05}")
}

fn run_v10(indexed: bool, sizes: &[usize]) -> Result<()> {
    let counters = Counters::default();
    let device = Counting::new(&counters);
    let mut fs = if indexed {
        UnifiedV10::format_indexed(device, 7)?
    } else {
        UnifiedV10::format(device, 7)?
    };
    fs.mkdir("/d")?;
    header(if indexed {
        "v10 UnifiedV10::format_indexed"
    } else {
        "v10 UnifiedV10::format"
    });
    let mut files = 0;
    for &target in sizes {
        while files < target {
            fs.write_at(&path(files), 0, &PAYLOAD)?;
            files += 1;
        }
        let (before_open, _) = counters.snapshot();
        fs = UnifiedV10::open(fs.into_device())?;
        let (after_open, after_open_writes) = counters.snapshot();
        let started = Instant::now();
        for i in 0..REPS {
            fs.write_at(&path(1), (i % 4) * 10, b"x")?;
        }
        let elapsed = started.elapsed();
        let (reads, writes) = counters.snapshot();
        print(&Row {
            files,
            open_reads: after_open - before_open,
            reads_per_op: (reads - after_open) / REPS,
            writes_per_op: (writes - after_open_writes) / REPS,
            micros_per_op: elapsed.as_micros() as u64 / REPS,
        });
    }
    Ok(())
}

fn run_v9(sizes: &[usize]) -> Result<()> {
    let counters = Counters::default();
    let mut fs = FileSystem::format(Counting::new(&counters), 9)?;
    let mut tx = fs.begin();
    tx.mkdir("/d")?;
    fs.commit(tx)?;
    header("v9 FileSystem (each op rewrites the whole 1000-byte file)");
    let mut files = 0;
    for &target in sizes {
        while files < target {
            let mut tx = fs.begin();
            tx.write_file(&path(files), &PAYLOAD)?;
            fs.commit(tx)?;
            files += 1;
        }
        let (before_open, _) = counters.snapshot();
        fs = FileSystem::open(fs.into_device())?;
        let (after_open, after_open_writes) = counters.snapshot();
        let started = Instant::now();
        for i in 0..REPS {
            let mut tx = fs.begin();
            tx.write_file(&path(1), &[i as u8; 1000])?;
            fs.commit(tx)?;
        }
        let elapsed = started.elapsed();
        let (reads, writes) = counters.snapshot();
        print(&Row {
            files,
            open_reads: after_open - before_open,
            reads_per_op: (reads - after_open) / REPS,
            writes_per_op: (writes - after_open_writes) / REPS,
            micros_per_op: elapsed.as_micros() as u64 / REPS,
        });
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map_or("v10", String::as_str);
    let sizes: Vec<usize> = args
        .get(2)
        .map_or("10,100,300", String::as_str)
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    match mode {
        "v9" => run_v9(&sizes),
        "v10-indexed" => run_v10(true, &sizes),
        _ => run_v10(false, &sizes),
    }
}
