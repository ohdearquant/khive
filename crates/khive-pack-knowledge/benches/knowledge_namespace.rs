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
    namespace::run(if smoke { 1 } else { 21 }, full);
    if smoke {
        println!("Success");
    }
}
