use criterion::measurement::WallTime;
use criterion::{criterion_group, criterion_main, BenchmarkGroup, Criterion};
use khive_query::{parse, parse_auto, QueryLanguage};

fn bench_gql_simple(g: &mut BenchmarkGroup<WallTime>) {
    let input = "MATCH (n:concept) RETURN n";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/simple_node", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input = "MATCH (a:concept)-[e:extends]->(b:concept) RETURN a, b";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/two_node_edge", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input = "MATCH (n:document) RETURN n LIMIT 20";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/node_with_limit", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });
}

fn bench_gql_medium(g: &mut BenchmarkGroup<WallTime>) {
    let input =
        "MATCH (a:concept)-[e:extends]->(b:project) WHERE b.name = 'lattice-inference' RETURN a LIMIT 10";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/where_eq_string", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input =
        "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' AND b.kind = 'concept' RETURN a, b";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/where_and", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input =
        "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN a";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/where_or", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input =
        "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'X' AND a.kind = 'concept' OR b.kind = 'project' RETURN a";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/where_and_or", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input =
        "MATCH (a)-[e:implements]->(b:project) WHERE b.name = 'khive' RETURN a, e, b LIMIT 50";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/where_with_edge_var", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input = "MATCH (n:document {entity_type: 'paper'}) RETURN n LIMIT 5";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/node_with_properties", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });
}

fn bench_gql_complex(g: &mut BenchmarkGroup<WallTime>) {
    let input =
        "MATCH (a:concept)-[:introduced_by]->(p:paper)-[:introduced_by]->(c:concept) RETURN a, c";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/three_node_chain", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input = "MATCH (a {name: 'LoRA'})-[:extends|variant_of*1..3]->(b) RETURN b LIMIT 20";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/variable_length_multi_rel", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input =
        "MATCH (a:concept)-[:extends*1..5]->(b:concept) WHERE a.name = 'FlashAttention' RETURN b LIMIT 100";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/variable_length_with_where", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input =
        "MATCH (a:person)<-[e:introduced_by]-(c:concept)-[:extends]->(b:concept) RETURN a, c, b LIMIT 10";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/three_node_mixed_direction", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input =
        "MATCH (n:concept {name: 'LoRA', entity_type: 'algorithm'})-[:extends]->(b) RETURN b";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/node_multi_property_map", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });

    let input = "MATCH (a:concept)-[e:competes_with]-(b:concept) RETURN a, b";
    parse(QueryLanguage::Gql, input).expect("fixture must parse");
    g.bench_function("gql/undirected_edge", |b| {
        b.iter(|| parse(QueryLanguage::Gql, criterion::black_box(input)))
    });
}

fn bench_sparql_simple(g: &mut BenchmarkGroup<WallTime>) {
    let input = "SELECT ?a ?b WHERE { ?a a :concept . ?a :extends ?b . } LIMIT 10";
    parse(QueryLanguage::Sparql, input).expect("fixture must parse");
    g.bench_function("sparql/two_node", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)))
    });

    let input = "SELECT ?b WHERE { ?a :name 'LoRA' . ?a :extends+ ?b . }";
    parse(QueryLanguage::Sparql, input).expect("fixture must parse");
    g.bench_function("sparql/variable_length_plus", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)))
    });

    let input = "SELECT ?a ?b WHERE { ?a :extends{1,3} ?b . }";
    parse(QueryLanguage::Sparql, input).expect("fixture must parse");
    g.bench_function("sparql/explicit_range", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)))
    });
}

fn bench_sparql_medium(g: &mut BenchmarkGroup<WallTime>) {
    let input = "SELECT ?a ?c WHERE { ?a :extends ?b . ?b :introduced_by ?c . ?c a :paper . }";
    parse(QueryLanguage::Sparql, input).expect("fixture must parse");
    g.bench_function("sparql/three_node_chain", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)))
    });

    let input = "SELECT ?a WHERE { ?a a :concept . ?a :domain 'attention' . ?a :extends+ ?b . }";
    parse(QueryLanguage::Sparql, input).expect("fixture must parse");
    g.bench_function("sparql/with_property_filter", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)))
    });

    let input =
        "SELECT ?a ?b WHERE { ?a a :concept . ?a :name 'FlashAttention' . ?a :extends ?b . } LIMIT 5";
    parse(QueryLanguage::Sparql, input).expect("fixture must parse");
    g.bench_function("sparql/kind_and_property_filter", |b| {
        b.iter(|| parse(QueryLanguage::Sparql, criterion::black_box(input)))
    });
}

fn bench_parse_auto(g: &mut BenchmarkGroup<WallTime>) {
    let gql_input = "MATCH (a:concept)-[e:extends]->(b) RETURN a LIMIT 10";
    parse_auto(gql_input).expect("fixture must parse");
    g.bench_function("auto/gql_dispatch", |b| {
        b.iter(|| parse_auto(criterion::black_box(gql_input)))
    });

    let sparql_input = "SELECT ?a ?b WHERE { ?a a :concept . ?a :extends ?b . }";
    parse_auto(sparql_input).expect("fixture must parse");
    g.bench_function("auto/sparql_dispatch", |b| {
        b.iter(|| parse_auto(criterion::black_box(sparql_input)))
    });

    let padded_gql = "  MATCH (n:concept) RETURN n";
    parse_auto(padded_gql).expect("fixture must parse");
    g.bench_function("auto/gql_with_leading_whitespace", |b| {
        b.iter(|| parse_auto(criterion::black_box(padded_gql)))
    });

    let padded_sparql = "  SELECT ?a WHERE { ?a :extends ?b . }";
    parse_auto(padded_sparql).expect("fixture must parse");
    g.bench_function("auto/sparql_with_leading_whitespace", |b| {
        b.iter(|| parse_auto(criterion::black_box(padded_sparql)))
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
