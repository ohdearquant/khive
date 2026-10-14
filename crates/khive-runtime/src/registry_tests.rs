use super::*;
use khive_fold::objective::objective_fn;

#[test]
fn register_and_get() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let old = registry.register("max", Box::new(obj));
    assert!(old.is_none());
    assert!(registry.contains("max"));
    assert!(!registry.contains("min"));
}

#[test]
fn register_overwrites() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| -(*n as f64));
    assert!(registry.register("test", Box::new(obj1)).is_none());
    assert!(registry.register("test", Box::new(obj2)).is_some());

    let candidates = vec![1, 5, 3];
    let selection = registry
        .select("test", &candidates, &ObjectiveContext::new())
        .unwrap();
    assert_eq!(*selection.item, 1);
}

#[test]
fn select_by_name() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    registry.register("max", Box::new(obj));

    let candidates = vec![1, 5, 3];
    let selection = registry
        .select("max", &candidates, &ObjectiveContext::new())
        .unwrap();
    assert_eq!(*selection.item, 5);
}

#[test]
fn default_objective() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    registry.register("max", Box::new(obj));
    registry.set_default("max").unwrap();

    let candidates = vec![1, 5, 3];
    let selection = registry
        .select_default(&candidates, &ObjectiveContext::new())
        .unwrap();
    assert_eq!(*selection.item, 5);
}

#[test]
fn list_objectives_sorted() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| -(*n as f64));
    let obj3 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| (*n as f64).abs());
    registry.register("zebra", Box::new(obj1));
    registry.register("alpha", Box::new(obj2));
    registry.register("middle", Box::new(obj3));

    let names = registry.list();
    assert_eq!(names, vec!["alpha", "middle", "zebra"]);
}

#[test]
fn get_nonexistent_returns_error() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let result = registry.get("nope");
    assert!(matches!(result, Err(ObjectiveError::NotFound(ref s)) if s == "nope"));
}

#[test]
fn get_default_without_setting_returns_error() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let result = registry.get_default();
    assert!(matches!(result, Err(ObjectiveError::NotFound(_))));
}

#[test]
fn set_default_nonexistent_returns_error() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let result = registry.set_default("ghost");
    assert!(matches!(result, Err(ObjectiveError::NotFound(ref s)) if s == "ghost"));
}

#[test]
fn score_via_registry() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64 * 2.0);
    registry.register("double", Box::new(obj));

    let score = registry
        .score("double", &5, &ObjectiveContext::new())
        .unwrap();
    assert!((score - 10.0).abs() < 1e-12);
}

#[test]
fn select_default_via_registry() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| -(*n as f64));
    registry.register("min", Box::new(obj));
    registry.set_default("min").unwrap();

    let candidates = vec![1, 5, 3];
    let selection = registry
        .select_default(&candidates, &ObjectiveContext::new())
        .unwrap();
    assert_eq!(*selection.item, 1);
}

#[test]
fn debug_impls() {
    let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
    let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    registry.register("test", Box::new(obj));
    let debug = format!("{:?}", registry);
    assert!(debug.contains("ObjectiveRegistry"));
    assert!(debug.contains("count: 1"));

    let registered = registry.get("test").unwrap();
    let debug = format!("{:?}", registered);
    assert!(debug.contains("RegisteredObjective"));
    assert!(debug.contains("test"));
}

#[test]
fn concurrent_read_write() {
    let registry = Arc::new(ObjectiveRegistry::<i32>::new());

    std::thread::scope(|s| {
        for i in 0..8 {
            let reg = Arc::clone(&registry);
            s.spawn(move || {
                let name = format!("obj_{i}");
                let obj =
                    objective_fn(move |n: &i32, _ctx: &ObjectiveContext| *n as f64 + i as f64);
                reg.register(name.clone(), Box::new(obj));

                assert!(reg.contains(&name));

                let candidates = vec![1, 2, 3];
                let _ = reg.select(&name, &candidates, &ObjectiveContext::new());
            });
        }
    });

    assert_eq!(registry.list().len(), 8);
}
