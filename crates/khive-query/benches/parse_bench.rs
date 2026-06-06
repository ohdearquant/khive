use criterion::measurement::WallTime;
use criterion::{criterion_group, criterion_main, BenchmarkGroup, Criterion};
use khive_query::{parse, parse_auto, QueryLanguage};

fn bench_gql_simple(g: &mut BenchmarkGroup<WallTime>) {
    let input = "MATCH (n:concept) RETURN n";
    g.bench_function("gql/simple_node", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input = "MATCH (a:concept)-[e:extends]->(b:concept) RETURN a, b";
    g.bench_function("gql/two_node_edge", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input = "MATCH (n:document) RETURN n LIMIT 20";
    g.bench_function("gql/node_with_limit", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });
}

fn bench_gql_medium(g: &mut BenchmarkGroup<WallTime>) {
    let input =
        "MATCH (a:concept)-[e:extends]->(b:project) WHERE b.name = 'lattice-inference' RETURN a LIMIT 10";
    g.bench_function("gql/where_eq_string", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input =
        "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' AND b.kind = 'concept' RETURN a, b";
    g.bench_function("gql/where_and", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input =
        "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN a";
    g.bench_function("gql/where_or", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input =
        "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'X' AND a.kind = 'concept' OR b.kind = 'project' RETURN a";
    g.bench_function("gql/where_and_or", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input =
        "MATCH (a)-[e:implements]->(b:project) WHERE b.name = 'khive' RETURN a, e, b LIMIT 50";
    g.bench_function("gql/where_with_edge_var", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input = "MATCH (n:document {entity_type: 'paper'}) RETURN n LIMIT 5";
    g.bench_function("gql/node_with_properties", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });
}

fn bench_gql_complex(g: &mut BenchmarkGroup<WallTime>) {
    let input =
        "MATCH (a:concept)-[:introduced_by]->(p:paper)-[:introduced_by]->(c:concept) RETURN a, c";
    g.bench_function("gql/three_node_chain", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input = "MATCH (a {name: 'LoRA'})-[:extends|variant_of*1..3]->(b) RETURN b LIMIT 20";
    g.bench_function("gql/variable_length_multi_rel", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input =
        "MATCH (a:concept)-[:extends*1..5]->(b:concept) WHERE a.name = 'FlashAttention' RETURN b LIMIT 100";
    g.bench_function("gql/variable_length_with_where", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    let input =
        "MATCH (a:person)<-[e:introduced_by]-(c:concept)-[:extends]->(b:concept) RETURN a, c, b LIMIT 10";
    g.bench_function("gql/three_node_mixed_direction", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    // Property map with multiple keys
    let input =
        "MATCH (n:concept {name: 'LoRA', entity_type: 'algorithm'})-[:extends]->(b) RETURN b";
    g.bench_function("gql/node_multi_property_map", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });

    // Undirected edge
    let input = "MATCH (a:concept)-[e:competes_with]-(b:concept) RETURN a, b";
    g.bench_function("gql/undirected_edge", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)).unwrap())
    });
}

fn bench_sparql_simple(g: &mut BenchmarkGroup<WallTime>) {
    let input = "SELECT ?a ?b WHERE { ?a a :concept . ?a :extends ?b . } LIMIT 10";
    g.bench_function("sparql/two_node", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)).unwrap())
    });

    let input = "SELECT ?b WHERE { ?a :name 'LoRA' . ?a :extends+ ?b . }";
    g.bench_function("sparql/variable_length_plus", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)).unwrap())
    });

    let input = "SELECT ?a ?b WHERE { ?a :extends{1,3} ?b . }";
    g.bench_function("sparql/explicit_range", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)).unwrap())
    });
}

fn bench_sparql_medium(g: &mut BenchmarkGroup<WallTime>) {
    let input = "SELECT ?a ?c WHERE { ?a :extends ?b . ?b :introduced_by ?c . ?c a :paper . }";
    g.bench_function("sparql/three_node_chain", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)).unwrap())
    });

    let input = "SELECT ?a WHERE { ?a a :concept . ?a :domain 'attention' . ?a :extends+ ?b . }";
    g.bench_function("sparql/with_property_filter", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)).unwrap())
    });

    let input =
        "SELECT ?a ?b WHERE { ?a a :concept . ?a :name 'FlashAttention' . ?a :extends ?b . } LIMIT 5";
    g.bench_function("sparql/kind_and_property_filter", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)).unwrap())
    });
}

fn bench_parse_auto(g: &mut BenchmarkGroup<WallTime>) {
    let gql_input = "MATCH (a:concept)-[e:extends]->(b) RETURN a LIMIT 10";
    g.bench_function("auto/gql_dispatch", |b| {
        b.iter(|| parse_auto(criterion::black_box(gql_input)).unwrap())
    });

    let sparql_input = "SELECT ?a ?b WHERE { ?a a :concept . ?a :extends ?b . }";
    g.bench_function("auto/sparql_dispatch", |b| {
        b.iter(|| parse_auto(criterion::black_box(sparql_input)).unwrap())
    });

    // parse_auto with leading whitespace — exercises the trim + prefix check
    let padded_gql = "  MATCH (n:concept) RETURN n";
    g.bench_function("auto/gql_with_leading_whitespace", |b| {
        b.iter(|| parse_auto(criterion::black_box(padded_gql)).unwrap())
    });

    let padded_sparql = "  SELECT ?a WHERE { ?a :extends ?b . }";
    g.bench_function("auto/sparql_with_leading_whitespace", |b| {
        b.iter(|| parse_auto(criterion::black_box(padded_sparql)).unwrap())
    });
}

fn gql_benchmarks(c: &mut Criterion) {
    let mut g = c.benchmark_group("gql");
    g.sample_size(200);
    bench_gql_simple(&mut g);
    g.finish();

    let mut g = c.benchmark_group("gql_medium");
    g.sample_size(200);
    bench_gql_medium(&mut g);
    g.finish();

    let mut g = c.benchmark_group("gql_complex");
    g.sample_size(100);
    bench_gql_complex(&mut g);
    g.finish();
}

fn sparql_benchmarks(c: &mut Criterion) {
    let mut g = c.benchmark_group("sparql");
    g.sample_size(200);
    bench_sparql_simple(&mut g);
    g.finish();

    let mut g = c.benchmark_group("sparql_medium");
    g.sample_size(100);
    bench_sparql_medium(&mut g);
    g.finish();
}

fn auto_detect_benchmarks(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse_auto");
    g.sample_size(200);
    bench_parse_auto(&mut g);
    g.finish();
}

criterion_group!(gql_benches, gql_benchmarks);
criterion_group!(sparql_benches, sparql_benchmarks);
criterion_group!(auto_benches, auto_detect_benchmarks);
criterion_main!(gql_benches, sparql_benches, auto_benches);
