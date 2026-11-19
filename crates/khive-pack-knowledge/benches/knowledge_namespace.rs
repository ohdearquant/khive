// This target uses only namespace setup from the fixture shared with the workload/index benches.
#[allow(dead_code)]
#[path = "knowledge_support/fixture.rs"]
mod fixture;
#[path = "knowledge_support/namespace.rs"]
mod namespace;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--list") {
        println!("knowledge_namespace/eligibility_fallback_sql: benchmark");
        println!("knowledge_namespace/full_suggest: benchmark");
        println!("knowledge_namespace/full_compose_store_size: benchmark (opt-in)");
        return;
    }
    let smoke = !args.iter().any(|arg| arg == "--bench") || args.iter().any(|arg| arg == "--test");
    let full = !smoke && std::env::var("KHIVE_BENCH_COMPOSE_STORE_SIZE").as_deref() == Ok("1");
    let store_atoms = full.then(|| {
        match std::env::var("KHIVE_BENCH_COMPOSE_LOCAL_ATOMS") {
            Ok(value) => value.parse::<usize>().ok().filter(|count| *count > 0),
            Err(std::env::VarError::NotPresent) => Some(180_474),
            Err(std::env::VarError::NotUnicode(_)) => None,
        }
        .unwrap_or_else(|| {
            eprintln!("KHIVE_BENCH_COMPOSE_LOCAL_ATOMS must be a positive integer");
            std::process::exit(2);
        })
    });
    namespace::run(if smoke { 1 } else { 21 }, store_atoms);
    if smoke {
        println!("Success");
    }
}
