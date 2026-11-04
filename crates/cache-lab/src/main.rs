use bfs_ng_cache_lab::simulator::{replay_all, report_csv};
use bfs_ng_cache_lab::trace::{Pattern, generate, load, save};
use std::env;
use std::error::Error;
use std::fs;

const HELP: &str = r#"BFS-NG Cache Lab (offline research simulator)

USAGE
  bfs-cache-lab generate --output PATH [--pattern mixed|hotset|scan|phase] [--events 50000] [--seed 42]
  bfs-cache-lab replay   --input PATH --capacity-bytes N [--policies none,lru,s3fifo,wtinylfu] [--output PATH]
  bfs-cache-lab sweep    --input PATH --capacities N,N,N [--policies none,lru,s3fifo,wtinylfu] [--output PATH]
  bfs-cache-lab demo     [--events 50000] [--seed 42] [--capacity-bytes 1048576]

OPTIONS
  --max-events N  Limit trace ingestion (default 1000000).

NOTES
  Counts demand accesses; speculative trace events populate cache but are
  excluded from demand hit rate. Refill costs are EXOGENOUS MODEL ESTIMATES,
  not real I/O latency. Outputs CSV with all-policy and per-class rows.
  Identical keys must have identical charged sizes and kinds.
"#;

fn get<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|v| v[0] == flag)
        .map(|v| v[1].as_str())
}
fn must<'a>(args: &'a [String], flag: &str) -> Result<&'a str, String> {
    get(args, flag).ok_or_else(|| format!("missing required {flag}\n\n{HELP}"))
}
fn number(args: &[String], flag: &str, default: u64) -> Result<u64, String> {
    match get(args, flag) {
        Some(value) => value
            .parse::<u64>()
            .map_err(|e| format!("invalid {flag}={value}: {e}")),
        None => Ok(default),
    }
}
fn capacities(s: &str) -> Result<Vec<u64>, String> {
    let values: Vec<u64> = s
        .split(',')
        .map(|v| {
            v.parse::<u64>()
                .map_err(|e| format!("invalid capacity '{v}': {e}"))
        })
        .collect::<Result<_, _>>()?;
    if values.is_empty() || values.len() > 128 {
        return Err("expected 1..=128 capacities".into());
    }
    Ok(values)
}
fn names(args: &[String]) -> Vec<String> {
    get(args, "--policies")
        .unwrap_or("none,lru,s3fifo,wtinylfu")
        .split(',')
        .map(|s| s.to_string())
        .collect()
}
fn output(args: &[String], results: &str) -> Result<(), Box<dyn Error>> {
    if let Some(path) = get(args, "--output") {
        fs::write(path, results)?;
        eprintln!("wrote {path}");
    } else {
        print!("{results}");
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "help" {
        println!("{HELP}");
        return Ok(());
    }
    match args[0].as_str() {
        "generate" => {
            let path = must(&args, "--output")?;
            let pattern = Pattern::parse(get(&args, "--pattern").unwrap_or("mixed"))?;
            let count = number(&args, "--events", 50_000)?;
            if count > 10_000_000 {
                return Err("--events max is 10000000".into());
            }
            let trace = generate(pattern, count as usize, number(&args, "--seed", 42)?);
            save(path, &trace)?;
            eprintln!("wrote {} events to {path}", trace.len());
        }
        "demo" => {
            let count = number(&args, "--events", 50_000)?;
            if count > 10_000_000 {
                return Err("--events max is 10000000".into());
            }
            let trace = generate(Pattern::Mixed, count as usize, number(&args, "--seed", 42)?);
            let cap = number(&args, "--capacity-bytes", 1_048_576)?;
            let results = replay_all(&trace, &names(&args), &[cap])?;
            output(&args, &report_csv(&results))?;
        }
        "replay" | "sweep" => {
            let path = must(&args, "--input")?;
            let max = number(&args, "--max-events", 1_000_000)?;
            if max > 10_000_000 {
                return Err("--max-events max is 10000000".into());
            }
            let trace = load(path, max as usize)?;
            let caps = if args[0] == "replay" {
                vec![must(&args, "--capacity-bytes")?.parse::<u64>()?]
            } else {
                capacities(must(&args, "--capacities")?)?
            };
            let results = replay_all(&trace, &names(&args), &caps)?;
            output(&args, &report_csv(&results))?;
        }
        other => return Err(format!("unknown command '{other}'\n\n{HELP}").into()),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(2);
    }
}
