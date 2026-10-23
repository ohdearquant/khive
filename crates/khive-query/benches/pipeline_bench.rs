//! SQL compilation, AST validation, and larger query inputs; no database I/O.

mod pipeline_support;

use std::cell::OnceCell;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use khive_query::{
    compile, parse, validate, validate_with_warnings, CompileOptions, QueryLanguage,
};
use pipeline_support::fixtures::{self, GQL};
use pipeline_support::scene;

fn pipeline(c: &mut Criterion) {
    let opts = CompileOptions {
        scopes: vec![fixtures::SCOPE.into()],
        max_limit: 500,
    };
    let mut group = c.benchmark_group("query_pipeline");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for &(name, text) in GQL {
        let parsed = OnceCell::new();
        let input = || {
            parsed.get_or_init(|| {
                parse(QueryLanguage::Gql, text).expect("legacy GQL fixture must parse")
            })
        };
        scene(
            &mut group,
            &format!("compile/{name}"),
            input,
            |parsed| compile(black_box(*parsed), black_box(&opts)),
            |parsed, compiled| fixtures::assert_compiled(compiled, parsed),
        );
        scene(
            &mut group,
            &format!("validate/{name}"),
            || input().clone(),
            validate,
            |normalized, _| fixtures::assert_shape(normalized, input()),
        );
        scene(
            &mut group,
            &format!("validate_with_warnings/{name}"),
            || input().clone(),
            validate_with_warnings,
            |normalized, warnings| {
                fixtures::assert_shape(normalized, input());
                assert!(warnings.is_empty());
            },
        );
    }

    let large = fixtures::large_gql();
    scene(
        &mut group,
        "large/parse_10_nodes_20_properties",
        || (),
        |_| parse(QueryLanguage::Gql, black_box(&large)),
        |_, parsed| fixtures::assert_large(parsed),
    );
    scene(
        &mut group,
        "large/parse_compile_10_nodes_20_properties",
        || (),
        |_| {
            let parsed = parse(QueryLanguage::Gql, black_box(&large))?;
            let compiled = compile(black_box(&parsed), black_box(&opts))?;
            Ok((parsed, compiled))
        },
        |_, (parsed, compiled)| {
            fixtures::assert_large(parsed);
            fixtures::assert_large_compiled(compiled, parsed);
        },
    );
    scene(
        &mut group,
        "sparql_complex/four_edges_six_triples_bounded_paths",
        || (),
        |_| parse(QueryLanguage::Sparql, black_box(fixtures::COMPLEX_SPARQL)),
        |_, parsed| fixtures::assert_sparql(parsed),
    );
    group.finish();
}

criterion_group!(benches, pipeline);
criterion_main!(benches);
